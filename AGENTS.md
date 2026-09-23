# AGENTS.md — Guide for LLM agents working on ratputer-rs

Firmware for the **M5Stack Cardputer ADV** (ESP32-S3FN8 / Stamp-S3A): a **no_std** Rust
firmware with a **Slint 1.18** UI (software renderer), pixel-art animation, and a TCA8418
keyboard driver. This file documents hard-won, project-specific knowledge so that future
work does not rediscover the same pitfalls.

---

## Golden rules

1. **Build & flash are the only verification you have.** There is no emulator. After any
   UI change, run `nix develop -c bash -c './build-bin.sh'`, then have the user flash
   with `espflash write-bin 0x0 ratputer-adv.bin --verify`. Always verify the merged
   image headers afterwards: `0x0` = `e9`, `0x8000` = `aa 50`, `0x10000` = `e9`.
2. **Never raise SPI above 40 MHz.** See "LCD quirks" — 80 MHz visibly scrambles the
   picture.
3. **The ADV has no PSRAM.** All heap lives in internal RAM. Before touching allocation,
   read "Memory".
4. Keep comments/docstrings in **Polish** (project convention), `README.md` in Polish,
   code in Rust idioms. UI strings are ASCII-only (pixel font has no Polish glyphs).

## Repository layout

```
src/main.rs        esp-hal init, LCD (ST7789), Slint platform, frame clock, kbd loop
src/keyboard.rs    TCA8418RTWR driver (I2C0 @ 0x34) → NavKey events
ui/ratputer.slint  All of the UI (splash → mainscreen: menu / rat-view / about)
ui/images/         rat0..3.png — pixel-art frames (32×20, displayed at ×4 = 128×80)
ui/fonts/          Press Start 2P (OFL) — THE UI font; DejaVu was removed
build.rs           slint-build: compiles ui/ratputer.slint, embeds fonts+images
flake.nix, rust-toolchain.toml, .cargo/config.toml — toolchain wiring
build-bin.sh       builds release + produces merged ratputer-adv.bin from 0x0
```

## Toolchain

- Rust fork **"esp"** installed once via `espup install` (NOT from nixpkgs). The fork is
  a *nightly*; check `rust-toolchain.toml` (`channel = "esp"`). Dependencies must be
  compatible with `-Zbuild-std` (`[unstable] build-std = ["core", "alloc"]` in
  `.cargo/config.toml`) and the `xtensa-esp32s3-none-elf` target
  (**⚠ without the `unknown` token** — the esp fork uses the short triple).
- On NixOS the forked rustc needs `programs.nix-ld.enable = true` (already configured
  on this host).
- `nix develop` provides `rustup`, `espup` and `espflash` and sources
  `~/export-esp.sh` (paths to the forked Xtensa GCC).
- If the firmware reports nothing on the LCD but the build worked, the most common
  cause is a **forgotten `display_offset`** or wrong `display_size` order — the ST7789
  panel is a 135×240 *window* inside a 240×320 GRAM. Correct, verified values:
  `display_size(135, 240)` + `display_offset(52, 40)` + `Rotation::Deg90` (landscape
  240×135). Documented in README "Geometria panelu".

## LCD quirks (ST7789)

- **SPI clock = `Rate::from_mhz(40)`** — reference (espp `m5stack-cardputer.hpp`) value.
  The LCD pins (G33–G38) are NOT native IOMUX SPI2 pins, so signals route through the
  GPIO matrix; at 80 MHz ST7789 setup times are violated and the picture gets
  scrambled/stretched differently per frame.
- Color setup that works: `ColorInversion::Inverted`, RGB565, no explicit color-order
  swap for our panel.

## Memory (no PSRAM!)

- Call `esp_alloc::heap_allocator!(size: 150 * 1024);` **inside `main()`** (esp-alloc
  0.11 macro is a statement, not a global item) BEFORE the first `Box`/`Rc`.
- `MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer)` + `render_by_line` +
  a `[Rgb565Pixel; 240]` line buffer — only one line of frame data lives in RAM;
  mipidsi streams it via `Display::set_pixels()`.
- Fonts/images become flash-resident data via
  `slint_build` `embed_resources(EmbedForSoftwareRenderer)`.

## Slint specifics (1.18, no_std)

- Custom fonts are imported in `.slint` files: `import "fonts/X.ttf";` —
  `CompilerConfiguration::with_bundled_fonts` **does not exist** in 1.18.
- The root `Window` **must** set `default-font-family` to an imported family or the
  compiler fails with an "internal error: could not determine a default font".
- `padding-*` works only on layout elements, not on `Text`.
- Font is **Press Start 2P** — readable at 8 px multiples (UI: 8 px body, 12 px titles).
  It has **no Polish diacritics** — all UI strings stay ASCII.
- Keyboard input is delivered via a root callback `key-pressed(string)`; Rust side
  dispatches `"tab" | "enter" | "back" | "space" | "up" | "down" | "left" | "right"`.
  **Do not** reintroduce `FocusScope` chain — the callback design keeps nav logic in
  one place.

## Keyboard (TCA8418)

- I²C0 @ 400 kHz, SDA=G8, SCL=G9, 7-bit address **0x34**, INT=G11 (unused — we poll).
- FIFO: read `KEY_LCK_EC` (0x03, low nibble = event count), pop with
  `KEY_EVENT_A` (0x04). `bit7 = pressed`, low 7 bits = 1-based key code with
  10 columns/row. `idx = code - (code/10)*2 - 1` maps into the 7×8 matrix
  (see `src/keyboard.rs` constants; full map in crate `cardputer` 0.2).
- Arrow keys are the physical `,` `;` `.` `/` keys (idx 43/46/47/51) — the Fn layer
  that produces punctuation is **software**, TCA8418 is unaware of it.

## Flashing

```sh
nix develop
./build-bin.sh                                   # → ratputer-adv.bin
espflash write-bin 0x0 ratputer-adv.bin --verify # merged image from 0x0
```

Boot mode (when the port is stubborn): hold G0 while plugging USB-C.

## Host-side verification tricks (no hardware needed)

- Copy the `RAT_FRAMES`/pixel pipeline into a throwaway crate and render with
  embedded-graphics into a hand-rolled `DrawTarget` — print the framebuffer as ASCII
  to check art alignment (`/tmp/egtest` pattern).
- Implement the mipidsi `Interface` trait on a recorder to make sure address windows
  (CASET/RASET/MADCTL) are as expected on real hardware.

## Commit hygiene

- Conventional Commits in English (`feat:`, `fix:`, …) — see git history.
- **Never commit** `target/`, the merged `*.bin`, or toolchain shims (`export-esp.sh`);
  these are pinned in `.gitignore`. `Cargo.lock` and `flake.lock` ARE committed
  (reproducible build).
