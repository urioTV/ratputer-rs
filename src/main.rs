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
mod msc;
mod net;
mod storage;
mod usbdisk;
mod wifi;

use cardputer_adv_keyboard::{Arrow, KeyInput, Keyboard};
use storage::{BuildTime, SavedNetwork, StorageError, WifiConfig};

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
// Glyph budget of the top-bar SSID field (120 px / 8 px per glyph).
const TOP_BAR_SSID_CHARS: usize = 15;

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

    esp_println::logger::init_logger_from_env();
    log::info!("RATPUTER (Slint) start");

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    let mut delay = Delay::new();

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
    // conservative 10 MHz data clock (the SPI default-speed limit is 25 MHz).
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
        let fast_config = SpiConfig::default().with_frequency(Rate::from_mhz(10));
        if sd_card
            .spi(|device| device.bus_mut().apply_config(&fast_config))
            .is_ok()
        {
            log::info!("SD initialized as {card_type:?}; SPI clock raised to 10 MHz");
        } else {
            log::warn!("SD initialized, but SPI remained at 400 kHz");
        }
    } else {
        log::warn!("SD initialization failed at 400 kHz");
    }
    // `None` means the raw card has been moved to USB MSC. Reconstructing the
    // manager after detach also invalidates its FAT block cache.
    let mut storage = Some(embedded_sdmmc::VolumeManager::new(sd_card, BuildTime));
    let (mut wifi_config, storage_available) = match storage::load(storage.as_ref().unwrap()) {
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

    // Single-line buffer (ReusedBuffer) — 240 px RGB565
    let mut line_buffer: [Rgb565Pixel; LCD_WIDTH] = [Rgb565Pixel(0); LCD_WIDTH];

    // --- main loop: Slint tick + redraw + splash -> firmware transition ---
    let mut frame_idx: u32 = 0;
    let mut splash_done = false;
    let splash_start = Instant::now();
    let mut last_switch = splash_start;
    let mut radio_pending: Option<RadioStep> = None;

    // Top-bar state; `shown_*` caches avoid re-setting unchanged properties.
    let mut wall_clock = clock::WallClock::default();
    let mut next_sntp_at = splash_start;
    let mut link_was_up = false;
    let mut shown_clock: Option<Option<(u8, u8)>> = None;
    let mut shown_link_state = -1;
    let mut last_battery_at: Option<Instant> = None;
    let mut usb_sd = None;
    let mut shown_usb_state = None;
    let mut usb_force_exit_armed = false;
    loop {
        slint::platform::update_timers_and_animations();

        let now = Instant::now();

        // After SPLASH_AFTER_MS: switch to the main screen (Slint states, 600 ms crossfade)
        if !splash_done && now - splash_start >= Duration::from_millis(SPLASH_AFTER_MS) {
            ui.set_splash_done(true);
            splash_done = true;
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

        // Execute one radio step. Its status (spinner + progress) was rendered in
        // previous iterations; only the current step blocks below.
        if let Some(step) = radio_pending.take() {
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
                            if storage
                                .as_ref()
                                .is_none_or(|manager| storage::save(manager, &wifi_config).is_err())
                            {
                                ui.set_wifi_status("CONNECTED - SD SAVE FAILED".into());
                            } else {
                                ui.set_wifi_status(
                                    format!("CONNECTED: {}", display_ssid(&ssid)).into(),
                                );
                            }
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
                            None
                        }
                    }
                }
            };
        }

        // Slint commands only SCHEDULE radio work. Each step runs below, one per
        // loop iteration, so the status/spinner keeps rendering between them.
        let wifi_action = ui.get_wifi_action();
        if wifi_action != 0 {
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

        // --- USB MSC: temporarily move the raw SD card out of the FAT manager ---
        // Poll before handling EXIT so a just-received host eject is observed.
        if usb_sd.is_some() {
            usb_disk.poll();
        }
        let usb_action = ui.get_usb_disk_action();
        if usb_action != 0 {
            ui.set_usb_disk_action(0);
            match usb_action {
                1 if usb_sd.is_none() => {
                    if let Some(manager) = storage.take() {
                        let (sd_card, _) = manager.free();
                        let card = Box::new(sd_card);
                        if usb_disk.attach(card.as_ref()) {
                            log::info!("SD card exported over USB MSC");
                            usb_sd = Some(card);
                            shown_usb_state = None;
                            usb_force_exit_armed = false;
                        } else {
                            storage = Some(embedded_sdmmc::VolumeManager::new(*card, BuildTime));
                            ui.set_usb_disk_status("USB INIT ERROR".into());
                        }
                    } else {
                        ui.set_usb_disk_status("SD ALREADY IN USE".into());
                    }
                }
                2 if usb_sd.is_some() => {
                    if usb_disk.can_detach() || usb_force_exit_armed {
                        if usb_force_exit_armed && !usb_disk.can_detach() {
                            log::warn!("Forcing USB MSC disconnect without host eject");
                        }
                        usb_disk.detach();
                        let sd_card = *usb_sd.take().unwrap();
                        storage = Some(embedded_sdmmc::VolumeManager::new(sd_card, BuildTime));
                        let reload_ok = match storage::load(storage.as_ref().unwrap()) {
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
                // The clock keeps running from the last sync; only the status changes.
                ui.set_wifi_status("CONNECTION LOST".into());
            }
            link_was_up = link_up;

            if network.is_online() && !network.sntp_running() && now >= next_sntp_at {
                network.start_sntp(wifi_config.clock.ntp_server.clone());
            }
        }

        // --- Top bar: Wi-Fi link, clock, battery ---
        let link_state = match (&radio_pending, network.as_ref()) {
            (Some(RadioStep::Connect { .. }), _) => 1,
            (_, Some(network)) if network.is_online() => 3,
            (_, Some(network)) if network.is_link_up() => 2,
            _ => 0,
        };
        if link_state != shown_link_state {
            ui.set_link_state(link_state);
            shown_link_state = link_state;
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

        window.draw_if_needed(|renderer| {
            renderer.render_by_line(&mut HardwareDrawBuffer::new(&mut display, &mut line_buffer));
        });

        delay.delay_millis(10);
    }
}
