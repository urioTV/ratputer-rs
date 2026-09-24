# AGENTS.md — Guide for LLM agents working on ratputer-rs

Firmware for the **M5Stack Cardputer ADV** (ESP32-S3FN8 / Stamp-S3A): a **no_std** Rust
firmware with a **Slint 1.18** UI (software renderer), pixel-art animation, a TCA8418
keyboard, Wi-Fi (esp-radio), and an SD card (hadris-fat + embedded-sdmmc block
driver) for credentials. This file
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
src/storage.rs     hadris-fat volume + toml/serde: /RATPUTER/WIFI.CFG (max 12) + [ftp]
src/usbdisk.rs     embassy-usb device setup, executor-less polling, USB-OTG PHY switching
src/msc.rs         pure-Rust MSC Bulk-Only Transport + SCSI class over an SD BlockDevice
src/ftp.rs         passive FTP server: control/data sessions + FAT file operations
src/net.rs         embassy-net stack (DHCP/DNS/UDP/TCP) + one-shot SNTP, no executor
src/clock.rs       WallClock (last SNTP sync + monotonic elapsed), UTC offset + EU DST
src/battery.rs     GPIO10/ADC1 battery gauge (2:1 divider, curve calibration, Li-ion %)
ui/ratputer.slint  All UI (splash → menu/rat/Wi-Fi/password/USB/FTP/about)
ui/images/, ui/fonts/  pixel-art frames + Press Start 2P (OFL)
src/sdblock.rs     seekable first-partition adapter: MBR translate + sector RMW
build.rs           compiles Slint resources
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
- Main-loop order matters: input → scheduled actions/step executor → USB poll/actions →
  network poll + top-bar refresh → animation ticker → **draw at the BOTTOM of the
  iteration**. This makes the `wifi-connecting` spinner
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

## Wi-Fi (esp-radio 1.0.0-beta.1) + SD (hadris-fat 2.4 over embedded-sdmmc 0.10 blocks)

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
- SD: dedicated SPI3, SCLK=G40 MOSI=G14 MISO=G39 CS=G12. Initialize at **400 kHz**,
  call `get_card_type()` to complete identification, then use `SdCard::spi` →
  `ExclusiveDevice::bus_mut()` → `Spi::apply_config` to switch to **20 MHz**.
  `embedded_sdmmc::SdCard::new(SpiDevice,...)` is the block driver only; the FAT
  volume is `hadris_fat::sync::FatVolume<SdBlockDevice>` (see `storage::mount`),
  which owns the card until `storage::free()` hands it to USB MSC. Files:
  `RATPUTER/WIFI.CFG`; writes commit with `FileWriter::finish()`.
- Watches on memory: Wi-Fi init allocs ~tens of KB from the 150 KB heap. If you grow
  the heap, re-verify on hardware; every `build`/`flash` is the only test we have.

## USB Mass Storage (pure Rust)

- `esp-hal` 1.2 exposes ESP32-S3 USB-OTG through `embassy-usb-driver`, but
  `embassy-usb` has no **device-side MSC class** (do not confuse with
  `embassy-usb-host::class::msc`; `usbd-storage` needs the incompatible
  `usb-device::UsbBus`). `src/msc.rs` is our own BOT + SCSI class: CBW/CSW
  validation, GET_MAX_LUN / BULK_ONLY_RESET, INQUIRY (+VPD 0/80/83), TEST UNIT
  READY, REQUEST SENSE, MODE SENSE 6/10, READ CAPACITY 10/16, READ FORMAT
  CAPACITIES, REPORT LUNS, READ/WRITE/VERIFY 10, START STOP UNIT, PREVENT/ALLOW,
  SYNCHRONIZE CACHE. Unknown opcodes fail with ILLEGAL REQUEST sense.
- The device is single-function (class 0, no IADs) so Windows binds `usbstor`
  directly. `embassy-usb` is built with `max-handler-count-1`/`max-interface-count-1`
  and without `usbd-hid`.
- The DWC2 FIFO is serviced by esp-hal's real USB interrupt; there is still no
  executor. `UsbDisk::poll()` polls the `join(device.run(), msc.run())` future with a
  waker that sets a static flag, and keeps re-polling while the interrupt reports
  progress for up to 40 ms per main-loop pass. **Do not go back to one poll per loop
  with `Waker::noop()`**: every 64-byte bulk packet needs a poll, so Windows I/O
  would time out and Explorer would hang.
- **Never call `BlockDevice::num_blocks()` per SCSI command**: embedded-sdmmc re-reads
  the CSD over SPI every time. The capacity is cached in `SharedState` at attach.
- Read cache (`BlockCache` in `src/msc.rs`): 3 LRU lines + 1 stream line, 8 blocks
  each, one `static` (16 KiB .bss, not heap; it shrinks `.stack` from ~114 to ~98 KiB).
  Misses read an aligned line via CMD18; READ(10) larger than one line uses the
  stream line only. Writes are chunked into the stream line and written with CMD25
  before the CSW, then patched into any cached LRU copy. The cache is invalidated on
  every attach (`SharedState::generation`) because the firmware may have rewritten
  `WIFI.CFG` in between. **Do not add write-back caching**: Windows treats the
  device as quick-removal and the forced second EXIT would lose acknowledged writes.
- The USB DISK stats line is refreshed at 2 Hz only; every Slint redraw pauses USB
  servicing for the duration of the SPI frame.
- The USB task is created once (descriptor/endpoint buffers are leaked `Box`es) and
  kept across USB DISK sessions; `detach()` only clears the backend and swaps the
  PHY back. Re-attaching relies on the host's bus reset to resynchronise BOT.
- USB-OTG and USB-Serial-JTAG share the ESP32-S3 PHY. Delay OTG PHY selection until
  the user opens USB DISK, otherwise espflash monitor disappears at boot. On exit,
  select Serial/JTAG again; the host port re-enumerates. Native pins are fixed:
  D−=GPIO19, D+=GPIO20.
- The SD card has **exactly one owner**. Entering USB DISK consumes
  `VolumeManager::free()`, boxes the raw `SdCard` at a stable address and registers
  the sector backend. Exit switches the PHY away and clears that pointer, then
  reconstructs `VolumeManager` to invalidate its FAT cache and reloads `WIFI.CFG`.
  Never let filesystem methods run while MSC owns the card.
- Host eject is observed through SCSI START STOP UNIT. Normal exit is rejected while
  the host is mounted. A second EXIT forces disconnect because forced B-valid means
  cable removal cannot always be detected; this is recovery-only and can lose host
  writes. Block writes themselves are synchronous; SYNCHRONIZE CACHE succeeds.
- Development descriptors use VID:PID `CAFE:4002`; obtain real identifiers before
  product distribution.
- USB sector traffic uses the 20 MHz post-init SPI clock. Never construct the card
  at 20 MHz: identification must remain ≤400 kHz, and only `apply_config` after a
  successful `get_card_type()` may raise it. The default-speed SD limit is 25 MHz.
  SPI3 has no IOMUX pins on the S3, so the bus goes through the GPIO matrix; 20 MHz
  keeps margin for MISO sampling. If a card shows read errors (`C` stays low, host
  I/O errors), fall back to 10 MHz before suspecting the MSC code.

## FTP server

- `src/ftp.rs` is a single-client FTP server on `embassy-net` TCP. It is active
  **only** while view 8 is open. Control = port 21, fixed passive data = port 50000;
  `PASV`/`EPSV` only, no active `PORT`/`EPRT`, anonymous access or TLS. Default
  login is `rat` / `cheese`; `[ftp]` in `WIFI.CFG` persists it and Tab on the FTP
  screen opens the password editor. Credentials and data are plaintext: LAN only.
- Supported file operations: LIST/NLST/MLSD/MLST, PWD/CWD/CDUP, SIZE/MDTM,
  RETR (+ REST), STOR, DELE, MKD, empty RMD, RNFR/RNTO and ABOR. `LIST -a`/`-la`
  works. Full VFAT long filenames (255 UTF-16 units) for read AND write are
  provided by `hadris-fat` 2.4, which generates the 8.3 alias itself.
- FAT implementation: `hadris-fat` with features read/write/lfn/alloc/sync and
  NO `cache` feature (its cache is write-back; this project is write-through).
  `src/sdblock.rs` adapts the raw `embedded_sdmmc::BlockDevice` to
  `embedded_io` 0.7 with MBR partition translation and sub-sector
  read-modify-write (max 16 blocks per card command). `embedded-sdmmc` remains
  only as the SD/BlockDevice driver; its VolumeManager is unused.
- `hadris-fat` handles delete-empty-dir validation, cluster-chain freeing,
  FSInfo updates, LFN runs across cluster boundaries, and stale-handle
  revalidation. Do not reintroduce hand-written FAT entry patching.
- Open FAT handles (`FatDir`, `FileReader`, `FileWriter`) borrow the volume,
  so FTP creates/uses/drops them within one poll step. Transfer state kept
  between polls is only paths + byte offsets (see src/ftp.rs header).
- The FTP screen owns one raw volume for its lifetime. Stop FTP and close all
  RawFile/RawDirectory handles before USB MSC can call `VolumeManager::free()`.
  `main.rs` enforces FTP↔USB exclusion and defensively stops FTP on navigation.
- There is no executor: `TcpSocket::accept/read/write` futures are polled once
  with `Waker::noop()` only when socket readiness says they can progress. During
  transfers, main burst-polls `Network::poll_stack()` + `FtpServer::poll()` for
  up to 15 ms, then returns to input/rendering. Do not consume `Network::poll()`
  inside the burst or an SNTP completion result will be lost.
- `Network` uses `StackResources<8>` for DHCP/DNS/SNTP plus two persistent FTP
  sockets. FTP allocates 1 KiB RX/TX control + 4 KiB RX/TX data buffers once and
  refuses first creation when heap free is below 32 KiB. UI refresh is 2 Hz.
- FAT timestamps use `storage::FatClock`, backed by the SNTP-derived local time;
  before sync they fall back to 2026-01-01. FTP uploads flush on `close_file` before
  the 226 reply. Do not add write-back caching.

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
