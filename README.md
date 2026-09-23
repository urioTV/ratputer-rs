# Ratputer RS — Cardputer ADV firmware in Rust

Firmware for the **M5Stack Cardputer ADV** (ESP32-S3FN8 / Stamp-S3A) written in Rust.
Contents: welcome splash with an animated pixel-art rat on a 240×135 ST7789V2 LCD,
a nav-menu UI, full keyboard support, everything in **Slint (no_std)**. 🐀

## Stack

| Layer | Choice |
|---|---|
| Chip | ESP32-S3 (Xtensa LX7) → target `xtensa-esp32s3-none-elf` |
| Framework | `esp-hal` 1.2 (no_std, bare-metal, safe API) |
| UI | **Slint 1.18** (`renderer-software`, `unsafe-single-threaded`, `libm`) |
| Memory | `esp-alloc` 0.11 — 150 KB heap in **internal SRAM** (the ADV has no PSRAM!) |
| Display | Slint software renderer → `LineBufferProvider` per line → SPI, `mipidsi` 0.9 (ST7789) |
| Fonts | **Press Start 2P** (OFL, pixel grid 8px) — `import "fonts/PressStart2P-Regular.ttf"` in .slint |
| Toolchain | **"esp"** Rust fork (installed once via `espup`) |

> Previous font picks were DejaVu Sans/Mono — weak readability at 6–8 px; the pixel font fits the art natively.

## UI Slint: structure

- `build.rs` compiles `ui/ratputer.slint` with `EmbedForSoftwareRenderer`: font
  files and images land in flash;
- The template defines **two top-level screens** driven by the `splash-done` property
  (Slint `states` with `animate opacity { duration: 600ms; easing }`) plus **three
  mainscreen views** (`in-out property <int> view-state`: 0=menu, 1=rat view,
  2=about) and a **`key-pressed(string)`** callback:
  - **splash** — animated pixel-art rat (`ui/images/rat0..3.png` scaled ×4 =
    128×80 px; frames: bob / step / blink / tail-up) + header `RATPUTER · BOOTING`
    + footer `MODE STALKING/SNIFFING/HUNTING/LOITERING` with a spinner;
  - **menu** — item highlight (*menu-index*) on a greenish strip; keyboard:
    **↑↓ / ←→** = choose (Tab also works), **Enter/Space** = enter,
    **Backspace** = back;
  - **rat view** (`view-state == 1`) — the same animated pixel-art as the splash
    (frames keep cycling while the view is active);
  - **about view** (`view-state == 2`) — technical info;
- The firmware (`src/main.rs`) polls the TCA8418 FIFO and sends readable key names
  (`"tab" | "enter" | "back" | "space" | "up" | "down" | "left" | "right"`); the nav
  logic lives entirely in the .slint callback, so a state change is a one-liner from
  Rust (`set_splash_done`, `view-state`, `menu-index`);
- Main loop: `update_timers_and_animations()` → `draw_if_needed(render_by_line)` →
  every **250 ms** `ui.set_frame_index(i)` (in the splash **and** in the rat view),
  after `SPLASH_AFTER_MS` → `ui.set_splash_done(true)`;

### Keyboard — TCA8418RTWR via I2C0

`src/keyboard.rs` — pops events from the FIFO (read event count from
`KEY_LCK_EC`=0x03, pop with `KEY_EVENT_A`=0x04), decodes bit7 as "pressed" and maps
the 1-based TCA8418 code to the 7×8 matrix index used by the ADV (Tab=1,
Backspace=52, Enter=54, Space=55, **arrows ←=43 / ↑=46 / ↓=47 / →=51**; the Fn layer
that turns those into `,` `;` `.` `/` is software-level and not used here). Register
set and sequence follow the `cardputer` 0.2 crate (cardputer-adv).

### Memory (ADV has no PSRAM!)

- Heap 150 KB in **internal SRAM** (`esp_alloc::heap_allocator!(size: 150*1024);`
  **called inside `main()` before any Box/Rc**); rest of the memory is `.bss`/stack;
- No persistent framebuffer — Slint in `ReusedBuffer` mode only holds **one raster
  line** (240 Rgb565 ≈ 480 B) and pushes it via `LineBufferProvider` →
  `mipidsi::Display::set_pixels(...)`;
- Fonts and images are packaged into flash by `build.rs` (embed resources);
- Previous version: 4 ASCII-art frames (by Gio) — in git history.

## Setup (once)

The Nix devshell provides `rustup`, `espup`, `espflash` and sources `export-esp.sh`
(paths to the forked Xtensa GCC).

The Xtensa toolchain is installed once, globally (the fork is not in nixpkgs):

```bash
nix develop
espup install          # ~1.2 GB into ~/.rustup/toolchains/esp
```

> **NixOS:** the espup-forked rustc needs a dynamic linker — add
> `programs.nix-ld.enable = true;` to your NixOS config (already enabled on
> the original dev host).

> The build is verified: `cargo build --release` passes
> (ELF in `target/xtensa-esp32s3-none-elf/release/`; app image ~467 KB —
  mostly the Slint software renderer + the bundled font).

## Build & flash

### Simplest: merged .bin (one command)

```bash
nix develop
./build-bin.sh                      # build + merged ratputer-adv.bin (~520 KB)
```

The image contains everything: bootloader @0x0 + partition table @0x8000 + app @0x10000.
Header per the M5Stack StampS3A module spec: **8 MB, QIO, 80 MHz** (verified byte-wise).

```bash
espflash write-bin 0x0 ratputer-adv.bin --verify
# or esptool:
esptool --chip esp32s3 write_flash 0x0 ratputer-adv.bin
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

## Keyboard (details)

Cardputer ADV's keyboard hangs off a **TCA8418** over I²C (G8=SDA, G9=SCL, G11=INT —
not used; we poll the FIFO). Reference driver: `cardputer` crate 0.2 (`src/adv/keyboard.rs`),
also `cardputer-adv-keyboard` (LarsBollmann) — embedded-hal 1.0 compatible.

## Resources

- [esp-hal docs — ESP32-S3](https://docs.espressif.com/projects/rust/esp-hal/latest/esp32s3/esp_hal/)
- [Rust on ESP Book](https://docs.espressif.com/projects/rust/book/)
- [Cardputer ADV — M5 docs](https://docs.m5stack.com/en/core/Cardputer-Adv)
- [espflash CLI](https://github.com/esp-rs/espflash)

## License

MIT OR Apache-2.0
