#![no_std]
#![no_main]

// RATPUTER — Slint (no_std, software renderer) on M5Stack Cardputer ADV (ESP32-S3).
//
// - build.rs compiles ui/ratputer.slint into Rust (see slint::include_modules!()),
// - MinimalSoftwareWindow + render_by_line push each line through mipidsi to the
//   ST7789V2 LCD (SPI @ 40 MHz — LCD pins go through the GPIO matrix, not IOMUX),
// - Slint needs an allocator: esp-alloc heap in internal SRAM (the ADV has no PSRAM).

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use embedded_graphics::pixelcolor::raw::RawU16;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_hal_bus::spi::ExclusiveDevice;

use esp_backtrace as _; // panic handler + backtrace to UART
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::{Duration, Instant, Rate};
use esp_hal::timer::timg::TimerGroup;
use esp_println as _;

mod battery;
mod clock;
mod debug;
mod filemanager;
mod fspath;
mod ftp;
mod msc;
mod net;
mod sdblock;
mod ssh;
mod storage;
mod usbdisk;
mod watchdog;
mod wifi;

use cardputer_adv_keyboard::{Arrow, KeyInput, Keyboard};
use storage::{SavedNetwork, StorageError, WifiConfig};

use mipidsi::interface::SpiInterface;
use mipidsi::models::ST7789;
use mipidsi::options::{ColorInversion, Orientation, Rotation};
use mipidsi::Builder;

use slint::platform::software_renderer::{LineBufferProvider, MinimalSoftwareWindow, Rgb565Pixel};
use slint::{ModelRc, SharedString, VecModel};

// ESP-IDF app descriptor (required by espflash save-image --merge)
esp_bootloader_esp_idf::esp_app_desc!();

// Modules generated from ui/ratputer.slint by build.rs
slint::include_modules!();

// Heap in internal SRAM — ADV = Stamp-S3A (ESP32-S3FN8) has no PSRAM.
// 512 KB SRAM total; MUST be called in main() before the first Box/Rc (Slint needs alloc).
const HEAP_SIZE: usize = 150 * 1024;
// Second heap region in the RAM the ESP-IDF 2nd-stage bootloader ran from
// (dram2_seg, 0x3FCDB700..0x3FCED710 = 72 KiB). The application never used it:
// the stack ends exactly where it starts. Without it the Wi-Fi views left
// ~19 KiB of fragmented heap and the renderer's scene Vec failed a 7 KiB
// allocation during a scan (OOM panic).
const RECLAIMED_HEAP_SIZE: usize = 64 * 1024;

const LCD_WIDTH: usize = 240;
const LCD_HEIGHT: usize = 135;
const FRAME_MS: u64 = 250;
// After this delay the splash screen transitions to the firmware UI
// (600 ms crossfade declared in .slint states).
const SPLASH_AFTER_MS: u64 = 3500;
// Top bar refresh: battery sampling period and SNTP resync / retry intervals.
const BATTERY_EVERY_MS: u64 = 5000;
const SNTP_RESYNC_SECS: u64 = 3600;
const SNTP_RETRY_SECS: u64 = 30;
/// `WDT STALL`: longer than the ordinary window, shorter than the slow one.
const WDT_STALL_SECS: u64 = 20;
const AUTO_CONNECT_AFTER_SPLASH_MS: u64 = 500;
const AUTO_NEXT_NETWORK_SECS: u64 = 5;
const AUTO_RETRY_CYCLE_SECS: u64 = 60;
const AUTO_RECONNECT_AFTER_DROP_SECS: u64 = 5;
const MAX_AUTO_PASSES: u8 = 3;
// Glyph budget of the top-bar SSID field (120 px / 8 px per glyph).
const TOP_BAR_SSID_CHARS: usize = 15;

fn retry_saved_network(index: &mut usize, pass: &mut u8, due: &mut Option<Instant>, count: usize) {
    if count == 0 {
        *due = None;
    } else if *index > 0 {
        *index -= 1;
        *due = Some(Instant::now() + Duration::from_secs(AUTO_NEXT_NETWORK_SECS));
    } else if *pass < MAX_AUTO_PASSES {
        *pass += 1;
        *index = count - 1;
        *due = Some(Instant::now() + Duration::from_secs(AUTO_RETRY_CYCLE_SECS));
    } else {
        // Do not keep retrying forever when the access point is absent.
        *due = None;
    }
}

// ---------------------------------------------------------------------------
// Slint backend for esp-hal (single core, no scheduler)
// ---------------------------------------------------------------------------
struct EspBackend {
    window: Rc<MinimalSoftwareWindow>,
    boot_micros: u64,
}

impl slint::platform::Platform for EspBackend {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> core::time::Duration {
        let now = Instant::now().duration_since_epoch().as_micros();
        core::time::Duration::from_micros(now - self.boot_micros)
    }
}

// ---------------------------------------------------------------------------
// Bridge: a line rendered by Slint -> a pixel burst to the ST7789 via mipidsi
// ---------------------------------------------------------------------------
struct HardwareDrawBuffer<'a, Display> {
    display: &'a mut Display,
    buffer: &'a mut [Rgb565Pixel],
}

impl<'a, Display> HardwareDrawBuffer<'a, Display> {
    fn new(display: &'a mut Display, buffer: &'a mut [Rgb565Pixel]) -> Self {
        Self { display, buffer }
    }
}

impl<
        DI: mipidsi::interface::Interface<Word = u8>,
        RST: embedded_hal::digital::OutputPin<Error = core::convert::Infallible>,
    > LineBufferProvider
    for &mut HardwareDrawBuffer<'_, mipidsi::Display<DI, mipidsi::models::ST7789, RST>>
{
    type TargetPixel = Rgb565Pixel;

    fn process_line(
        &mut self,
        line: usize,
        range: core::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [Rgb565Pixel]),
    ) {
        let buf = &mut self.buffer[range.clone()];
        render_fn(buf);
        self.display
            .set_pixels(
                range.start as u16,
                line as u16,
                range.end as u16,
                line as u16,
                buf.iter().map(|x| RawU16::new(x.0).into()),
            )
            .unwrap();
    }
}

fn string_model(values: Vec<String>) -> ModelRc<SharedString> {
    let values = values
        .into_iter()
        .map(SharedString::from)
        .collect::<Vec<_>>();
    Rc::new(VecModel::from(values)).into()
}

fn saved_network_model(config: &WifiConfig) -> ModelRc<SharedString> {
    string_model(
        config
            .networks
            .iter()
            .map(|network| display_ssid(&network.ssid))
            .collect(),
    )
}

fn scan_network_model(networks: &[wifi::ScanNetwork]) -> ModelRc<SharedString> {
    string_model(
        networks
            .iter()
            .map(|network| {
                let security = if !network.supported {
                    "!"
                } else if network.auth == storage::AuthKind::Open {
                    " "
                } else {
                    "*"
                };
                format!(
                    "{} {} {}",
                    display_ssid(&network.ssid),
                    network.signal_strength,
                    security
                )
            })
            .collect(),
    )
}

fn display_ssid(ssid: &str) -> String {
    truncate_ascii(ssid, 18)
}

fn truncate_ascii(text: &str, max_chars: usize) -> String {
    text.chars()
        .take(max_chars)
        .map(|character| if character.is_ascii() { character } else { '?' })
        .collect()
}

fn set_password(ui: &MainWindow, password: &str) {
    // Password stays visible while typing — masked entry is impractical on this keyboard.
    ui.set_wifi_password(password.into());
}

fn view_name(view: i32) -> &'static str {
    match view {
        0 => "menu",
        1 => "rat",
        2 => "wifi_menu",
        3 => "wifi_saved",
        4 => "wifi_scan",
        5 => "wifi_password",
        6 => "about",
        7 => "usb_disk",
        8 => "ftp",
        9 => "ftp_password",
        10 => "sd_files",
        11 => "sd_actions",
        12 => "sd_name",
        13 => "sd_delete",
        _ => "unknown",
    }
}

fn apply_debug_key(ui: &MainWindow, key: debug::DebugKey) {
    match key {
        debug::DebugKey::Up => ui.invoke_key_pressed("up".into()),
        debug::DebugKey::Down => ui.invoke_key_pressed("down".into()),
        debug::DebugKey::Left => ui.invoke_key_pressed("left".into()),
        debug::DebugKey::Right => ui.invoke_key_pressed("right".into()),
        debug::DebugKey::Enter => ui.invoke_key_pressed("enter".into()),
        debug::DebugKey::Back => ui.invoke_key_pressed("back".into()),
        debug::DebugKey::Delete => ui.invoke_key_pressed("delete".into()),
        debug::DebugKey::Tab => ui.invoke_key_pressed("tab".into()),
        debug::DebugKey::Space => ui.invoke_key_pressed("space".into()),
        debug::DebugKey::Backspace if ui.get_view_state() == 5 => {
            let mut password = ui.get_wifi_password().to_string();
            password.pop();
            set_password(ui, &password);
        }
        debug::DebugKey::Backspace if ui.get_view_state() == 9 => {
            let mut password = ui.get_ftp_password().to_string();
            password.pop();
            ui.set_ftp_password(password.into());
        }
        debug::DebugKey::Backspace => ui.invoke_key_pressed("back".into()),
    }
}

fn file_debug_key(key: debug::DebugKey) -> filemanager::Key {
    use filemanager::Key;
    match key {
        debug::DebugKey::Up => Key::Up,
        debug::DebugKey::Down => Key::Down,
        debug::DebugKey::Left => Key::Left,
        debug::DebugKey::Right => Key::Right,
        debug::DebugKey::Enter => Key::Enter,
        debug::DebugKey::Back => Key::Back,
        debug::DebugKey::Backspace => Key::Backspace,
        debug::DebugKey::Delete => Key::Delete,
        debug::DebugKey::Tab => Key::Tab,
        debug::DebugKey::Space => Key::Char(' '),
    }
}

fn append_debug_text(ui: &MainWindow, text: &str) -> Result<(), &'static str> {
    match ui.get_view_state() {
        5 => {
            let mut password = ui.get_wifi_password().to_string();
            if password.len() + text.len() > 64 {
                return Err("wifi_text_too_long");
            }
            password.push_str(text);
            set_password(ui, &password);
            Ok(())
        }
        9 => {
            if !text.bytes().all(|byte| byte.is_ascii_graphic()) {
                return Err("ftp_text_disallows_spaces");
            }
            let mut password = ui.get_ftp_password().to_string();
            if password.len() + text.len() > 32 {
                return Err("ftp_text_too_long");
            }
            password.push_str(text);
            ui.set_ftp_password(password.into());
            Ok(())
        }
        _ => Err("text_entry_not_active"),
    }
}

/// Blocking radio work split into steps (one scan pass / one connect attempt),
/// so the UI keeps redrawing a spinner + progress between the steps.
enum RadioStep {
    Scan {
        pass: usize,
    },
    Connect {
        network: SavedNetwork,
        attempt: usize,
    },
}

// The future owns the controller while connecting, then returns it. This
// avoids self-referential borrows and lets the no-executor main loop poll it
// once per pass without blocking input, USB, networking or rendering.
struct AutoConnectJob {
    ssid: String,
    future:
        Pin<Box<dyn Future<Output = (wifi::WifiManager<'static>, Result<(), wifi::ConnectError>)>>>,
}

fn busy_status(step: &RadioStep, frame_idx: u32) -> String {
    let spinner = ["|", "/", "-", "+"][(frame_idx % 4) as usize];
    match *step {
        RadioStep::Scan { pass } => format!("SCANNING {pass}/{} {spinner}", wifi::SCAN_PASSES),
        RadioStep::Connect { attempt, .. } => {
            format!("CONNECTING {attempt}/{} {spinner}", wifi::CONNECT_ATTEMPTS)
        }
    }
}

/// Apply credentials and, on success, hand the connection to the step machine.
fn begin_connect(
    wifi: &mut Option<wifi::WifiManager>,
    ui: &MainWindow,
    network: SavedNetwork,
    frame_idx: u32,
) -> Option<RadioStep> {
    match wifi
        .as_mut()
        .map(|manager| manager.configure(&network.ssid, &network.password, network.auth))
    {
        Some(Ok(())) => {
            ui.set_wifi_selected_ssid(network.ssid.clone().into());
            let step = RadioStep::Connect {
                network,
                attempt: 1,
            };
            ui.set_wifi_status(busy_status(&step, frame_idx).into());
            ui.set_wifi_connecting(true);
            Some(step)
        }
        Some(Err(error)) => {
            log::error!("Wi-Fi configure failed: {error:?}");
            ui.set_wifi_status("CONNECTION FAILED".into());
            None
        }
        None => {
            ui.set_wifi_status("WI-FI NOT AVAILABLE".into());
            None
        }
    }
}

// ---------------------------------------------------------------------------
#[esp_hal::main]
fn main() -> ! {
    esp_alloc::heap_allocator!(size: HEAP_SIZE);
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: RECLAIMED_HEAP_SIZE);

    esp_println::logger::init_logger_from_env();
    log::info!("RATPUTER (Slint) start");

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    let mut delay = Delay::new();

    // The hardware USB Serial/JTAG CDC endpoint carries both normal logs and an
    // interactive command shell. USB MSC temporarily takes the shared PHY, so
    // console commands pause while the SD is exported.
    let mut debug_console = debug::DebugConsole::new(peripherals.USB_DEVICE);

    // Native USB-OTG uses the ESP32-S3's fixed D+=GPIO20 / D-=GPIO19 pins.
    let usb =
        esp_hal::usb::otg::Usb::new_fs(peripherals.USB_FS, peripherals.GPIO20, peripherals.GPIO19);
    let mut usb_disk = usbdisk::UsbDisk::new(usb);

    // --- LCD ST7789V2 on SPI2 (Cardputer ADV) ---
    let dc = Output::new(peripherals.GPIO34, Level::Low, OutputConfig::default());
    let cs = Output::new(peripherals.GPIO37, Level::High, OutputConfig::default());
    let rst = Output::new(peripherals.GPIO33, Level::High, OutputConfig::default());
    let mut backlight = Output::new(peripherals.GPIO38, Level::High, OutputConfig::default());

    // 40 MHz — the LCD pins are routed through the GPIO matrix (not IOMUX);
    // at 80 MHz ST7789 setup times are violated and the picture scrambles.
    // Reference: espp m5stack-cardputer.hpp (lcd_clock_speed = 40 MHz).
    let spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default().with_frequency(Rate::from_mhz(40)),
    )
    .unwrap()
    .with_sck(peripherals.GPIO36)
    .with_mosi(peripherals.GPIO35);

    let spi_dev = ExclusiveDevice::new(spi, cs, Delay::new()).unwrap();
    let mut buffer = [0u8; 512]; // mipidsi DCS transaction buffer
    let interface = SpiInterface::new(spi_dev, dc, &mut buffer);

    // display_size/display_offset are in the ST7789-native space (portrait 240x320 GRAM);
    // Deg90 rotation produces a 240x135 logical landscape.
    let mut display = Builder::new(ST7789, interface)
        .invert_colors(ColorInversion::Inverted)
        .orientation(Orientation::new().rotate(Rotation::Deg90))
        .display_size(135, 240)
        .display_offset(52, 40)
        .reset_pin(rst)
        .init(&mut delay)
        .unwrap();

    backlight.set_level(Level::High);
    display.clear(Rgb565::BLACK).unwrap();

    // --- Keyboard TCA8418 (Cardputer ADV): I2C0 @ 400 kHz, SDA=G8, SCL=G9 ---
    let i2c = I2c::new(
        peripherals.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(400)),
    )
    .unwrap()
    .with_sda(peripherals.GPIO8)
    .with_scl(peripherals.GPIO9);
    let mut keyboard = Keyboard::new(i2c).expect("TCA8418 keyboard initialization failed");

    // --- SD card on dedicated SPI3: SCLK=G40, MOSI=G14, MISO=G39, CS=G12 ---
    // SD identification MUST run at <=400 kHz. After it succeeds, switch to a
    // 20 MHz data clock, below the 25 MHz SD default-speed limit.
    let sd_cs = Output::new(peripherals.GPIO12, Level::High, OutputConfig::default());
    let sd_spi = Spi::new(
        peripherals.SPI3,
        SpiConfig::default().with_frequency(Rate::from_khz(400)),
    )
    .unwrap()
    .with_sck(peripherals.GPIO40)
    .with_mosi(peripherals.GPIO14)
    .with_miso(peripherals.GPIO39);
    let sd_device = ExclusiveDevice::new(sd_spi, sd_cs, Delay::new()).unwrap();
    let sd_card = embedded_sdmmc::SdCard::new(sd_device, Delay::new());
    if let Some(card_type) = sd_card.get_card_type() {
        let fast_config = SpiConfig::default().with_frequency(Rate::from_mhz(20));
        if sd_card
            .spi(|device| device.bus_mut().apply_config(&fast_config))
            .is_ok()
        {
            log::info!("SD initialized as {card_type:?}; SPI clock raised to 20 MHz");
        } else {
            log::warn!("SD initialized, but SPI remained at 400 kHz");
        }
    } else {
        log::warn!("SD initialization failed at 400 kHz");
    }
    // `None` means the raw card has been moved to USB MSC. Re-mounting after
    // detach re-parses the MBR/FAT because the host may have rewritten them.
    let mut storage = storage::mount(sd_card);
    let (mut wifi_config, storage_available) = match storage
        .as_ref()
        .ok_or(StorageError::Sd)
        .and_then(|volume| storage::load(volume))
    {
        Ok(config) => (config, true),
        Err(StorageError::Missing) => (WifiConfig::default(), true),
        Err(error) => {
            log::warn!("Unable to load Wi-Fi credentials: {error:?}");
            (WifiConfig::default(), false)
        }
    };

    // esp-radio relies on esp-rtos for its internal Wi-Fi tasks and timers.
    let timer_group = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timer_group.timer0, peripherals.FROM_CPU_INTR0);
    let (mut wifi, mut network) = match wifi::WifiManager::new(peripherals.WIFI) {
        Ok((manager, interface)) => {
            // The hardware RNG is a true entropy source while the radio is running.
            let rng = esp_hal::rng::Rng::new();
            let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
            (Some(manager), Some(net::Network::new(interface, seed)))
        }
        Err(error) => {
            log::error!("Unable to initialize Wi-Fi: {error:?}");
            (None, None)
        }
    };

    // --- Battery gauge: GPIO10 / ADC1_CH9, 2:1 divider ---
    let mut battery = battery::Battery::new(peripherals.ADC1, peripherals.GPIO10);
    let mut scan_networks: Vec<wifi::ScanNetwork> = Vec::new();
    // Socket buffers are reserved at boot, while the heap is still fresh.
    // First renders of the Slint views permanently consume a chunk of heap;
    // creating the server lazily on first open then failed with LOW MEMORY.
    let mut ftp_server: Option<ftp::FtpServer> = network
        .as_ref()
        .map(|network| ftp::FtpServer::new(network.stack()));
    // The SSH listener follows the same rule: buffers reserved at boot.
    let mut ssh_server: Option<ssh::SshServer> = network
        .as_ref()
        .map(|network| ssh::SshServer::new(network.stack(), storage.as_ref()));
    let mut file_manager: Option<filemanager::FileManager> = None;

    // --- Slint: minimal software window + platform ---
    let window = MinimalSoftwareWindow::new(
        slint::platform::software_renderer::RepaintBufferType::ReusedBuffer,
    );
    window.set_size(slint::PhysicalSize::new(
        LCD_WIDTH as u32,
        LCD_HEIGHT as u32,
    ));

    let boot_micros = Instant::now().duration_since_epoch().as_micros();
    slint::platform::set_platform(Box::new(EspBackend {
        window: window.clone(),
        boot_micros,
    }))
    .expect("Slint platform already set");

    let ui = MainWindow::new().unwrap();
    ui.set_saved_networks(saved_network_model(&wifi_config));
    ui.set_scan_networks(string_model(Vec::new()));
    ui.set_wifi_status(
        if wifi.is_none() {
            "WI-FI INIT ERROR"
        } else if storage_available {
            "READY"
        } else {
            "SD ERROR"
        }
        .into(),
    );
    ui.set_usb_disk_status(
        if usb_disk.available() {
            "CONNECT USB CABLE"
        } else {
            "USB INIT ERROR"
        }
        .into(),
    );
    set_ftp_login_text(&ui, &wifi_config);
    ui.set_ftp_status("STOPPED".into());

    // --- RTC watchdog: reset instead of waiting for a battery pull ---
    // Arming happens here, after radios, SD and UI are up, so slow first-run
    // initialisation is never mistaken for a hang.
    let mut watchdog = watchdog::Watchdog::new(peripherals.RTC_TIMER);
    log::info!(
        "Reset cause: {} (boot before: {}, watchdog resets since power-on: {})",
        watchdog.boot_cause(),
        watchdog.previous_cause(),
        watchdog.watchdog_resets()
    );
    let last = watchdog.previous_window();
    log::info!(
        "Watchdog window before that reset: {} s (hold={} cal={})",
        last.window_s,
        last.hold,
        last.cal
    );
    watchdog.arm();

    // Single-line buffer (ReusedBuffer) — 240 px RGB565
    let mut line_buffer: [Rgb565Pixel; LCD_WIDTH] = [Rgb565Pixel(0); LCD_WIDTH];

    // --- main loop: Slint tick + redraw + splash -> firmware transition ---
    let mut frame_idx: u32 = 0;
    let mut splash_done = false;
    let splash_start = Instant::now();
    let mut last_switch = splash_start;
    let mut radio_pending: Option<RadioStep> = None;
    // At most three full saved-network passes per boot or link-loss episode.
    // The automatic job is polled once per loop; no blocking radio call runs
    // from the main loop while it owns the controller.
    let mut auto_next_index = wifi_config.networks.len().saturating_sub(1);
    let mut auto_pass: u8 = 1;
    let mut auto_job: Option<AutoConnectJob> = None;
    let mut auto_connect_at = (wifi.is_some() && !wifi_config.networks.is_empty()).then_some(
        splash_start + Duration::from_millis(SPLASH_AFTER_MS + AUTO_CONNECT_AFTER_SPLASH_MS),
    );

    // Top-bar state; `shown_*` caches avoid re-setting unchanged properties.
    let mut wall_clock = clock::WallClock::default();
    let mut next_sntp_at = splash_start;
    let mut link_was_up = false;
    let mut shown_clock: Option<Option<(u8, u8)>> = None;
    let mut shown_link_state = -1;
    let mut last_battery_at: Option<Instant> = None;
    let mut usb_sd = None;
    let mut shown_usb_state = None;
    let mut next_usb_stats_at = Instant::now();
    let mut next_ftp_stats_at = Instant::now();
    let mut usb_force_exit_armed = false;
    // Reboot once the acknowledgement left the FIFO, or after a deadline: a host
    // that closes the port can leave the final flush pending forever.
    let mut debug_reboot_at: Option<Instant> = None;
    // Watchdog self-test: wedge the loop once the reply has left the FIFO.
    let mut debug_freeze_at: Option<Instant> = None;
    // Watchdog self-test: block one iteration for longer than the ordinary
    // window but shorter than the slow window, like a slow radio step.
    let mut debug_stall_at: Option<Instant> = None;
    // Sampled low-water mark, visible through STATUS even without profiling.
    // Transient allocations between samples need the heap-profiling feature.
    let mut heap_free_min = esp_alloc::HEAP.free();
    loop {
        slint::platform::update_timers_and_animations();

        let now = Instant::now();
        // One feed per iteration: the watchdog resets if this line stops
        // running, whichever call is holding the loop.
        watchdog.feed(now);
        heap_free_min = heap_free_min.min(esp_alloc::HEAP.free());

        // After SPLASH_AFTER_MS: switch to the main screen (Slint states, 600 ms crossfade)
        if !splash_done && now - splash_start >= Duration::from_millis(SPLASH_AFTER_MS) {
            ui.set_splash_done(true);
            splash_done = true;
        }

        // USB debug control is intentionally handled before physical input, so
        // an attached host can drive the exact same Slint navigation callbacks.
        if let Some(command) = debug_console.poll() {
            match command {
                debug::DebugCommand::Ping => {
                    debug_console.ok(format_args!("PONG protocol=1"));
                    debug_console.end();
                }
                debug::DebugCommand::Help => {
                    debug_console.ok(format_args!("commands"));
                    debug_console.data(format_args!("PING"));
                    debug_console.data(format_args!("STATUS"));
                    debug_console.data(format_args!(
                        "KEY up|down|left|right|enter|back|backspace|delete|tab|space"
                    ));
                    debug_console.data(format_args!("TEXT <printable ASCII>"));
                    debug_console.data(format_args!("WDT [ON|OFF|FREEZE|STALL]"));
                    debug_console.data(format_args!("CLEAR"));
                    debug_console.data(format_args!("REBOOT"));
                    debug_console.end();
                }
                debug::DebugCommand::Status => {
                    let view = ui.get_view_state();
                    debug_console.ok(format_args!("status"));
                    debug_console.data(format_args!(
                        "system uptime_ms={} heap_free={} heap_free_min={}",
                        now.duration_since_epoch().as_millis(),
                        esp_alloc::HEAP.free(),
                        heap_free_min
                    ));
                    debug_console.data(format_args!(
                        "watchdog armed={} window_s={} loop_max_ms={} last_reset={} \
                         previous_reset={} wdt_resets={}",
                        watchdog.is_armed(),
                        watchdog.window_secs(),
                        watchdog.slowest_iteration_ms(),
                        watchdog.boot_cause(),
                        watchdog.previous_cause(),
                        watchdog.watchdog_resets(),
                    ));
                    #[cfg(feature = "heap-profiling")]
                    debug_console.data(format_args!(
                        "heap_peak_used={}",
                        esp_alloc::HEAP.stats().max_usage
                    ));
                    debug_console.data(format_args!(
                        "ui view={} name={} menu={} wifi_menu={} saved={} scan={}",
                        view,
                        view_name(view),
                        ui.get_menu_index(),
                        ui.get_wifi_menu_index(),
                        ui.get_saved_index(),
                        ui.get_scan_index()
                    ));
                    let radio = match radio_pending.as_ref() {
                        Some(RadioStep::Scan { .. }) => "scan",
                        Some(RadioStep::Connect { .. }) => "connect",
                        None if auto_job.is_some() => "auto_connect",
                        None => "idle",
                    };
                    let wifi_status = ui.get_wifi_status();
                    debug_console.data(format_args!(
                        "wifi link={} radio={} status={:?}",
                        ui.get_link_state(),
                        radio,
                        wifi_status.as_str()
                    ));
                    if let Some(network) = network.as_ref() {
                        let ipv4 = network
                            .stack()
                            .config_v4()
                            .map(|config| config.address.address());
                        debug_console.data(format_args!(
                            "network link_up={} online={} ipv4={ipv4:?}",
                            network.is_link_up(),
                            network.is_online()
                        ));
                    } else {
                        debug_console.data(format_args!("network unavailable"));
                    }
                    debug_console.data(format_args!(
                        "storage mounted={} usb_exported={}",
                        storage.is_some(),
                        usb_sd.is_some()
                    ));
                    let usb_status = ui.get_usb_disk_status();
                    debug_console.data(format_args!(
                        "usb state={:?} status={:?}",
                        usb_disk.state(),
                        usb_status.as_str()
                    ));
                    let ftp_status = ui.get_ftp_status();
                    let ftp_addr = ui.get_ftp_addr();
                    let ftp_peer = ftp_server.as_ref().and_then(|server| server.peer());
                    debug_console.data(format_args!(
                        "ftp state={:?} peer={ftp_peer:?} address={:?}",
                        ftp_status.as_str(),
                        ftp_addr.as_str()
                    ));
                    if let Some(server) = ssh_server.as_ref() {
                        debug_console.data(format_args!(
                            "ssh state={:?} host_key={} sessions={}",
                            server.state().as_str(),
                            server.fingerprint(),
                            server.sessions_total
                        ));
                    }
                    let clock = ui.get_clock_text();
                    debug_console.data(format_args!(
                        "top clock={:?} synced={} battery={} ssid={:?}",
                        clock.as_str(),
                        ui.get_clock_synced(),
                        ui.get_battery_percent(),
                        ui.get_link_ssid().as_str()
                    ));
                    if (10..=13).contains(&view) {
                        let path = ui.get_fm_path();
                        let status = ui.get_fm_status();
                        let selected = file_manager.as_ref().and_then(|fm| fm.selected_label());
                        debug_console.data(format_args!(
                            "sd path={:?} page={} index={} selected={:?} copy={} status={:?}",
                            path.as_str(),
                            ui.get_fm_page(),
                            file_manager.as_ref().map_or(0, |fm| fm.selected_index()),
                            selected.as_ref().map(SharedString::as_str),
                            file_manager.as_ref().is_some_and(|fm| fm.copying()),
                            status.as_str(),
                        ));
                    }
                    debug_console.data(format_args!(
                        "lists saved={} scan={}",
                        wifi_config.networks.len(),
                        scan_networks.len()
                    ));
                    for (index, network) in wifi_config.networks.iter().enumerate() {
                        debug_console.data(format_args!(
                            "saved index={index} ssid={:?} auth={:?}",
                            network.ssid, network.auth
                        ));
                    }
                    for (index, network) in scan_networks.iter().enumerate() {
                        debug_console.data(format_args!(
                            "scan index={index} ssid={:?} rssi={} auth={:?} supported={}",
                            network.ssid, network.signal_strength, network.auth, network.supported
                        ));
                    }
                    debug_console.end();
                }
                debug::DebugCommand::Key { key } => {
                    if !splash_done || radio_pending.is_some() {
                        debug_console.error(format_args!("input_busy"));
                    } else if matches!(key, debug::DebugKey::Enter)
                        && ui.get_view_state() == 0
                        && ui.get_menu_index() == 2
                    {
                        // Entering USB DISK moves this same physical PHY from
                        // Serial/JTAG to OTG, so a remote-only session could not
                        // send Backspace to leave it.
                        debug_console.error(format_args!("usb_disk_requires_physical_input"));
                    } else if file_manager.as_ref().is_some_and(|fm| fm.copying()) {
                        debug_console.error(format_args!("sd_copy_busy"));
                    } else {
                        if (10..=13).contains(&ui.get_view_state()) {
                            if let (Some(fm), Some(volume)) =
                                (file_manager.as_mut(), storage.as_ref())
                            {
                                fm.key(file_debug_key(key), volume, &ui);
                            }
                        } else {
                            apply_debug_key(&ui, key);
                        }
                        debug_console.ok(format_args!(
                            "key={key:?} view={} name={}",
                            ui.get_view_state(),
                            view_name(ui.get_view_state())
                        ));
                    }
                    debug_console.end();
                }
                debug::DebugCommand::Text { text } => {
                    if !splash_done || radio_pending.is_some() {
                        debug_console.error(format_args!("input_busy"));
                    } else {
                        let result = if ui.get_view_state() == 12 {
                            file_manager
                                .as_mut()
                                .ok_or("sd_not_open")
                                .and_then(|fm| fm.append_text(text.as_str(), &ui))
                        } else {
                            append_debug_text(&ui, text.as_str())
                        };
                        match result {
                            Ok(()) => debug_console.ok(format_args!("text_accepted")),
                            Err(reason) => debug_console.error(format_args!("{reason}")),
                        }
                    }
                    debug_console.end();
                }
                debug::DebugCommand::Clear => {
                    match ui.get_view_state() {
                        5 => {
                            set_password(&ui, "");
                            debug_console.ok(format_args!("wifi_text_cleared"));
                        }
                        9 => {
                            ui.set_ftp_password("".into());
                            debug_console.ok(format_args!("ftp_text_cleared"));
                        }
                        12 => {
                            if let Some(fm) = file_manager.as_mut() {
                                fm.clear_text(&ui);
                                debug_console.ok(format_args!("sd_name_cleared"));
                            }
                        }
                        _ => debug_console.error(format_args!("text_entry_not_active")),
                    }
                    debug_console.end();
                }
                debug::DebugCommand::Reboot => {
                    debug_console.ok(format_args!("rebooting"));
                    debug_console.end();
                    debug_reboot_at = Some(Instant::now() + Duration::from_millis(500));
                }
                debug::DebugCommand::Wdt { action } => {
                    match action {
                        debug::DebugWdt::Status => {
                            debug_console.ok(format_args!(
                                "wdt={}",
                                if watchdog.is_armed() {
                                    "armed"
                                } else {
                                    "disarmed"
                                }
                            ));
                            debug_console.data(format_args!(
                                "window_s={} loop_max_ms={} last_reset={} previous_reset={} \
                                 wdt_resets={}",
                                watchdog.window_secs(),
                                watchdog.slowest_iteration_ms(),
                                watchdog.boot_cause(),
                                watchdog.previous_cause(),
                                watchdog.watchdog_resets(),
                            ));
                            let now_window = watchdog.current_window();
                            let last = watchdog.previous_window();
                            debug_console.data(format_args!(
                                "now window_s={} hold={} cal={} | before_reset window_s={} \
                                 hold={} cal={}",
                                now_window.window_s,
                                now_window.hold,
                                now_window.cal,
                                last.window_s,
                                last.hold,
                                last.cal,
                            ));
                        }
                        debug::DebugWdt::On => {
                            if !watchdog.is_armed() {
                                watchdog.arm();
                            }
                            debug_console.ok(format_args!(
                                "wdt=armed reset_after_s={}",
                                watchdog.window_secs()
                            ));
                        }
                        debug::DebugWdt::Off => {
                            watchdog.disarm();
                            debug_console.ok(format_args!("wdt=disarmed"));
                        }
                        debug::DebugWdt::Stall => {
                            if usb_sd.is_some() {
                                debug_console.error(format_args!("busy_usb_disk"));
                            } else {
                                debug_console
                                    .ok(format_args!("wdt=stalling seconds={}", WDT_STALL_SECS));
                                debug_stall_at = Some(Instant::now() + Duration::from_millis(300));
                            }
                        }
                        debug::DebugWdt::Freeze => {
                            // A core reset in the middle of MSC service would land
                            // on the host's side of a write, so refuse it there.
                            if usb_sd.is_some() {
                                debug_console.error(format_args!("busy_usb_disk"));
                            } else {
                                debug_console.ok(format_args!("wdt=self_test_resetting"));
                                debug_freeze_at = Some(Instant::now() + Duration::from_millis(300));
                            }
                        }
                    }
                    debug_console.end();
                }
                debug::DebugCommand::Invalid { reason } => {
                    debug_console.error(format_args!("{reason}"));
                    debug_console.end();
                }
            }
        }

        // Full keyboard decoding includes printable characters and the Fn layer.
        if let Ok(inputs) = keyboard.inputs() {
            for input in inputs {
                // Drain the FIFO but DISCARD input while the splash is up or a radio
                // operation is in progress, so stray presses neither open views behind
                // the animation nor replay all at once when the radio unblocks.
                if !splash_done || radio_pending.is_some() {
                    continue;
                }
                if (10..=13).contains(&ui.get_view_state()) {
                    if let (Some(fm), Some(volume)) = (file_manager.as_mut(), storage.as_ref()) {
                        use filemanager::Key;
                        let key = match input {
                            KeyInput::Char(c) if ui.get_view_state() == 12 => Some(Key::Char(c)),
                            KeyInput::Char(' ') => Some(Key::Char(' ')),
                            KeyInput::Char(',') => Some(Key::Left),
                            KeyInput::Char(';') => Some(Key::Up),
                            KeyInput::Char('.') => Some(Key::Down),
                            KeyInput::Char('/') => Some(Key::Right),
                            KeyInput::Char(_) | KeyInput::Modifier(_) => None,
                            KeyInput::Enter => Some(Key::Enter),
                            KeyInput::Backspace if ui.get_view_state() == 12 => {
                                Some(Key::Backspace)
                            }
                            KeyInput::Backspace | KeyInput::Escape => Some(Key::Back),
                            KeyInput::Delete => Some(Key::Delete),
                            KeyInput::Tab => Some(Key::Tab),
                            KeyInput::Arrow(Arrow::Up) => Some(Key::Up),
                            KeyInput::Arrow(Arrow::Down) => Some(Key::Down),
                            KeyInput::Arrow(Arrow::Left) => Some(Key::Left),
                            KeyInput::Arrow(Arrow::Right) => Some(Key::Right),
                        };
                        if let Some(key) = key {
                            fm.key(key, volume, &ui);
                        }
                    }
                    continue;
                }
                match input {
                    KeyInput::Char(character) if ui.get_view_state() == 5 => {
                        if character.is_ascii() && !character.is_ascii_control() {
                            let mut password = ui.get_wifi_password().to_string();
                            if password.len() < 64 {
                                password.push(character);
                                set_password(&ui, &password);
                            }
                        }
                    }
                    KeyInput::Char(character) if ui.get_view_state() == 9 => {
                        if character.is_ascii_graphic() {
                            let mut password = ui.get_ftp_password().to_string();
                            if password.len() < 32 {
                                password.push(character);
                                ui.set_ftp_password(password.into());
                            }
                        }
                    }
                    KeyInput::Char(' ') => ui.invoke_key_pressed("space".into()),
                    // Legacy ADV behavior: outside text entry the bare `,` `;` `.` `/`
                    // keys act as arrows (directions match the Fn layer in the crate).
                    KeyInput::Char(',') => ui.invoke_key_pressed("left".into()),
                    KeyInput::Char(';') => ui.invoke_key_pressed("up".into()),
                    KeyInput::Char('.') => ui.invoke_key_pressed("down".into()),
                    KeyInput::Char('/') => ui.invoke_key_pressed("right".into()),
                    KeyInput::Char(_) | KeyInput::Modifier(_) => {}
                    KeyInput::Enter => ui.invoke_key_pressed("enter".into()),
                    KeyInput::Backspace if ui.get_view_state() == 5 => {
                        let mut password = ui.get_wifi_password().to_string();
                        password.pop();
                        set_password(&ui, &password);
                    }
                    KeyInput::Backspace if ui.get_view_state() == 9 => {
                        let mut password = ui.get_ftp_password().to_string();
                        password.pop();
                        ui.set_ftp_password(password.into());
                    }
                    KeyInput::Backspace | KeyInput::Escape => ui.invoke_key_pressed("back".into()),
                    KeyInput::Delete => ui.invoke_key_pressed("delete".into()),
                    KeyInput::Tab => ui.invoke_key_pressed("tab".into()),
                    KeyInput::Arrow(Arrow::Up) => ui.invoke_key_pressed("up".into()),
                    KeyInput::Arrow(Arrow::Down) => ui.invoke_key_pressed("down".into()),
                    KeyInput::Arrow(Arrow::Left) => ui.invoke_key_pressed("left".into()),
                    KeyInput::Arrow(Arrow::Right) => ui.invoke_key_pressed("right".into()),
                }
            }
        }

        // Opening from the menu never retains a FAT handle. Drop browser state
        // on exit, before USB can take ownership of the raw SD card.
        if (10..=13).contains(&ui.get_view_state()) {
            if file_manager.is_none() {
                if let Some(volume) = storage.as_ref() {
                    if ftp_server
                        .as_ref()
                        .is_none_or(|server| server.status() == ftp::FtpStatus::Stopped)
                    {
                        file_manager = Some(filemanager::FileManager::new(volume, &ui));
                    } else {
                        ui.set_view_state(0);
                    }
                } else {
                    ui.set_view_state(0);
                }
            }
        } else {
            file_manager = None;
        }
        if let (Some(fm), Some(volume)) = (file_manager.as_mut(), storage.as_ref()) {
            // A single bounded FAT step per pass keeps other services responsive.
            fm.poll(volume, &ui);
        }

        // Execute one radio step. Its status (spinner + progress) was rendered in
        // previous iterations; only the current step blocks below.
        if let Some(step) = radio_pending.take() {
            // A scan pass or an association attempt is expected to block for
            // seconds; widen the reset window so it does not look like a hang.
            let _slow_radio_step = watchdog.slow_window();
            ui.set_wifi_status(busy_status(&step, frame_idx).into());
            radio_pending = match step {
                RadioStep::Scan { pass } => {
                    match wifi.as_mut().map(|manager| manager.scan_pass()) {
                        Some(Ok(results)) => {
                            wifi::merge_scan_results(&mut scan_networks, results);
                            ui.set_scan_networks(scan_network_model(&scan_networks));
                            if pass < wifi::SCAN_PASSES {
                                Some(RadioStep::Scan { pass: pass + 1 })
                            } else {
                                // Scan done: close the busy overlay or it stays on top
                                // of the scan view forever.
                                ui.set_wifi_connecting(false);
                                ui.set_wifi_status(
                                    if scan_networks.is_empty() {
                                        "NO NETWORKS FOUND"
                                    } else {
                                        "* = PASSWORD  ! = UNSUPPORTED"
                                    }
                                    .into(),
                                );
                                None
                            }
                        }
                        Some(Err(error)) => {
                            log::warn!("Wi-Fi scan pass {pass} failed: {error:?}");
                            ui.set_wifi_connecting(false);
                            ui.set_wifi_status(
                                if scan_networks.is_empty() {
                                    "SCAN FAILED"
                                } else {
                                    "SCAN PARTIAL - SHOWING WHAT WE HAVE"
                                }
                                .into(),
                            );
                            None
                        }
                        None => {
                            ui.set_wifi_status("WI-FI NOT AVAILABLE".into());
                            None
                        }
                    }
                }
                RadioStep::Connect { network, attempt } => {
                    match wifi.as_mut().map(|manager| manager.connect_attempt()) {
                        Some(Ok(())) => {
                            let ssid = network.ssid.clone();
                            ui.set_link_ssid(truncate_ascii(&ssid, TOP_BAR_SSID_CHARS).into());
                            wifi_config.upsert(network);
                            ui.set_saved_networks(saved_network_model(&wifi_config));
                            auto_next_index = wifi_config
                                .networks
                                .iter()
                                .position(|entry| entry.ssid == ssid)
                                .unwrap_or_else(|| wifi_config.networks.len().saturating_sub(1));
                            auto_connect_at = None;
                            let save_failed = storage.as_ref().is_none_or(|manager| {
                                storage::save(manager, &wifi_config).is_err()
                            });
                            ui.set_wifi_status(if save_failed {
                                "CONNECTED - SD SAVE FAILED".into()
                            } else {
                                format!("CONNECTED: {}", display_ssid(&ssid)).into()
                            });
                            if ui.get_view_state() == 5 {
                                set_password(&ui, "");
                                ui.set_view_state(2);
                            }
                            ui.set_wifi_connecting(false);
                            None
                        }
                        Some(Err(_)) if attempt < wifi::CONNECT_ATTEMPTS => {
                            Some(RadioStep::Connect {
                                network,
                                attempt: attempt + 1,
                            })
                        }
                        Some(Err(_)) => {
                            ui.set_wifi_connecting(false);
                            ui.set_wifi_status("CONNECTION FAILED".into());
                            None
                        }
                        None => {
                            ui.set_wifi_connecting(false);
                            ui.set_wifi_status("WI-FI NOT AVAILABLE".into());
                            auto_connect_at = None;
                            None
                        }
                    }
                }
            };
        }

        // Poll the automatic radio future once per iteration. Its event
        // subscriber wakes from esp-rtos; repeated main-loop polling works
        // without an executor or a self-referential controller borrow.
        let finished = auto_job.as_mut().and_then(|job| {
            let mut context = Context::from_waker(Waker::noop());
            match job.future.as_mut().poll(&mut context) {
                Poll::Ready(result) => Some(result),
                Poll::Pending => None,
            }
        });
        if let Some((manager, result)) = finished {
            let ssid = auto_job.take().unwrap().ssid;
            wifi = Some(manager);
            match result {
                Ok(()) => {
                    log::info!("Auto-connected to saved network {ssid}");
                    ui.set_link_ssid(truncate_ascii(&ssid, TOP_BAR_SSID_CHARS).into());
                    ui.set_wifi_status(format!("CONNECTED: {}", display_ssid(&ssid)).into());
                    auto_connect_at = None;
                    auto_pass = 1;
                }
                Err(error) => {
                    log::warn!("Auto-connect to {ssid} failed: {error:?}");
                    ui.set_wifi_status("AUTO CONNECT FAILED".into());
                    retry_saved_network(
                        &mut auto_next_index,
                        &mut auto_pass,
                        &mut auto_connect_at,
                        wifi_config.networks.len(),
                    );
                }
            }
        }

        // Manual Wi-Fi commands retain the existing blocking behavior. While
        // the automatic future owns the radio, keep a requested action queued
        // until it completes; users can still navigate every other screen.
        let wifi_action = ui.get_wifi_action();
        if wifi_action != 0 && auto_job.is_some() {
            auto_connect_at = None;
            let view = ui.get_view_state();
            if !matches!(
                (wifi_action, view),
                (1, 4) | (2, 3) | (3, 4) | (4, 5) | (5, 3)
            ) {
                ui.set_wifi_action(0);
                ui.set_wifi_connecting(false);
            } else {
                ui.set_wifi_connecting(false);
                if ui.get_wifi_status().as_str() != "WAIT FOR RADIO" {
                    ui.set_wifi_status("WAIT FOR RADIO".into());
                }
            }
        } else if wifi_action != 0 {
            // Manual Wi-Fi input takes precedence over any later auto pass.
            auto_connect_at = None;
            ui.set_wifi_action(0);
            ui.set_wifi_connecting(false);
            match wifi_action {
                1 if radio_pending.is_none() => {
                    if wifi.is_none() {
                        ui.set_wifi_status("WI-FI NOT AVAILABLE".into());
                    } else {
                        scan_networks.clear();
                        ui.set_scan_networks(string_model(Vec::new()));
                        ui.set_scan_index(0);
                        let step = RadioStep::Scan { pass: 1 };
                        ui.set_wifi_status(busy_status(&step, frame_idx).into());
                        ui.set_wifi_connecting(true);
                        radio_pending = Some(step);
                    }
                }
                2 if radio_pending.is_none() => {
                    let index = ui.get_wifi_action_index() as usize;
                    if let Some(network) = wifi_config.networks.get(index).cloned() {
                        auto_connect_at = None;
                        radio_pending = begin_connect(&mut wifi, &ui, network, frame_idx);
                    }
                }
                3 if radio_pending.is_none() => {
                    let index = ui.get_wifi_action_index() as usize;
                    if let Some(network) = scan_networks.get(index).cloned() {
                        if !network.supported {
                            ui.set_wifi_status("SECURITY NOT SUPPORTED".into());
                        } else if network.auth == storage::AuthKind::Open {
                            radio_pending = begin_connect(
                                &mut wifi,
                                &ui,
                                SavedNetwork {
                                    ssid: network.ssid.clone(),
                                    password: String::new(),
                                    auth: network.auth,
                                },
                                frame_idx,
                            );
                        } else {
                            ui.set_wifi_selected_ssid(network.ssid.into());
                            set_password(&ui, "");
                            ui.set_wifi_status("TYPE PASSWORD".into());
                            ui.set_view_state(5);
                        }
                    }
                }
                4 if radio_pending.is_none() => {
                    let selected_ssid = ui.get_wifi_selected_ssid().to_string();
                    let password = ui.get_wifi_password().to_string();
                    if let Some(network) = scan_networks
                        .iter()
                        .find(|network| network.ssid == selected_ssid)
                        .cloned()
                    {
                        radio_pending = begin_connect(
                            &mut wifi,
                            &ui,
                            SavedNetwork {
                                ssid: network.ssid.clone(),
                                password,
                                auth: network.auth,
                            },
                            frame_idx,
                        );
                    }
                }
                5 => {
                    let index = ui.get_wifi_action_index() as usize;
                    if wifi_config.remove(index) {
                        ui.set_saved_index(0);
                        ui.set_saved_networks(saved_network_model(&wifi_config));
                        auto_next_index = wifi_config.networks.len().saturating_sub(1);
                        if wifi_config.networks.is_empty() {
                            auto_connect_at = None;
                        }
                        if storage
                            .as_ref()
                            .is_none_or(|manager| storage::save(manager, &wifi_config).is_err())
                        {
                            ui.set_wifi_status("SD SAVE FAILED".into());
                        } else {
                            ui.set_wifi_status("NETWORK FORGOTTEN".into());
                        }
                    }
                }
                _ => {}
            }
        }

        // Start one owned future only on non-editing screens. Once started it
        // continues to be polled on every screen without blocking the UI.
        if radio_pending.is_none()
            && auto_job.is_none()
            && wifi_action == 0
            && !network.as_ref().is_some_and(|network| network.is_link_up())
            && auto_connect_at.is_some_and(|due| Instant::now() >= due)
            && matches!(ui.get_view_state(), 0 | 1 | 6)
            && usb_sd.is_none()
        {
            if let Some(saved) = wifi_config.networks.get(auto_next_index).cloned() {
                if let Some(manager) = wifi.take() {
                    log::info!(
                        "Auto-connecting (pass {auto_pass}/{MAX_AUTO_PASSES}) to {}",
                        saved.ssid
                    );
                    let ssid = saved.ssid.clone();
                    auto_job = Some(AutoConnectJob {
                        ssid,
                        future: Box::pin(manager.connect_automatically(saved)),
                    });
                    auto_connect_at = None;
                    ui.set_wifi_connecting(false);
                    ui.set_wifi_status(
                        format!("AUTO CONNECT {auto_pass}/{MAX_AUTO_PASSES}").into(),
                    );
                } else {
                    auto_connect_at = None;
                }
            } else {
                auto_connect_at = None;
            }
        }

        // --- USB MSC: temporarily move the raw SD card out of the FAT manager ---
        // Poll before handling EXIT so a just-received host eject is observed.
        if usb_sd.is_some() {
            usb_disk.poll();
        }
        let usb_action = ui.get_usb_disk_action();
        if usb_action != 0 {
            ui.set_usb_disk_action(0);
            match usb_action {
                1 if usb_sd.is_none()
                    && ftp_server
                        .as_ref()
                        .is_none_or(|server| server.status() == ftp::FtpStatus::Stopped) =>
                {
                    if let Some(volume) = storage.take() {
                        let card = Box::new(storage::free(volume));
                        if usb_disk.attach(card.as_ref()) {
                            log::info!("SD card exported over USB MSC");
                            usb_sd = Some(card);
                            shown_usb_state = None;
                            usb_force_exit_armed = false;
                        } else {
                            storage = storage::mount(*card);
                            ui.set_usb_disk_status("USB INIT ERROR".into());
                        }
                    } else {
                        ui.set_usb_disk_status("SD ALREADY IN USE".into());
                    }
                }
                1 if ftp_server
                    .as_ref()
                    .is_some_and(|server| server.status() != ftp::FtpStatus::Stopped) =>
                {
                    ui.set_usb_disk_status("SD IN USE BY FTP".into());
                }
                2 if usb_sd.is_some() => {
                    if usb_disk.can_detach() || usb_force_exit_armed {
                        if usb_force_exit_armed && !usb_disk.can_detach() {
                            log::warn!("Forcing USB MSC disconnect without host eject");
                        }
                        usb_disk.detach();
                        let sd_card = *usb_sd.take().unwrap();
                        storage = storage::mount(sd_card);
                        let reload_ok = match storage
                            .as_ref()
                            .ok_or(StorageError::Sd)
                            .and_then(|volume| storage::load(volume))
                        {
                            Ok(config) => {
                                wifi_config = config;
                                true
                            }
                            Err(StorageError::Missing) => {
                                wifi_config = WifiConfig::default();
                                true
                            }
                            Err(error) => {
                                log::warn!(
                                    "Unable to reload Wi-Fi credentials after USB: {error:?}"
                                );
                                false
                            }
                        };
                        ui.set_saved_networks(saved_network_model(&wifi_config));
                        ui.set_saved_index(0);
                        if reload_ok && !wifi_config.networks.is_empty() && wifi.is_some() {
                            auto_next_index = wifi_config.networks.len() - 1;
                        } else if wifi_config.networks.is_empty() {
                            auto_connect_at = None;
                        }
                        ui.set_wifi_status(
                            if reload_ok {
                                "SD RELOADED"
                            } else {
                                "SD ERROR AFTER USB"
                            }
                            .into(),
                        );
                        ui.set_view_state(0);
                        ui.set_usb_disk_status("CONNECT USB CABLE".into());
                        shown_usb_state = None;
                        usb_force_exit_armed = false;
                        log::info!("USB MSC stopped; firmware regained the SD card");
                    } else {
                        // VBUS sensing is forced on this bare-metal port, so a cable
                        // unplug cannot always be distinguished from a mounted host.
                        // A second press provides an explicit (unsafe) escape hatch.
                        ui.set_usb_disk_status("EJECT OR PRESS EXIT AGAIN".into());
                        usb_force_exit_armed = true;
                    }
                }
                _ => {}
            }
        }

        if usb_sd.is_some() {
            let state = usb_disk.state();
            if shown_usb_state != Some(state) {
                ui.set_usb_disk_status(
                    match state {
                        usbdisk::UsbDiskState::Waiting => "CONNECT USB CABLE",
                        usbdisk::UsbDiskState::Mounted => "SD MOUNTED ON PC",
                        usbdisk::UsbDiskState::Ejected => "SAFE TO EXIT",
                        usbdisk::UsbDiskState::Inactive => "USB NOT ACTIVE",
                    }
                    .into(),
                );
                shown_usb_state = Some(state);
            }
            // Refresh counters at 2 Hz only: every redraw steals time from USB.
            if Instant::now() >= next_usb_stats_at {
                next_usb_stats_at = Instant::now() + Duration::from_millis(500);
                let stats = usb_disk.stats();
                let text = usb_stats_text(stats);
                if ui.get_usb_disk_stats() != text.as_str() {
                    ui.set_usb_disk_stats(text.into());
                }
            }
        }

        // --- FTP server: active only while its screen is open ---
        let ftp_action = ui.get_ftp_action();
        if ftp_action != 0 {
            ui.set_ftp_action(0);
            match ftp_action {
                1 => start_ftp(&mut ftp_server, &storage, &network, &wifi_config, &ui),
                2 => {
                    stop_ftp(&mut ftp_server, &storage);
                    ui.set_ftp_status("STOPPED".into());
                }
                3 => {
                    stop_ftp(&mut ftp_server, &storage);
                    ui.set_ftp_password(wifi_config.ftp.password.clone().into());
                    ui.set_ftp_status("TYPE NEW PASSWORD".into());
                }
                4 => {
                    let password = ui.get_ftp_password().to_string();
                    if !storage::valid_ftp_credential(&password) {
                        ui.set_ftp_status("1-32 ASCII, NO SPACES".into());
                    } else if let Some(manager) = storage.as_ref() {
                        wifi_config.ftp.password = password;
                        if storage::save(manager, &wifi_config).is_err() {
                            ui.set_ftp_status("SD SAVE FAILED".into());
                        } else {
                            set_ftp_login_text(&ui, &wifi_config);
                            ui.set_view_state(8);
                            start_ftp(&mut ftp_server, &storage, &network, &wifi_config, &ui);
                        }
                    } else {
                        ui.set_ftp_status("SD BUSY".into());
                    }
                }
                5 => {
                    ui.set_view_state(8);
                    start_ftp(&mut ftp_server, &storage, &network, &wifi_config, &ui);
                }
                _ => {}
            }
        }

        // Defensive invariant: any unexpected navigation away from FTP also
        // closes its files/volume before another feature can use the card.
        if !matches!(ui.get_view_state(), 8 | 9)
            && ftp_server
                .as_ref()
                .is_some_and(|server| server.status() != ftp::FtpStatus::Stopped)
        {
            stop_ftp(&mut ftp_server, &storage);
        }

        if let (Some(server), Some(manager), Some(network)) =
            (ftp_server.as_mut(), storage.as_ref(), network.as_ref())
        {
            if server.status() != ftp::FtpStatus::Stopped {
                server.poll(manager, network.stack(), &wifi_config.ftp);
                let status = if server.status() == ftp::FtpStatus::Transfer
                    && !server.activity().is_empty()
                {
                    truncate_ascii(server.activity(), 26)
                } else if let Some(peer) = server.peer() {
                    format!("{} {}", server.status().label(), truncate_ascii(peer, 12))
                } else {
                    server.status().label().to_string()
                };
                if ui.get_ftp_status() != status.as_str() {
                    ui.set_ftp_status(status.into());
                }
                let address = server
                    .ip()
                    .map(|ip| format!("FTP://{}:21", ip))
                    .unwrap_or_else(|| String::from("NO IP YET"));
                if ui.get_ftp_addr() != address.as_str() {
                    ui.set_ftp_addr(address.into());
                }
                if now >= next_ftp_stats_at {
                    next_ftp_stats_at = now + Duration::from_millis(500);
                    ui.set_ftp_stats(ftp_stats_text(server.stats()).into());
                }
            }
        }

        // --- Network: DHCP/DNS/SNTP advance without blocking (one poll per iteration) ---
        if let Some(network) = network.as_mut() {
            if let Some(result) = network.poll() {
                let finished = Instant::now();
                match result {
                    Ok(unix_seconds) => {
                        log::info!("SNTP sync: {unix_seconds}");
                        wall_clock.set(unix_seconds, finished);
                        next_sntp_at = finished + Duration::from_secs(SNTP_RESYNC_SECS);
                    }
                    Err(error) => {
                        log::warn!("SNTP sync failed: {error:?}");
                        next_sntp_at = finished + Duration::from_secs(SNTP_RETRY_SECS);
                    }
                }
            }

            let link_up = network.is_link_up();
            if link_up && !link_was_up {
                // Fresh association: sync as soon as DHCP completes.
                next_sntp_at = now;
            } else if !link_up && link_was_up && radio_pending.is_none() {
                // Fresh link loss starts a new, bounded three-pass background
                // reconnect cycle. The clock keeps running from its last sync.
                ui.set_wifi_status("CONNECTION LOST".into());
                if !wifi_config.networks.is_empty() {
                    auto_next_index = wifi_config.networks.len() - 1;
                    auto_pass = 1;
                    auto_connect_at =
                        Some(Instant::now() + Duration::from_secs(AUTO_RECONNECT_AFTER_DROP_SECS));
                }
            }
            link_was_up = link_up;

            if network.is_online() && !network.sntp_running() && now >= next_sntp_at {
                network.start_sntp(wifi_config.clock.ntp_server.clone());
            }
        }

        // --- Top bar: Wi-Fi link, clock, battery ---
        let link_state = match (&radio_pending, network.as_ref()) {
            _ if auto_job.is_some() => 1,
            (Some(RadioStep::Connect { .. }), _) => 1,
            (_, Some(network)) if network.is_online() => 3,
            (_, Some(network)) if network.is_link_up() => 2,
            _ => 0,
        };
        if link_state != shown_link_state {
            ui.set_link_state(link_state);
            shown_link_state = link_state;
        }

        if let Some(unix_seconds) = wall_clock.unix_now() {
            storage::set_local_time(clock::local_seconds(unix_seconds, &wifi_config.clock));
        }
        let clock_hh_mm = wall_clock
            .unix_now()
            .map(|unix_seconds| clock::local_hh_mm(unix_seconds, &wifi_config.clock));
        if shown_clock != Some(clock_hh_mm) {
            let text = match clock_hh_mm {
                Some((hours, minutes)) => format!("{hours:02}:{minutes:02}"),
                None => String::from("--:--"),
            };
            ui.set_clock_text(text.into());
            ui.set_clock_synced(wall_clock.is_synced());
            shown_clock = Some(clock_hh_mm);
        }

        if last_battery_at.is_none_or(|at| now - at >= Duration::from_millis(BATTERY_EVERY_MS)) {
            if let Some(percent) = battery.sample_percent() {
                ui.set_battery_percent(i32::from(percent));
            }
            last_battery_at = Some(now);
        }

        // Frame ticker: splash and rat view animate the rat; while the radio is busy
        // the same ticker spins the Wi-Fi progress indicator.
        let animate_frame = !splash_done || ui.get_view_state() == 1 || radio_pending.is_some();
        if animate_frame && now - last_switch >= Duration::from_millis(FRAME_MS) {
            frame_idx = (frame_idx + 1) % 4;
            ui.set_frame_index(frame_idx as i32);
            if let Some(step) = radio_pending.as_ref() {
                ui.set_wifi_status(busy_status(step, frame_idx).into());
            }
            last_switch = now;
        }

        // SSH: one bounded step per pass; never blocks except for the key
        // exchange computation inside one progress() call.
        if let (Some(server), Some(_)) = (ssh_server.as_mut(), network.as_ref()) {
            let loop_max_ms = watchdog.slowest_iteration_ms();
            server.poll(storage.as_ref(), &wifi_config.ftp, &|| ssh::ShellStatus {
                heap_free: esp_alloc::HEAP.free(),
                heap_free_min,
                uptime_ms: Instant::now().duration_since_epoch().as_millis(),
                loop_max_ms,
            });
        }
        let ssh_busy = ssh_server.as_ref().is_some_and(ssh::SshServer::busy);

        // During a transfer, burst-poll the network and FTP for up to 15 ms.
        // This keeps TCP windows moving without starving input/display forever.
        let ftp_busy = ftp_server.as_ref().is_some_and(ftp::FtpServer::busy);
        if ftp_busy {
            let burst_start = Instant::now();
            while burst_start.elapsed() < Duration::from_millis(15) {
                let (Some(network), Some(manager), Some(server)) =
                    (network.as_mut(), storage.as_ref(), ftp_server.as_mut())
                else {
                    break;
                };
                network.poll_stack();
                server.poll(manager, network.stack(), &wifi_config.ftp);
                if server.status() != ftp::FtpStatus::Transfer {
                    break;
                }
            }
        }

        window.draw_if_needed(|renderer| {
            renderer.render_by_line(&mut HardwareDrawBuffer::new(&mut display, &mut line_buffer));
        });

        // Capture allocations from this iteration, including FTP and rendering.
        heap_free_min = heap_free_min.min(esp_alloc::HEAP.free());

        // Responses are queued so a disconnected host can never block the UI.
        // Reboot only after the acknowledgement has left the hardware FIFO.
        debug_console.service();
        if debug_reboot_at
            .is_some_and(|deadline| debug_console.output_idle() || Instant::now() >= deadline)
        {
            delay.delay_millis(20);
            esp_hal::system::software_reset();
        }

        if debug_stall_at
            .is_some_and(|deadline| debug_console.output_idle() || Instant::now() >= deadline)
        {
            debug_stall_at = None;
            let _slow = watchdog.slow_window();
            log::warn!("RTC watchdog self-test: stalling {WDT_STALL_SECS} s in the slow window");
            let stall_start = Instant::now();
            while stall_start.elapsed() < Duration::from_secs(WDT_STALL_SECS) {
                core::hint::spin_loop();
            }
            log::info!("RTC watchdog self-test: stall finished without a reset");
        }

        // Never returns: the watchdog must reset the chip within its window.
        if debug_freeze_at
            .is_some_and(|deadline| debug_console.output_idle() || Instant::now() >= deadline)
        {
            log::warn!("RTC watchdog self-test: main loop frozen, expecting a reset");
            loop {
                core::hint::spin_loop();
            }
        }

        delay.delay_millis(if ftp_busy || ssh_busy { 1 } else { 10 });
    }
}

fn set_ftp_login_text(ui: &MainWindow, config: &WifiConfig) {
    // 27 glyphs fit the 216 px content width. Long custom passwords are still
    // accepted but elided here; they remain editable in the password screen.
    let text = format!("USER:{} PASS:{}", config.ftp.user, config.ftp.password);
    ui.set_ftp_user(truncate_ascii(&text, 27).into());
}

fn start_ftp(
    server: &mut Option<ftp::FtpServer>,
    storage: &Option<storage::SdVolume>,
    network: &Option<net::Network>,
    config: &WifiConfig,
    ui: &MainWindow,
) {
    if storage.is_none() || network.is_none() {
        ui.set_ftp_status("SD OR WI-FI UNAVAILABLE".into());
        return;
    }
    let network = network.as_ref().unwrap();
    // The server exists since boot (network was up); starting only binds its
    // already-reserved sockets, so no low-memory branch is needed here.
    match server.as_mut().unwrap().start(network.stack()) {
        Ok(()) => {
            set_ftp_login_text(ui, config);
            ui.set_ftp_status("WAITING FOR WI-FI".into());
        }
        Err(()) => ui.set_ftp_status("SD VOLUME ERROR".into()),
    }
}

fn stop_ftp(server: &mut Option<ftp::FtpServer>, storage: &Option<storage::SdVolume>) {
    if let (Some(server), Some(_)) = (server.as_mut(), storage.as_ref()) {
        if server.status() != ftp::FtpStatus::Stopped {
            server.stop();
        }
    }
}

fn ftp_stats_text(stats: ftp::FtpStats) -> String {
    format!(
        "UP {}  DOWN {}  FREE {}K",
        byte_size_text(stats.sent_bytes),
        byte_size_text(stats.received_bytes),
        esp_alloc::HEAP.free() / 1024
    )
}

fn byte_size_text(bytes: u64) -> String {
    if bytes < 1024 * 1024 {
        format!("{}K", bytes / 1024)
    } else {
        format!("{}M", bytes / (1024 * 1024))
    }
}

/// "R 1234K W 56K C 87%": host reads/writes and the read cache hit rate.
fn usb_stats_text(stats: msc::Stats) -> String {
    let hit = if stats.read_blocks == 0 {
        0
    } else {
        (stats.cache_hit_blocks as u64 * 100 / stats.read_blocks as u64) as u32
    };
    format!(
        "R {} W {} C {}%",
        usb_size_text(stats.read_blocks),
        usb_size_text(stats.write_blocks),
        hit
    )
}

/// Short size for 512-byte block counts; switches to MiB to fit the 240 px line.
fn usb_size_text(blocks: u32) -> String {
    let kib = blocks / 2;
    if kib < 10_000 {
        format!("{kib}K")
    } else {
        format!("{}M", kib / 1024)
    }
}
