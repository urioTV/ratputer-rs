//! USB Mass Storage device backed by the physical SD card.
//!
//! TinyUSB owns the USB-OTG controller while this module provides sector I/O
//! callbacks into an `embedded_sdmmc::BlockDevice`. The backend pointer is only
//! installed while the FAT filesystem manager is absent, so the host and the
//! firmware can never access the card concurrently.

use core::{ffi::c_void, ptr, slice};

use embedded_sdmmc::{Block, BlockDevice, BlockIdx};
use esp_hal::usb::otg::Usb;

type CountFn = unsafe fn(*const ()) -> u32;
type ReadFn = unsafe fn(*const (), u32, u32, *mut u8, u32) -> i32;
type WriteFn = unsafe fn(*const (), u32, u32, *const u8, u32) -> i32;

#[derive(Clone, Copy)]
struct Backend {
    context: *const (),
    count: CountFn,
    read: ReadFn,
    write: WriteFn,
}

unsafe impl Sync for Backend {}

unsafe fn no_count(_: *const ()) -> u32 {
    0
}
unsafe fn no_read(_: *const (), _: u32, _: u32, _: *mut u8, _: u32) -> i32 {
    -1
}
unsafe fn no_write(_: *const (), _: u32, _: u32, _: *const u8, _: u32) -> i32 {
    -1
}

static mut BACKEND: Backend = Backend {
    context: ptr::null(),
    count: no_count,
    read: no_read,
    write: no_write,
};

unsafe extern "C" {
    fn ratputer_tinyusb_init() -> bool;
    fn ratputer_tinyusb_poll();
    fn ratputer_tinyusb_connect();
    fn ratputer_tinyusb_disconnect();
    fn ratputer_tinyusb_state() -> u8;
    fn ratputer_tinyusb_can_disconnect() -> bool;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsbDiskState {
    Inactive,
    Waiting,
    Mounted,
    Ejected,
}

/// Owns USB_FS and keeps esp-hal's peripheral clock guard alive.
pub struct UsbDisk {
    _usb: Usb<'static>,
    initialized: bool,
    init_attempted: bool,
}

impl UsbDisk {
    pub fn new(usb: Usb<'static>) -> Self {
        // Keep the default USB-Serial-JTAG path intact until the user actually
        // opens USB DISK. This preserves espflash monitor logs during boot.
        Self {
            _usb: usb,
            initialized: false,
            init_attempted: false,
        }
    }

    pub fn available(&self) -> bool {
        !self.init_attempted || self.initialized
    }

    fn select_otg_phy(&self) {
        // Reproduce esp-hal's USB-OTG device-mode platform setup. TinyUSB then
        // initializes the Synopsys DWC2 core itself.
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
        // ESP32-S3's two USB controllers share one PHY. Software selection 0
        // returns it to the ROM USB-Serial-JTAG controller used by espflash.
        esp_hal::peripherals::LPWR::regs()
            .usb_conf()
            .modify(|_, w| w.sw_hw_usb_phy_sel().set_bit().sw_usb_phy_sel().clear_bit());
    }

    /// Attach an exclusively owned block device and enumerate it on USB.
    /// The caller must keep `device` pinned at the same address until `detach`.
    pub fn attach<D: BlockDevice>(&mut self, device: &D) -> bool {
        self.select_otg_phy();
        if !self.initialized {
            self.init_attempted = true;
            self.initialized = unsafe { ratputer_tinyusb_init() };
        }
        if !self.initialized {
            self.restore_serial_jtag_phy();
            return false;
        }
        unsafe {
            BACKEND = Backend {
                context: (device as *const D).cast(),
                count: block_count::<D>,
                read: read_blocks::<D>,
                write: write_blocks::<D>,
            };
            ratputer_tinyusb_connect();
        }
        true
    }

    pub fn poll(&mut self) {
        if self.initialized {
            unsafe { ratputer_tinyusb_poll() };
        }
    }

    pub fn state(&self) -> UsbDiskState {
        if !self.initialized {
            return UsbDiskState::Inactive;
        }
        match unsafe { ratputer_tinyusb_state() } {
            1 => UsbDiskState::Waiting,
            2 => UsbDiskState::Mounted,
            3 => UsbDiskState::Ejected,
            _ => UsbDiskState::Inactive,
        }
    }

    pub fn can_detach(&self) -> bool {
        !self.initialized || unsafe { ratputer_tinyusb_can_disconnect() }
    }

    /// Disconnect first, then clear the backend pointer before its owner moves.
    pub fn detach(&mut self) {
        if self.initialized {
            unsafe {
                ratputer_tinyusb_disconnect();
                BACKEND = Backend {
                    context: ptr::null(),
                    count: no_count,
                    read: no_read,
                    write: no_write,
                };
            }
            self.restore_serial_jtag_phy();
        }
    }
}

unsafe fn block_count<D: BlockDevice>(context: *const ()) -> u32 {
    let device = &*context.cast::<D>();
    device.num_blocks().map(|count| count.0).unwrap_or(0)
}

unsafe fn read_blocks<D: BlockDevice>(
    context: *const (),
    lba: u32,
    offset: u32,
    output: *mut u8,
    length: u32,
) -> i32 {
    let device = &*context.cast::<D>();
    let output = slice::from_raw_parts_mut(output, length as usize);
    transfer_read(device, lba, offset as usize, output)
        .then_some(length as i32)
        .unwrap_or(-1)
}

unsafe fn write_blocks<D: BlockDevice>(
    context: *const (),
    lba: u32,
    offset: u32,
    input: *const u8,
    length: u32,
) -> i32 {
    let device = &*context.cast::<D>();
    let input = slice::from_raw_parts(input, length as usize);
    transfer_write(device, lba, offset as usize, input)
        .then_some(length as i32)
        .unwrap_or(-1)
}

fn transfer_read<D: BlockDevice>(
    device: &D,
    mut lba: u32,
    mut offset: usize,
    mut out: &mut [u8],
) -> bool {
    if offset >= Block::LEN {
        return false;
    }
    while !out.is_empty() {
        let mut block = [Block::new()];
        if device.read(&mut block, BlockIdx(lba)).is_err() {
            return false;
        }
        let count = out.len().min(Block::LEN - offset);
        out[..count].copy_from_slice(&block[0].contents[offset..offset + count]);
        out = &mut out[count..];
        lba += 1;
        offset = 0;
    }
    true
}

fn transfer_write<D: BlockDevice>(
    device: &D,
    mut lba: u32,
    mut offset: usize,
    mut input: &[u8],
) -> bool {
    if offset >= Block::LEN {
        return false;
    }
    while !input.is_empty() {
        let count = input.len().min(Block::LEN - offset);
        let mut block = [Block::new()];
        // TinyUSB normally submits whole 512-byte chunks. Read-modify-write
        // keeps partial transfers correct as required by the callback contract.
        if (offset != 0 || count != Block::LEN) && device.read(&mut block, BlockIdx(lba)).is_err() {
            return false;
        }
        block[0].contents[offset..offset + count].copy_from_slice(&input[..count]);
        if device.write(&block, BlockIdx(lba)).is_err() {
            return false;
        }
        input = &input[count..];
        lba += 1;
        offset = 0;
    }
    true
}

#[no_mangle]
pub extern "C" fn ratputer_sd_block_count() -> u32 {
    unsafe { (BACKEND.count)(BACKEND.context) }
}

#[no_mangle]
pub unsafe extern "C" fn ratputer_sd_read(
    lba: u32,
    offset: u32,
    buffer: *mut c_void,
    length: u32,
) -> i32 {
    if buffer.is_null() {
        return -1;
    }
    (BACKEND.read)(BACKEND.context, lba, offset, buffer.cast(), length)
}

#[no_mangle]
pub unsafe extern "C" fn ratputer_sd_write(
    lba: u32,
    offset: u32,
    buffer: *const c_void,
    length: u32,
) -> i32 {
    if buffer.is_null() {
        return -1;
    }
    (BACKEND.write)(BACKEND.context, lba, offset, buffer.cast(), length)
}
