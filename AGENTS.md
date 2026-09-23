# AGENTS.md — Guide for LLM agents working on ratputer-rs

Firmware for the **M5Stack Cardputer ADV** (ESP32-S3FN8 / Stamp-S3A): a **no_std** Rust
firmware with a **Slint 1.18** UI (software renderer), pixel-art animation, a TCA8418
keyboard, Wi-Fi (esp-radio), and an SD card (embedded-sdmmc) for credentials. This file
documents hard-won, project-specific knowledge so that future work does not rediscover
the same pitfalls.

---

## Golden rules

1. **Build & flash are the only verification you have.** There is no emulator. After any
   UI change, run `nix develop -c build`, then have the user flash with
   `espflash write-bin 0x0 ratputer-adv.bin` (writes are verified by default). The
   xtask verifies the merged image headers automatically: `0x0` = `e9`, `0x2` = `02`
   (DIO), `0x8000` = `aa 50`, `0x10000` = `e9`, `0x10002` = `02` (DIO).
2. **Never raise SPI above 40 MHz.** See "LCD quirks" — 80 MHz visibly scrambles the
   picture.
3. **The ADV has no PSRAM.** All heap lives in internal RAM. Before touching allocation,
   read "Memory".
4. Write **everything in English** — code comments, `README.md`, docstrings, commit
   messages. UI strings are ASCII-only (the pixel font has no non-ASCII glyphs).

## Repository layout

```
src/main.rs        esp-hal init, LCD, SD SPI3, esp-rtos, Slint platform, main loop/state
src/wifi.rs        esp-radio 1.0.0-beta.1 wrapper: scan (max 8) + connect (blocking block_on)
src/storage.rs     embedded-sdmmc + toml/serde: /RATPUTER/WIFI.CFG credentials (max 12) + [clock]
src/net.rs         embassy-net stack (DHCP/DNS/UDP) + one-shot SNTP job, polled without executor
src/clock.rs       WallClock (last SNTP sync + monotonic elapsed), UTC offset + EU DST
src/battery.rs     GPIO10/ADC1 battery gauge (2:1 divider, curve calibration, Li-ion %)
ui/ratputer.slint  All UI (splash → menu/rat/wifi-menu/saved/scan/password/about)
ui/images/, ui/fonts/  pixel-art frames + Press Start 2P (OFL)
build.rs           slint-build: compiles ui/ratputer.slint, embeds fonts+images
xtask/             host helper: builds release, creates and verifies merged binary
flake.nix, rust-toolchain.toml, .cargo/config.toml — toolchain wiring
```

## Toolchain

- The **"esp"** Rust fork is supplied by the pinned `esp-rs-nix` flake input. The
  devshell sets `RUSTUP_TOOLCHAIN` to its immutable Nix store path; do not run
  `espup install` or source `~/export-esp.sh`.
- The fork is a *nightly*. Dependencies must be compatible with `-Zbuild-std`
  (`[unstable] build-std = ["core", "alloc"]` in `.cargo/config.toml`) and the
  `xtensa-esp32s3-none-elf` target (**without the `unknown` token** — the esp fork
  uses the short triple).
- `nix develop` provides the complete Xtensa toolchain and `espflash`.
- If the firmware reports nothing on the LCD but the build worked, the most common
  cause is a **forgotten `display_offset`** or wrong `display_size` order — the ST7789
  panel is a 135×240 *window* inside a 240×320 GRAM. Correct, verified values:
  `display_size(135, 240)` + `display_offset(52, 40)` + `Rotation::Deg90` (landscape
  240×135). Documented in README "Panel geometry".

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
  dispatches `"tab" | "enter" | "back" | "space" | "up" | "down" | "left" | "right"`
  plus `"delete"`. **Do not** reintroduce `FocusScope` chain — the callback design keeps
  nav logic in one place.
- Rust-side Wi-Fi commands flow through `wifi-action`/`wifi-action-index` (in-out ints).
  Actions only SCHEDULE work (`radio_pending: Option<RadioStep>`); the actual radio call
  (one scan pass / one connect attempt) runs in a later loop iteration in
  `RadioStep { Scan, Connect }` steps. This keeps `busy_status()` (spinner + progress)
  rendering between the blocking steps and retried attempts. Keep this step-machine
  pattern for any new slow operation. While `radio_pending.is_some()`, keyboard input
  is drained and discarded — otherwise queued keys replay after the radio unblocks.
  The same drain-and-ignore guard covers the splash screen.
- Main-loop order matters: input → scheduled actions/step executor → network poll +
  top-bar refresh → animation ticker → **draw at the BOTTOM of the iteration**. This makes the `wifi-connecting` spinner
  pop-up appear in the same frame as the Enter press, BEFORE the first blocking radio
  call; a step scheduled in iteration N executes at iteration N+1. Do not move the
  draw back to the top, or every connect/scan will look like an input lag.
- No arrow buttons exist in Slint 1.18 `visible`/`if` alternation used for empty-list
  hints; lists are plain `for item[i] in model` renderers against `VecModel<SharedString>`
  set from Rust via `ModelRc`. SSIDs longer than the screen are truncated to 18 chars.
- The top bar takes the first 12 px of `mainscreen`; every view lives inside
  `content` (y = 12 px, 123 px tall). New views go inside `content`, not next to it.
- Saved/scan lists are **clipped viewports** (`saved-list`/`scan-list`): the row
  column slides by `first * 10px` and rows outside `first..first+rows` are hidden,
  otherwise a half row bleeds over the status line. Keep 10 px rows or update `rows`.
- Press Start 2P has no `…` glyph, so `overflow: elide` is useless — truncate in Rust
  (`truncate_ascii`) and use `overflow: clip`.

## Keyboard (TCA8418)

- Use the **`cardputer-adv-keyboard` 0.2.6 crate** (wrapper `Keyboard::new(i2c)` +
  `inputs()`) — it resolves the full ADV keymap including Shift/Fn layers. The old
  hand-rolled `src/keyboard.rs` was only good for nav keys and was removed.
- I²C0 @ 400 kHz, SDA=G8, SCL=G9, 7-bit address **0x34**, INT=G11 (unused — we poll).
- Arrow keys: bare `,` `;` `.` `/` = Left/Up/Down/Right (legacy behavior, restored in
  main.rs as Char remaps outside password entry); Fn+those are also arrows per the
  crate's Fn layer (Fn+, → Left, Fn+; → Up, Fn+. → Down, Fn+/ → Right). In password
  entry chars stay punctuation. Enter connects; Escape (Fn+backtick) is "back";
  Delete (Fn+Backspace) forgets a saved network.

## Wi-Fi (esp-radio 1.0.0-beta.1) + SD (embedded-sdmmc 0.10)

- **`esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0)` MUST run before
  `WifiController::new`**; the internal Wi-Fi tasks/timers depend on it. We do NOT use
  embassy executor — `scan_async`/`connect_async` are driven with
  `embassy_futures::block_on` (esp-rtos provides the polling hooks).
- API shape: `WifiController::new(peripherals.WIFI, ControllerConfig::default())` then
  `Interface::station()` (one-shot singleton). `set_config(&Config::Station(...))` also
  (re)starts the radio; `connect_async()` returns `Result<_, ConnectionError>` (NOT
  WifiError) and `is_connected()` is unstable/private — call `disconnect_async()` and
  ignore `NotConnected`.
- Scan results use `ScanConfig` + `AccessPointInfo` (ssid/signal_strength/auth_method).
  Default active dwell (10–20 ms/channel) misses APs — we scan with
  `ScanTypeConfig::Active{min:50ms,max:250ms}` and merge 2 passes in `src/wifi.rs`.
  Association is flaky; `connect()` retries 3× (disconnect between attempts).
  Station auth is `AuthenticationMethodConfig`
  (Open/Wep/Wpa/Wpa2Personal/WpaWpa2Personal) — WPA3-only APs are marked unsupported.
- SD: dedicated SPI3, SCLK=G40 MOSI=G14 MISO=G39 CS=G12 @ **400 kHz** (proper card
  init speed; file is a few KB anyway). `embedded_sdmmc::SdCard::new(SpiDevice,...)` +
  `VolumeManager` (RefCell inside → all methods `&self`). Files: 8.3 uppercase FAT
  names (`RATPUTER/WIFI.CFG`), `embedded_io::Write` + `flush` required.
- Watches on memory: Wi-Fi init allocs ~tens of KB from the 150 KB heap. If you grow
  the heap, re-verify on hardware; every `build`/`flash` is the only test we have.

## Network, clock, battery (top bar)

- `WifiManager::new` returns `(manager, Interface)`; the station `Interface` implements
  `embassy-net-driver` 0.2 and is moved into `embassy_net::new` (see `src/net.rs`).
  Versions must line up: `embassy-net` 0.8 ↔ driver 0.2 ↔ `embassy-time` 0.5 ↔
  esp-rtos 0.4 (`embassy-time-driver` 0.2).
- **esp-rtos needs the `embassy` feature**: it registers the embassy-time driver that
  embassy-net's internal timers (DHCP, DNS, `with_timeout`) rely on. Without it, the
  link fails with `undefined reference to _embassy_time_now` / `_embassy_time_schedule_wake`.
- There is still **no executor**: `Network::poll()` polls the stack runner and the
  SNTP job once per loop with `Waker::noop()`. This REQUIRES
  `embassy-time-queue-utils` with a `generic-queue-*` feature (set in `Cargo.toml`).
  Otherwise esp-rtos gets the executor-integrated timer queue, whose `schedule_wake`
  unwraps `try_task_from_waker` and panics ("Found waker not created by the Embassy
  executor") on the first embassy-net timer, i.e. in the first loop iteration, before
  anything is drawn: symptom = **backlight on, black screen**. Check with
  `strings <elf> | grep "Found waker not created"` (must be empty). Never `block_on` network futures —
  DHCP/DNS take seconds and would freeze the UI. Stack resources and the runner are
  `Box::leak`ed to get `'static`.
- Link state comes from esp-radio's `station_state() == Connected` via the driver, so
  `stack.is_link_up()` detects drops; `is_config_up()` = DHCP lease. Top-bar
  `link-state`: 0 offline, 1 connecting (a `RadioStep::Connect` is pending),
  2 associated/no IP, 3 online.
- SNTP: resolve `clock.ntp_server` (4 s timeout, fallback 162.159.200.1), send a
  48-byte mode-3 request, accept only a mode-4, non-zero-stratum reply from the same
  endpoint; overall 8 s timeout. Sync on every fresh association, then hourly;
  retry after 30 s on failure. `WallClock` = last sync + monotonic elapsed, so time
  survives a lost link (not a reboot — no RTC battery).
- Local time: `utc_offset_minutes` + optional EU DST computed in `src/clock.rs`
  (Hinnant civil-date algorithms, verified host-side against the 2024–2027 switch
  dates). No chrono/tz database — keep it that way for flash size.
- Battery: **GPIO10 = ADC1_CH9**, 100k/100k divider. Use ADC1 only — ADC2 is unusable
  while Wi-Fi runs. `AdcCalCurve<ADC1>` returns millivolts at the pin; multiply by 2.
  Sampled every 5 s (8 reads + 1/4 exponential smoothing).

## Flashing

- The merged image must use **DIO flash mode**. A QIO header makes the ESP32-S3 ROM
  load only the first bootloader segment and then reset with `ets_loader.c 78` /
  `TG0WDT_SYS_RST`, before either the second-stage bootloader or application starts.
- Flash frequency remains 80 MHz; this is independent of the LCD's 40 MHz SPI limit.

```sh
nix develop
build # → ratputer-adv.bin
flash # rebuild, flash from 0x0, and verify
```

Boot mode (when the port is stubborn): hold G0 while plugging USB-C.

## Host-side verification tricks (no hardware needed)

- Copy the `RAT_FRAMES`/pixel pipeline into a throwaway crate and render with
  embedded-graphics into a hand-rolled `DrawTarget` — print the framebuffer as ASCII
  to check art alignment (`/tmp/egtest` pattern).
- Implement the mipidsi `Interface` trait on a recorder to make sure address windows
  (CASET/RASET/MADCTL) are as expected on real hardware.
- **Render the real UI on the host** (`/tmp/slintpreview` pattern): a throwaway std
  binary that compiles `ui/ratputer.slint` with the SAME no_std Slint features as the
  firmware (`compat-1-2`, `unsafe-single-threaded`, `libm`, `renderer-software` — the
  `std` feature pulls fontconfig and fails in the Nix shell), a `Platform` with a fake
  clock (advance it so the splash crossfade finishes), `MinimalSoftwareWindow::render`
  into a 240×135 `Rgb565Pixel` buffer, then dump PPM → PNG and inspect it. This caught
  clipped popups and half-visible list rows that the compiler cannot.

## Commit hygiene

- Conventional Commits in English (`feat:`, `fix:`, …) — see git history.
- **Never commit** `target/`, the merged `*.bin`, or toolchain shims (`export-esp.sh`);
  these are pinned in `.gitignore`. `Cargo.lock` and `flake.lock` ARE committed
  (reproducible build).
