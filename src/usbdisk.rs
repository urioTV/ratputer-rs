//! USB Mass Storage device backed by the physical SD card.
//!
//! The USB transport is entirely Rust: `esp-hal` and `embassy-usb` drive the
//! ESP32-S3 DWC2 peripheral, while `crate::msc` implements BOT and SCSI. The
//! backend is installed only while the FAT volume manager is absent, so the
//! host and firmware can never access the card concurrently.

use alloc::{boxed::Box, rc::Rc};
use core::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

use embassy_futures::join::join;
use embedded_sdmmc::BlockDevice;
use esp_hal::time::{Duration, Instant};
use esp_hal::usb::otg::{embassy_usb_device, Usb};

use crate::msc::{MscClass, SharedState, Stats};

/// Longest time one `poll()` call may keep driving USB before returning to the UI.
const POLL_BUDGET: Duration = Duration::from_millis(40);

/// Set by the USB-OTG interrupt (through embassy's wakers) whenever the task
/// can make progress. There is no executor, so `poll()` spins on this flag.
static USB_WOKEN: AtomicBool = AtomicBool::new(true);

static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake, waker_drop);

fn waker_clone(_: *const ()) -> RawWaker {
    RawWaker::new(core::ptr::null(), &WAKER_VTABLE)
}

fn waker_wake(_: *const ()) {
    USB_WOKEN.store(true, Ordering::Release);
}

fn waker_drop(_: *const ()) {}

fn flag_waker() -> Waker {
    // SAFETY: the vtable ignores the data pointer and only touches a static flag.
    unsafe { Waker::from_raw(waker_clone(core::ptr::null())) }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsbDiskState {
    Inactive,
    Waiting,
    Mounted,
    Ejected,
}

type UsbTask = Pin<Box<dyn Future<Output = ()> + 'static>>;

pub struct UsbDisk {
    usb: Option<Usb<'static>>,
    task: Option<UsbTask>,
    shared: Rc<RefCell<SharedState>>,
    active: bool,
    init_attempted: bool,
}

impl UsbDisk {
    pub fn new(usb: Usb<'static>) -> Self {
        // Keep the default USB-Serial-JTAG path intact until the user actually
        // opens USB DISK. This preserves espflash monitor logs during boot.
        Self {
            usb: Some(usb),
            task: None,
            shared: Rc::new(RefCell::new(SharedState::default())),
            active: false,
            init_attempted: false,
        }
    }

    pub fn available(&self) -> bool {
        !self.init_attempted || self.task.is_some()
    }

    fn select_otg_phy(&self) {
        // ESP32-S3's USB-OTG and USB-Serial-JTAG controllers share one PHY.
        esp_hal::peripherals::LPWR::regs()
            .usb_conf()
            .modify(|_, w| w.sw_hw_usb_phy_sel().set_bit().sw_usb_phy_sel().set_bit());
        esp_hal::peripherals::USB_WRAP::regs()
            .otg_conf()
            .modify(|_, w| {
                w.usb_pad_enable()
                    .set_bit()
                    .phy_sel()
                    .clear_bit()
                    .clk_en()
                    .set_bit()
                    .ahb_clk_force_on()
                    .set_bit()
                    .phy_clk_force_on()
                    .set_bit()
                    .pad_pull_override()
                    .clear_bit()
            });
    }

    fn restore_serial_jtag_phy(&self) {
        esp_hal::peripherals::LPWR::regs()
            .usb_conf()
            .modify(|_, w| w.sw_hw_usb_phy_sel().set_bit().sw_usb_phy_sel().clear_bit());
    }

    fn initialize(&mut self) -> bool {
        if self.task.is_some() {
            return true;
        }
        self.init_attempted = true;
        let Some(usb) = self.usb.take() else {
            return false;
        };

        // One OUT packet can be pending on EP0 and another on the MSC bulk OUT
        // endpoint. Extra space keeps the Synopsys driver layout future-proof.
        let endpoint_buffer = Box::leak(Box::new([0_u8; 1024]));
        let driver = embassy_usb_device::Driver::new(
            usb,
            endpoint_buffer,
            embassy_usb_device::Config::default(),
        );

        let mut config = embassy_usb::Config::new(0xcafe, 0x4002);
        config.manufacturer = Some("RATPUTER");
        config.product = Some("RATPUTER SD");
        config.serial_number = Some("RATPUTER-ADV");
        config.max_power = 500;
        // A single-function device: let Windows bind usbstor directly instead
        // of going through the composite (IAD) driver.
        config.device_class = 0x00;
        config.device_sub_class = 0x00;
        config.device_protocol = 0x00;
        config.composite_with_iads = false;

        let config_descriptor = Box::leak(Box::new([0_u8; 256]));
        let bos_descriptor = Box::leak(Box::new([0_u8; 64]));
        let msos_descriptor = Box::leak(Box::new([0_u8; 64]));
        let control_buffer = Box::leak(Box::new([0_u8; 64]));
        let mut builder = embassy_usb::Builder::new(
            driver,
            config,
            config_descriptor,
            bos_descriptor,
            msos_descriptor,
            control_buffer,
        );
        let msc = MscClass::new(&mut builder, self.shared.clone());
        let mut device = builder.build();

        self.task = Some(Box::pin(async move {
            let _ = join(device.run(), msc.run()).await;
        }));
        true
    }

    /// Attach an exclusively owned block device and enumerate it on USB.
    /// The caller must keep `device` pinned at the same address until `detach`.
    pub fn attach<D: BlockDevice>(&mut self, device: &D) -> bool {
        self.select_otg_phy();
        if !self.initialize() {
            self.restore_serial_jtag_phy();
            return false;
        }
        self.shared.borrow_mut().attach(device);
        self.active = true;
        true
    }

    pub fn poll(&mut self) {
        if !self.active {
            return;
        }
        let Some(task) = self.task.as_mut() else {
            return;
        };
        // A bulk transfer advances one 64-byte packet per wake-up. Keep polling
        // while the interrupt reports progress, bounded so the UI stays live.
        let waker = flag_waker();
        let mut context = Context::from_waker(&waker);
        let start = Instant::now();
        loop {
            USB_WOKEN.store(false, Ordering::Release);
            if let Poll::Ready(()) = task.as_mut().poll(&mut context) {
                self.active = false;
                return;
            }
            // Wait briefly for the next packet before yielding to the main loop.
            while !USB_WOKEN.load(Ordering::Acquire) {
                if start.elapsed() >= POLL_BUDGET {
                    return;
                }
            }
        }
    }

    pub fn state(&self) -> UsbDiskState {
        if !self.active {
            return UsbDiskState::Inactive;
        }
        let shared = self.shared.borrow();
        if shared.ejected() {
            UsbDiskState::Ejected
        } else if shared.configured() {
            UsbDiskState::Mounted
        } else if shared.attached() {
            UsbDiskState::Waiting
        } else {
            UsbDiskState::Inactive
        }
    }

    pub fn stats(&self) -> Stats {
        self.shared.borrow().stats()
    }

    pub fn can_detach(&self) -> bool {
        if !self.active {
            return true;
        }
        let shared = self.shared.borrow();
        !shared.configured() || shared.ejected()
    }

    /// Remove the backend before its owner moves, then return the shared PHY to
    /// USB-Serial-JTAG. The Rust DWC2 task is retained for the next attachment.
    pub fn detach(&mut self) {
        if self.active {
            self.shared.borrow_mut().detach();
            self.active = false;
            self.restore_serial_jtag_phy();
        }
    }
}
