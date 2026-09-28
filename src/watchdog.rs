//! RTC watchdog: reset the chip when the single main loop stops turning.
//!
//! The firmware is one thread with no executor. One future or driver call that
//! never returns freezes rendering, the keyboard and the USB console at the
//! same moment, and the only recovery used to be pulling the battery (seen
//! during a file-manager copy). The RTC watchdog lives in the RTC power domain
//! and keeps counting with interrupts disabled, so it resets the chip even when
//! the application is completely wedged.
//!
//! Feeding happens exactly once per main-loop iteration, so "alive" means "the
//! previous iteration finished". Operations that are *meant* to block - the
//! manual radio steps, which sit inside `block_on` while a scan pass (8 s cap)
//! or an association attempt (12 s cap) runs - widen the window through
//! [`Watchdog::slow_window`] so a slow access point is not mistaken for a hang.
//!
//! Deliberately no interrupt: stage 0 resets the system directly. Warning about
//! it first would need an interrupt handler plus a critical section, and the
//! cause of the previous reset is reported at boot and through `STATUS` anyway.
//!
//! The reset cause alone is not enough. After any reset the USB Serial/JTAG
//! port re-enumerates, and a terminal reopening it toggles the control lines,
//! which resets the chip a second time (`usb_uart`) and hides the real cause.
//! A small reset history therefore lives in RTC fast memory that esp-hal zeroes
//! only on power-on: the cause of the boot before this one, and a count of
//! watchdog resets since the battery was last switched on.

use esp_hal::peripherals::RTC_TIMER;
use esp_hal::ram;
use esp_hal::rtc_cntl::{reset_reason, Rtc, RwdtStage, SocResetReason};
use esp_hal::system::Cpu;
use esp_hal::time::{Duration, Instant};

/// Time the main loop may take between two iterations.
pub const NORMAL_WINDOW: Duration = Duration::from_secs(15);

/// Window granted around an operation that is expected to block. It has to
/// exceed one manual radio step: a scan pass is capped at 8 s, an association
/// attempt at 12 s plus a 3 s disconnect.
pub const SLOW_WINDOW: Duration = Duration::from_secs(25);

/// Marks the history below as written by this firmware. Anything else in the
/// words (for example after flashing a different image) is discarded.
const HISTORY_MAGIC: u32 = 0x5241_5457; // "RATW"

// Retained across every reset except power-on (esp-hal zeroes the persistent
// section only for `ChipPowerOn`). NOLOAD: the initialisers are never applied.
#[ram(unstable(rtc_fast, persistent))]
static mut HISTORY_MAGIC_WORD: u32 = 0;
#[ram(unstable(rtc_fast, persistent))]
static mut PREVIOUS_CAUSE: u32 = 0;
#[ram(unstable(rtc_fast, persistent))]
static mut WATCHDOG_RESETS: u32 = 0;
// Last window programmed before the reset, with the raw stage-0 hold value and
// the RTC slow-clock calibration used to compute it: tells which window was
// really active when a reset fired.
#[ram(unstable(rtc_fast, persistent))]
static mut LIVE_WINDOW_S: u32 = 0;
#[ram(unstable(rtc_fast, persistent))]
static mut LIVE_HOLD: u32 = 0;
#[ram(unstable(rtc_fast, persistent))]
static mut LIVE_CAL: u32 = 0;

/// RTC_CNTL (ESP32-S3): stage-0 hold (`WDTCONFIG1`) and the slow-clock
/// calibration word (`STORE1`) that esp-hal converts timeouts with.
const RTC_CNTL_WDTCONFIG1: *const u32 = 0x6000_809C as *const u32;
const RTC_CNTL_STORE1: *const u32 = 0x6000_8054 as *const u32;

/// Hardware values behind the current window, read back from the registers.
#[derive(Clone, Copy, Default)]
pub struct WindowSnapshot {
    pub window_s: u32,
    pub hold: u32,
    pub cal: u32,
}

fn read_hardware() -> (u32, u32) {
    // SAFETY: plain reads of always-readable RTC_CNTL registers.
    unsafe {
        (
            RTC_CNTL_WDTCONFIG1.read_volatile(),
            RTC_CNTL_STORE1.read_volatile(),
        )
    }
}

fn read_retained(word: *const u32) -> u32 {
    // SAFETY: private statics, touched only from `record_boot` during
    // single-threaded start-up; volatile because the value predates this boot.
    unsafe { word.read_volatile() }
}

fn write_retained(word: *mut u32, value: u32) {
    // SAFETY: see `read_retained`.
    unsafe { word.write_volatile(value) }
}

/// Update the retained history with this boot's cause. Returns the cause of
/// the previous boot and the watchdog reset count including this boot.
fn record_boot(cause: Option<SocResetReason>) -> (Option<SocResetReason>, u32) {
    let magic = core::ptr::addr_of_mut!(HISTORY_MAGIC_WORD);
    let previous = core::ptr::addr_of_mut!(PREVIOUS_CAUSE);
    let resets = core::ptr::addr_of_mut!(WATCHDOG_RESETS);

    if read_retained(magic) != HISTORY_MAGIC {
        write_retained(previous, 0);
        write_retained(resets, 0);
        write_retained(core::ptr::addr_of_mut!(LIVE_WINDOW_S), 0);
        write_retained(core::ptr::addr_of_mut!(LIVE_HOLD), 0);
        write_retained(core::ptr::addr_of_mut!(LIVE_CAL), 0);
        write_retained(magic, HISTORY_MAGIC);
    }

    let previous_cause = SocResetReason::from_repr(read_retained(previous) as usize);
    write_retained(previous, cause.map_or(0, |cause| cause as u32));

    let mut count = read_retained(resets);
    if is_watchdog(cause) {
        count = count.saturating_add(1);
        write_retained(resets, count);
    }
    (previous_cause, count)
}

fn is_watchdog(cause: Option<SocResetReason>) -> bool {
    matches!(
        cause,
        Some(SocResetReason::CoreRtcWdt)
            | Some(SocResetReason::CpuRtcWdt)
            | Some(SocResetReason::SysRtcWdt)
    )
}

/// Short ASCII token for a reset cause, for logs and `STATUS`.
fn cause_name(cause: Option<SocResetReason>) -> &'static str {
    match cause {
        None => "unknown",
        Some(SocResetReason::ChipPowerOn) => "power_on",
        Some(SocResetReason::CoreSw) | Some(SocResetReason::CpuSw) => "software",
        _ if is_watchdog(cause) => "watchdog",
        Some(SocResetReason::CoreMwdt0)
        | Some(SocResetReason::CoreMwdt1)
        | Some(SocResetReason::CpuMwdt0)
        | Some(SocResetReason::CpuMwdt1) => "timer_watchdog",
        Some(SocResetReason::SysSuperWdt) => "super_watchdog",
        Some(SocResetReason::SysBrownOut) => "brownout",
        Some(SocResetReason::CoreDeepSleep) => "deep_sleep",
        Some(SocResetReason::SysClkGlitch) => "clk_glitch",
        Some(SocResetReason::CoreEfuseCrc) => "efuse_crc",
        Some(SocResetReason::CoreUsbUart) => "usb_uart",
        Some(SocResetReason::CoreUsbJtag) => "usb_jtag",
        Some(SocResetReason::CorePwrGlitch) => "pwr_glitch",
        _ => "other",
    }
}

pub struct Watchdog<'d> {
    rtc: Rtc<'d>,
    /// Window currently programmed into stage 0.
    window: Duration,
    armed: bool,
    /// Millis-since-boot of the last [`Watchdog::feed`], kept as an integer so
    /// the gap calculation can never underflow.
    last_feed_ms: u64,
    slowest_iteration_ms: u64,
    boot_cause: Option<SocResetReason>,
    previous_cause: Option<SocResetReason>,
    watchdog_resets: u32,
    /// Window that was active when the previous boot ended.
    previous_window: WindowSnapshot,
}

impl<'d> Watchdog<'d> {
    /// Takes the RTC timer peripheral (nobody else uses it) and reads why the
    /// chip started. The watchdog stays off until [`Self::arm`], because boot
    /// does slow work (SD identification at 400 kHz, the first renders) before
    /// the loop exists.
    pub fn new(rtc_timer: RTC_TIMER<'d>) -> Self {
        let mut rtc = Rtc::new(rtc_timer);
        rtc.rwdt.disable();
        let boot_cause = reset_reason(Cpu::ProCpu);
        let (previous_cause, watchdog_resets) = record_boot(boot_cause);
        let previous_window = WindowSnapshot {
            window_s: read_retained(core::ptr::addr_of!(LIVE_WINDOW_S)),
            hold: read_retained(core::ptr::addr_of!(LIVE_HOLD)),
            cal: read_retained(core::ptr::addr_of!(LIVE_CAL)),
        };

        Self {
            rtc,
            window: NORMAL_WINDOW,
            armed: false,
            last_feed_ms: Instant::now().duration_since_epoch().as_millis(),
            slowest_iteration_ms: 0,
            boot_cause,
            previous_cause,
            watchdog_resets,
            previous_window,
        }
    }

    /// Window, raw hold and calibration active when the previous boot ended.
    pub fn previous_window(&self) -> WindowSnapshot {
        self.previous_window
    }

    /// Window, raw hold and calibration programmed right now.
    pub fn current_window(&self) -> WindowSnapshot {
        let (hold, cal) = read_hardware();
        WindowSnapshot {
            window_s: self.window.as_secs() as u32,
            hold,
            cal,
        }
    }

    /// Cause of the reset that started this boot.
    pub fn boot_cause(&self) -> &'static str {
        cause_name(self.boot_cause)
    }

    /// Cause of the boot before this one; `none` right after power-on. Shows a
    /// watchdog reset even when reopening the terminal reset the chip again.
    pub fn previous_cause(&self) -> &'static str {
        match self.previous_cause {
            None => "none",
            cause => cause_name(cause),
        }
    }

    /// Watchdog resets since the last power-on, including this boot.
    pub fn watchdog_resets(&self) -> u32 {
        self.watchdog_resets
    }

    /// Starts counting with the normal window. Stage 0 resets the whole system;
    /// `enable` also turns stages 1-3 off, and nothing here re-enables them.
    pub fn arm(&mut self) {
        self.rtc.rwdt.enable();
        self.program(NORMAL_WINDOW);
        self.armed = true;
        log::info!(
            "RTC watchdog armed: reset after {} s without a loop pass",
            NORMAL_WINDOW.as_secs()
        );
    }

    /// Stops counting. Recovery tool only - a hung device then needs a power
    /// cycle, which is exactly what the watchdog exists to avoid.
    pub fn disarm(&mut self) {
        self.rtc.rwdt.disable();
        self.armed = false;
        log::warn!("RTC watchdog disabled");
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }

    pub fn window_secs(&self) -> u64 {
        self.window.as_secs()
    }

    /// Longest gap between two iterations since boot: tells a slow-but-alive
    /// loop (a radio step) apart from a loop that stopped.
    pub fn slowest_iteration_ms(&self) -> u64 {
        self.slowest_iteration_ms
    }

    /// Record one completed iteration. `now` is the loop's own timestamp, so
    /// this adds no extra clock read.
    pub fn feed(&mut self, now: Instant) {
        let ms = now.duration_since_epoch().as_millis();
        let gap = ms.saturating_sub(self.last_feed_ms);
        if gap > self.slowest_iteration_ms {
            self.slowest_iteration_ms = gap;
        }
        self.last_feed_ms = ms;

        if self.armed {
            self.rtc.rwdt.feed();
        }
    }

    /// Widen the window for as long as the returned guard is alive.
    pub fn slow_window(&mut self) -> SlowWindow<'_, 'd> {
        if self.armed {
            self.program(SLOW_WINDOW);
        }
        SlowWindow { watchdog: self }
    }

    /// Program a window and restart the counter. The feed matters: writing only
    /// the hold register leaves the counter running, so restoring the shorter
    /// window after a long operation would otherwise expire immediately.
    /// `last_feed_ms` is deliberately left alone: this hardware feed is not a
    /// loop pass, and resetting it would hide slow steps from `loop_max_ms`.
    fn program(&mut self, window: Duration) {
        self.rtc.rwdt.set_timeout(RwdtStage::Stage0, window);
        self.rtc.rwdt.feed();
        self.window = window;
        let (hold, cal) = read_hardware();
        write_retained(
            core::ptr::addr_of_mut!(LIVE_WINDOW_S),
            window.as_secs() as u32,
        );
        write_retained(core::ptr::addr_of_mut!(LIVE_HOLD), hold);
        write_retained(core::ptr::addr_of_mut!(LIVE_CAL), cal);
    }
}

/// Restores the normal window when the slow operation it guards is over.
pub struct SlowWindow<'a, 'd> {
    watchdog: &'a mut Watchdog<'d>,
}

impl Drop for SlowWindow<'_, '_> {
    fn drop(&mut self) {
        if self.watchdog.armed {
            self.watchdog.program(NORMAL_WINDOW);
        }
    }
}
