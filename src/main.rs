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
use alloc::rc::Rc;

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
use esp_println as _;

mod keyboard;
use keyboard::{Keyboard, NavKey};

use mipidsi::interface::SpiInterface;
use mipidsi::models::ST7789;
use mipidsi::options::{ColorInversion, Orientation, Rotation};
use mipidsi::Builder;

use slint::platform::software_renderer::{LineBufferProvider, MinimalSoftwareWindow, Rgb565Pixel};

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

impl<DI: mipidsi::interface::Interface<Word = u8>, RST: embedded_hal::digital::OutputPin<Error = core::convert::Infallible>>
    LineBufferProvider for &mut HardwareDrawBuffer<'_, mipidsi::Display<DI, mipidsi::models::ST7789, RST>>
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

// ---------------------------------------------------------------------------
#[esp_hal::main]
fn main() -> ! {
    esp_alloc::heap_allocator!(size: HEAP_SIZE);

    esp_println::logger::init_logger_from_env();
    log::info!("RATPUTER (Slint) start");

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    let mut delay = Delay::new();

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
    let mut keyboard = Keyboard::new(i2c);

    // --- Slint: minimal software window + platform ---
    let window = MinimalSoftwareWindow::new(
        slint::platform::software_renderer::RepaintBufferType::ReusedBuffer,
    );
    window.set_size(slint::PhysicalSize::new(LCD_WIDTH as u32, LCD_HEIGHT as u32));

    let boot_micros = Instant::now().duration_since_epoch().as_micros();
    slint::platform::set_platform(Box::new(EspBackend {
        window: window.clone(),
        boot_micros,
    }))
    .expect("Slint platform already set");

    let ui = MainWindow::new().unwrap();

    // Single-line buffer (ReusedBuffer) — 240 px RGB565
    let mut line_buffer: [Rgb565Pixel; LCD_WIDTH] = [Rgb565Pixel(0); LCD_WIDTH];

    // --- main loop: Slint tick + redraw + splash -> firmware transition ---
    let mut frame_idx: u32 = 0;
    let mut splash_done = false;
    let splash_start = Instant::now();
    let mut last_switch = splash_start;
    loop {
        slint::platform::update_timers_and_animations();

        window.draw_if_needed(|renderer| {
            renderer
                .render_by_line(&mut HardwareDrawBuffer::new(&mut display, &mut line_buffer));
        });

        let now = Instant::now();

        // After SPLASH_AFTER_MS: switch to the main screen (Slint states, 600 ms crossfade)
        if !splash_done && now - splash_start >= Duration::from_millis(SPLASH_AFTER_MS) {
            ui.set_splash_done(true);
            splash_done = true;
        }

        // --- Keyboard: dispatch raw nav events to the Slint callback ---
        while let Some(nav) = keyboard.next_nav_key() {
            match nav {
                NavKey::Tab => { ui.invoke_key_pressed("tab".into()) }
                NavKey::Enter => { ui.invoke_key_pressed("enter".into()) }
                NavKey::Backspace => { ui.invoke_key_pressed("back".into()) }
                NavKey::Space => { ui.invoke_key_pressed("space".into()) }
                NavKey::ArrowUp => { ui.invoke_key_pressed("up".into()) }
                NavKey::ArrowDown => { ui.invoke_key_pressed("down".into()) }
                NavKey::ArrowLeft => { ui.invoke_key_pressed("left".into()) }
                NavKey::ArrowRight => { ui.invoke_key_pressed("right".into()) }
                NavKey::Other(_) => {}
            }
        }

        // Rat animation: in the splash AND when the mainscreen shows the rat view
        let animate_rat = !splash_done || ui.get_view_state() == 1;
        if animate_rat && now - last_switch >= Duration::from_millis(FRAME_MS) {
            frame_idx = (frame_idx + 1) % 4;
            ui.set_frame_index(frame_idx as i32);
            last_switch = now;
        }

        delay.delay_millis(10);
    }
}
