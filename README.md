# Ratputer RS — Cardputer ADV firmware in Rust

Firmware for the **M5Stack Cardputer ADV** (ESP32-S3FN8 / Stamp-S3A) written in Rust.
Contents: welcome splash with an animated pixel-art rat on a 240×135 ST7789V2 LCD,
a nav-menu UI, full keyboard support, Wi-Fi scanning/connection, SD-backed
credentials, and USB Mass Storage export of the SD card, everything in **Slint
(no_std)**. 🐀

## Stack

| Layer | Choice |
|---|---|
| Chip | ESP32-S3 (Xtensa LX7) → target `xtensa-esp32s3-none-elf` |
| Framework | `esp-hal` 1.2 (no_std, bare-metal, safe API) |
| UI | **Slint 1.18** (`renderer-software`, `unsafe-single-threaded`, `libm`) |
| Memory | `esp-alloc` 0.11 — 150 KB heap in **internal SRAM** (the ADV has no PSRAM!) |
| Display | Slint software renderer → `LineBufferProvider` per line → SPI, `mipidsi` 0.9 (ST7789) |
| Wi-Fi | `esp-radio` + `esp-rtos`, station mode, scan and association |
| Storage | `embedded-sdmmc`, FAT card on SPI3, TOML credentials |
| USB disk | TinyUSB 0.21 MSC/SCSI (isolated C component, no ESP-IDF/FreeRTOS) over ESP32-S3 USB-OTG |
| Keyboard | `cardputer-adv-keyboard` — full ASCII, Shift/Fn, arrows and editing keys |
| Fonts | **Press Start 2P** (OFL, pixel grid 8px) — `import "fonts/PressStart2P-Regular.ttf"` in .slint |
| Toolchain | **"esp"** Rust fork, pinned and supplied by the Nix devshell |
| Network | `embassy-net` 0.8 (DHCP + DNS + UDP) over the esp-radio station interface — SNTP clock sync |

> Previous font picks were DejaVu Sans/Mono — weak readability at 6–8 px; the pixel font fits the art natively.

## UI Slint: structure

- `build.rs` compiles `ui/ratputer.slint` with `EmbedForSoftwareRenderer`: font
  files and images land in flash;
- Every main-screen view sits below a 12 px **top bar**: local time (`HH:MM`,
  `--:--` until the first NTP sync), Wi-Fi signal bars + SSID (`CONNECTING`/`OFFLINE`),
  and the battery percentage with a gauge icon (red below 15 %);
- The template defines **two top-level screens** driven by the `splash-done` property
  (Slint `states` with `animate opacity { duration: 600ms; easing }`) plus eight
  mainscreen views (`view-state`: 0=menu, 1=rat, 2=Wi-Fi menu, 3=saved networks,
  4=scan results, 5=password, 6=about, 7=USB disk) and a
  **`key-pressed(string)`** callback:
  - **splash** — animated pixel-art rat (`ui/images/rat0..3.png` scaled ×4 =
    128×80 px; frames: bob / step / blink / tail-up) + header `RATPUTER · BOOTING`
    + footer `MODE STALKING/SNIFFING/HUNTING/LOITERING` with a spinner;
  - **menu** — item highlight (*menu-index*) on a greenish strip; keyboard:
    **↑↓ / ←→** = choose (Tab also works), **Enter/Space** = enter,
    **Backspace** = back;
  - **rat view** (`view-state == 1`) — the same animated pixel-art as the splash
    (frames keep cycling while the view is active);
  - **Wi-Fi views** — saved-network list, scan results, masked password entry,
    connection status, and forgetting credentials;
  - **USB disk** (`view-state == 7`) — exports the whole physical SD card to the
    connected computer and reports waiting/mounted/ejected state;
  - **about view** (`view-state == 6`) — technical info;
- The firmware (`src/main.rs`) polls the TCA8418 FIFO. Navigation keys are sent to
  Slint while printable characters are appended to the password in Rust. Wi-Fi
  commands are emitted by Slint and processed on the next main-loop iteration;
- Main loop: input → scheduled radio/USB work → network/top-bar updates → the
  250 ms frame ticker → `draw_if_needed(render_by_line)` at the bottom. After
  `SPLASH_AFTER_MS`, it calls `ui.set_splash_done(true)`;

### Keyboard — TCA8418RTWR via I2C0

The `cardputer-adv-keyboard` crate drains the TCA8418 FIFO and resolves the full
Cardputer ADV keymap, including Shift/Fn layers. In password entry, printable ASCII,
Space and Backspace edit the value; Enter connects; Escape (Fn+backtick) returns.
Navigation uses the bare `,` / `;` / `.` / `/` keys for left/up/down/right (Fn+those
keys yields the same arrows). Delete (Fn+Backspace) forgets the selected saved network.

### Wi-Fi credentials on SD

The SD card must contain a FAT volume. Credentials are stored at
`/RATPUTER/WIFI.CFG` (FAT paths are case-insensitive) as TOML:

```toml
version = 1

[clock]                       # optional; these are the defaults (CET/CEST)
utc_offset_minutes = 60
dst = "eu"                    # "eu" or "none"
ntp_server = "pool.ntp.org"

[[networks]]
ssid = "example"
password = "secret"
auth = "wpa2"
```

The file can hold up to 12 networks. The firmware never connects automatically:
open **WI-FI → SAVED NETWORKS** and select one, or use **SCAN NETWORKS** to add a
network after a successful connection. Only scan-visible SSIDs are supported.
Passwords are plain text on the removable card; TOML is a portable configuration
format, not encrypted storage. The current radio API supports open, WEP, WPA, WPA2,
and WPA/WPA2 networks; unsupported scan results are marked `!`.

SD wiring uses the ADV's dedicated SPI3 bus:

| GPIO | SD function |
|---|---|
| G40 | SCLK |
| G14 | MOSI |
| G39 | MISO |
| G12 | CS |

The card is identified at the standards-compliant 400 kHz startup clock. After
successful initialization, SPI3 switches to a conservative 10 MHz data clock
(the SD default-speed limit is 25 MHz).

### USB Mass Storage

Open **USB DISK** from the main menu to expose the complete physical SD card as a
standard writable USB Mass Storage/SCSI device. The native ESP32-S3 USB-OTG pins
are fixed: D−=GPIO19 and D+=GPIO20, both already connected to the Cardputer ADV
USB-C socket.

The USB implementation is a deliberately isolated C component: a vendored subset
of **TinyUSB 0.21.0** (device core, MSC/SCSI and Synopsys DWC2 controller) compiled
by `build.rs`. It uses no ESP-IDF or FreeRTOS. The DWC2 controller moves endpoint
buffers with its internal DMA; Rust polls completion events and TinyUSB's queue from
the normal firmware loop and provides sector callbacks backed by `embedded-sdmmc`.

Only one side owns the card at a time. Entering USB DISK consumes the firmware's
`VolumeManager`; leaving disconnects USB, recreates the FAT manager (discarding its
old cache), and reloads `RATPUTER/WIFI.CFG`. Wi-Fi credential writes are therefore
impossible while the host owns the card.

**Always eject/unmount `RATPUTER SD` on the computer before pressing Enter or
Backspace to exit.** The first exit attempt while the host is still mounted shows
a warning; pressing exit again forces disconnection for recovery after an
unplugged cable and can corrupt pending host writes. USB sector traffic uses the
post-initialization 10 MHz SD data clock.

The USB-OTG controller shares its PHY with ESP32-S3 USB-Serial-JTAG. The firmware
switches to OTG only when USB DISK opens and restores Serial/JTAG when it closes;
the serial port will disappear and re-enumerate during that interval. Descriptors
currently use TinyUSB's development VID `0xCAFE` with PID `0x4002` and are not
intended as production USB identifiers.

### Top bar: clock, Wi-Fi, battery

- **Clock** — after a connection gets a DHCP lease, the firmware sends an SNTP
  request to `clock.ntp_server` (falls back to `time.cloudflare.com` by IP if DNS
  fails). The result is anchored to the monotonic timer, so the time keeps running
  from the last sync when Wi-Fi drops or is never reconnected (until reboot — the
  ADV has no battery-backed RTC). Resync every hour while online, retry every 30 s
  on failure. Local time = UTC + `utc_offset_minutes` + EU DST (last Sunday of
  March → last Sunday of October, 01:00 UTC) when `dst = "eu"`.
- **Wi-Fi** — bars are bright when online (DHCP lease), dim when associated without
  an IP, sweep while connecting, dark when offline. A dropped link shows
  `CONNECTION LOST` in the Wi-Fi views.
- **Battery** — GPIO10 / ADC1_CH9 behind a 100k/100k divider (BAT+/2), read with
  eFuse curve calibration every 5 s, smoothed, and mapped through a 1S Li-ion
  discharge curve. While charging, the reading is the charger voltage, so it shows
  close to 100 %.

The network stack runs without an async executor: its runner and the SNTP job are
polled once per main-loop iteration, so DHCP/DNS/NTP never block the UI.

### Scan + connect reliability

The default active scan dwells only 10–20 ms per channel and visibly misses APs,
so the firmware uses a 50–250 ms dwell and merges **two scan passes** (status:
`SCANNING... 2 PASSES`, a few seconds). Association attempts are retried up to
3 times; per-attempt failures are logged over UART (`Wi-Fi connect attempt N failed`).

### Memory (ADV has no PSRAM!)

- Heap 150 KB in **internal SRAM** (`esp_alloc::heap_allocator!(size: 150*1024);`
  **called inside `main()` before any Box/Rc**); rest of the memory is `.bss`/stack;
- No persistent framebuffer — Slint in `ReusedBuffer` mode only holds **one raster
  line** (240 Rgb565 ≈ 480 B) and pushes it via `LineBufferProvider` →
  `mipidsi::Display::set_pixels(...)`;
- Fonts and images are packaged into flash by `build.rs` (embed resources);
- Previous version: 4 ASCII-art frames (by Gio) — in git history.

## Development environment

`nix develop` downloads the pinned Xtensa Rust fork, `rust-src`, LLVM, GCC, and
`espflash`. The toolchain stays in the Nix store; no global `espup install` or
`~/export-esp.sh` is required. The exact `esp-rs-nix` revision is recorded in
`flake.lock`.

> The build is verified: `cargo build --release` passes. The ELF is written to
> `target/xtensa-esp32s3-none-elf/release/`.

## Build & flash

### Simplest: merged .bin (one command)

```bash
nix develop
build                               # build + merged ratputer-adv.bin
```

The `build` devshell command runs the host-side `cargo xtask build`, which builds
the release firmware, invokes `espflash save-image`, and verifies all three image
headers. The image contains everything: bootloader @0x0 + partition table @0x8000
+ app @0x10000. The image uses **8 MB, DIO, 80 MHz**. DIO is required for
reliable ROM loading on the Cardputer ADV; a QIO bootloader header causes an early
`ets_loader.c 78` watchdog-reset loop before the second-stage bootloader starts.

To build and immediately flash the connected device with verification:

```bash
flash
```

For manual flashing of an existing image (espflash verifies writes by default):

```bash
espflash write-bin 0x0 ratputer-adv.bin
```

### Dev-loop flash (with UART monitor)

```bash
nix develop
cargo run --release     # flash + UART monitor (espflash monitor)
```

espflash auto-detects the device over USB-C.
Download mode (if the port doesn't appear): hold G0 while connecting USB.

## LCD pins (Cardputer ADV, per the M5Stack ST7789V2 pin map)

| GPIO | Function |
|---|---|
| G36 | SPI SCK |
| G35 | SPI MOSI (DAT) |
| G37 | CS |
| G34 | RS / DC |
| G33 | RST |
| G38 | Backlight |

The LCD bus runs at **40 MHz** — the reference espp implementation for this board
(`lcd_clock_speed = 40 * 1000 * 1000`). At 80 MHz the picture gets "scrambled": the
LCD pins (G33–G38) are not native IOMUX SPI2 pins on the ESP32-S3 (signals route
through the GPIO matrix), so ST7789 setup times are violated — data corruption shows
as a smeared/stretched image, different per frame.

If the image is **flipped horizontally/vertically** (but fills the screen) — change
`Rotation::Deg90` to `Rotation::Deg270` in `src/main.rs`. If it were
**offset/clipped**, check `display_size`/`display_offset` are given in the panel's
native orientation (below).

### Panel geometry (why these numbers)

The panel is a 1.14" ST7789V2 135×240 (native portrait), while the controller has
a 240×320 GRAM. The panel window is **centered** in it:

| Axis | GRAM range | Offset |
|---|---|---|
| 135 px → GRAM x | 52..186 | `(240-135)/2 = 52` |
| 240 px → GRAM y | 40..279 | `(320-240)/2 = 40` |

Hence (and because `Builder::new` in mipidsi ≥0.8 requires explicit sizes):
`display_size(135, 240)` in the **native** orientation + `display_offset(52, 40)`.
The landscape (240×135) rotation is done by MADCTL — mipidsi transforms the offset
itself (for `Deg90` it produces native `(40, 53)`, matching the `st7789_pico1`
variant from mipidsi 0.7 for the same panel).

## Hardware test checklist

1. Format an SD card as FAT32, insert it, then run `flash`.
2. Confirm the splash, main menu, rat animation, and full keyboard navigation.
3. Open **WI-FI → SCAN NETWORKS** and confirm visible SSIDs and RSSI values appear.
4. Select a WPA/WPA2 network, type its password, and press Enter.
5. Confirm `CONNECTED: <SSID>` and `/RATPUTER/WIFI.CFG` on the SD card.
6. Reboot, open **SAVED NETWORKS**, and connect without re-entering the password.
7. Press Fn+Backspace on the saved entry and confirm it is removed from the TOML.
8. Test an incorrect password, an open network, no SD card, and an empty scan.
9. Open **USB DISK** and confirm that the computer mounts `RATPUTER SD`; read and
   write a test file, eject it on the host, then press Backspace and verify that
   `WIFI.CFG` is reloaded.
10. Top bar: after connecting, the clock switches from `--:--` to local time within a
   few seconds and the bars turn bright; power off the AP and confirm `OFFLINE` while
   the clock keeps counting; compare the battery % against the charge level.

A successful host build verifies compilation and image headers, but radio, antenna,
SD-card compatibility, and internal-SRAM headroom require this physical test.

## Resources

- [esp-hal docs — ESP32-S3](https://docs.espressif.com/projects/rust/esp-hal/latest/esp32s3/esp_hal/)
- [Rust on ESP Book](https://docs.espressif.com/projects/rust/book/)
- [Cardputer ADV — M5 docs](https://docs.m5stack.com/en/core/Cardputer-Adv)
- [espflash CLI](https://github.com/esp-rs/espflash)

## License

MIT OR Apache-2.0
