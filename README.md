# Ratputer RS — Cardputer ADV Firmware w Rust

Firmware dla **M5Stack Cardputer ADV** (ESP32-S3FN8 / Stamp-S3A) napisany w Rust.
Demo: animowany ASCII Szczur na ekranie ST7789V2 (240×135) — w pełnym **UI Slint** 🐀

## Stack

| Warstwa | Wybór |
|---|---|
| Chip | ESP32-S3 (Xtensa LX7) → target `xtensa-esp32s3-none-elf` |
| Framework | `esp-hal` 1.2 (no_std, bare-metal, safe API) |
| UI | **Slint 1.18** (`renderer-software`, `unsafe-single-threaded`, `libm`) |
| Pamięć | `esp-alloc` 0.11 — heap 150 KB w **RAM wewnętrznym** (ADV nie ma PSRAM!) |
| Ekran | renderer software Slint → `LineBufferProvider` po liniach → SPI, `mipidsi` 0.9 (ST7789) |
| Fonty | **Press Start 2P** (OFL, pixel-style 8px) — `import "fonts/PressStart2P-Regular.ttf"` |
> Poprzednio: DejaVu Sans/Mono (słaba czytelność przy 6–8 px); teraz font pixelowy
| Toolchain | fork Rusta **"esp"** (instaluje `espup`) |

## UI Slint: struktura

- `build.rs` kompiluje `ui/ratputer.slint` z embedowaniem zasobów
  (`EmbedForSoftwareRenderer`) — fonty i obrazki pakowane są do flashu;
- szablon definiuje **dwa nadrzędne ekrany** sterowane własnością **splash-done**
  (Slint `states` z `animate opacity { duration: 600ms; easing }`) oraz
  **trzy widoki mainscreen** (`in-out property <int> view-state`: 0=menu,
  1=podgląd szczura, 2=o projekcie) + callback **`key-pressed(string)`**:
  - **splash** — animowany pixel-art szczur (`ui/images/rat0..3.png` skalowane ×4
    = 128×80 px; klatki: bob / krok / mrugnięcie / ogon w górę) + nagłówek
    `RATPUTER · BOOTING` + stopka `MODE STALKING/SNIFFING/HUNTING/LOITERING`
    z spinnerem;
  - **menu** — zaznaczenie pozycji *(menu-index)* na zielonawym tle;
    obsługuje klawiaturę: **↑↓ / ←→** = wybór (działa też **Tab**),
    **Enter/Spacja** = wejście, **Backspace** = powrót;
  - **widok szczura** (`view-state == 1`) — ten sam animowany pixel-art co splash
    (klatka nadal jest cyklowana przez firmware, dopóki widok jest aktywny);
  - **widok o projekcie** (`view-state == 2`) — technikalia;
- firmware (`src/main.rs`) multipleksuje FIFO klawiatury TCA8418 → czytelne nazwy
  (`"tab" | "enter" | "back" | "space"`), a logikę nawigacji wykonuje callback w .slint —
  zmiana stanu (`set_splash_done`, `view-state`, `menu-index`) to jedna linijka Rust;
- firmware w głównej pętli: `update_timers_and_animations()` →
  `draw_if_needed(render_by_line)` → co **250 ms** `ui.set_frame_index(i)` (w splashu
  **i w widoku szczura**), po `SPLASH_AFTER_MS` → `ui.set_splash_done(true)`;

### Klawiatura — TCA8418RTWR przez I2C0

`src/keyboard.rs` — zdejmuje zdarzenia z FIFO (rejestr KEY_EVENT_A=0x04,
blok EVENTA=LCK i EVENT COUNT=0x03), dekoduje bit7 jako „wciśnięty” i mapuje
1-based kod TCA8418 na indeks macierzy 7×8 używanej w ADV (Tab=1,
Backspace=52, Enter=54, Spacja=55, **strzałki ←=43 / ↑=46 / ↓=47 / →=51**
  — ADV ma fizyczne strzałki; ich fn-warstwa to `,` `;` `.` `/`). Metodologia i rejestry są spójne z
przykładowym crate'em `cardputer` 0.2 (cardputer-adv).

### Pamięć (ADV nie ma PSRAM!)

- Heap 150 KB w **RAM wewnętrznym** (`esp_alloc::heap_allocator!(size: 150*1024);`
  wywołać w main PRZED pierwszym Box/Rc); reszta pamięci jest w `.bss`/stack;
- Framebuffer *nie* jest potrzebny trwały — Slint w trybie `ReusedBuffer`
  przechowuje tylko **jedną linię** (240 Rgb565 = 480 B) i wypycha ją w SPI
  przez `LineBufferProvider` → `mipidsi::Display::set_pixels(...)`.
- Fonty i obrazki pakowane do flashu przez `build.rs` (embed resources);
- poprzednia wersja: 4 klatki ASCII-artu (Gio, z fontem mono) — w historii git.

## Konfiguracja (raz)

Nix devshell dostarcza `rustup`, `espup`, `espflash` i podpina `export-esp.sh`
(ścieżki do forkowanego GCC Xtensa).

Toolchain Xtensa instaluje się raz globalnie (fork nie jest dostępny w nixpkgs):

```bash
nix develop
espup install          # ~1.2 GB do ~/.rustup/toolchains/esp
```

> **NixOS:** forkowany rustc z espup potrzebuje dynamicznego linkera —
> `programs.nix-ld.enable = true;` w konfiguracji NixOS (na tym hoście już włączone).

> Build jest zweryfikowany: `cargo build --release` przechodzi
> (ELF w `target/xtensa-esp32s3-none-elf/release/`; obraz aplikacji ~467 KB —
> głównie renderer software Slint + spakowane fonty).

## Build i flash

### Łatwy flash: merged .bin (jedno polecenie)

```bash
nix develop
./build-bin.sh                      # build + merged ratputer-adv.bin (~520 KB)
```

Obraz zawiera wszystko: bootloader @0x0 + tabelę partycji @0x8000 + aplikację @0x10000.
Nagłówek wg spec modułu M5Stack StampS3A: **8 MB, QIO, 80 MHz** (zweryfikowane bajtami nagłówka).

```bash
espflash write-bin 0x0 ratputer-adv.bin --verify
# albo esptool:
esptool --chip esp32s3 write_flash 0x0 ratputer-adv.bin
```

### Flashowanie w pętli dev (z monitorem UART)

```bash
nix develop
cargo run --release     # flash + podgląd UART (espflash monitor)
```

Urządzenie wykrywa espflash automatycznie po kablu USB-C.
W trybie download (jeśli port nie pojawia się): wciśnij G0 + podłącz USB.

## Piny LCD (Cardputer ADV, wg pin mapy M5Stack ST7789V2)
| GPIO | Funkcja |
|---|---|
| G36 | SPI SCK |
| G35 | SPI MOSI (DAT) |

Szyna LCD działa na **40 MHz** — tyle używa referencyjna implementacja espp dla tej płytki
(`lcd_clock_speed = 40 * 1000 * 1000`). Przy 80 MHz obraz się „rozjeżdżał": piny LCD
(G33–G38) nie są natywnymi pinami IOMUX SPI2 na ESP32-S3 (sygnał idzie przez macierz
GPIO), więc przy 80 MHz czasy setup ST7789 są naruszane — korupcja danych daje
rozsypany/rozciągany obraz, różny w każdej klatce.
| G37 | CS |
| G34 | RS / DC |
| G33 | RST |
| G38 | Backlight |

Jeśli obraz jest **odwrócony w poziomie/pionie** (ale wypełnia ekran) — zmień
`Rotation::Deg90` na `Rotation::Deg270` w `src/main.rs`. Gdyby obraz był
**przesunięty ucięty**, sprawdź czy `display_size` i `display_offset` są podane
w natywnej orientacji panelu (patrz niżej).

### Geometria panelu (dlaczego takie wartości)

Panel to 1.14" ST7789V2 135x240 (natywnie portrait), a kontroler ma GRAM 240x320.
Panel jest w nim **wyśrodkowany**:

| Oś | Zakres w GRAM | Offset |
|---|---|---|
| 135 px → GRAM x | 52..186 | `(240-135)/2 = 52` |
| 240 px → GRAM y | 40..279 | `(320-240)/2 = 40` |

Dlatego (i bo `Builder::new` w mipidsi ≥0.8 wymaga ręcznych rozmiarów):
`display_size(135, 240)` w **natywnej** orientacji + `display_offset(52, 40)`.
Rotację do landscape (240x135) wykonuje MADCTL — mipidsi sam przelicza offset
(dla `Deg90` daje to natywne `(40, 53)`, co odpowiada wariantowi `st7789_pico1`
z mipidsi 0.7 dla tego samego panelu).

## Klawiatura (na później)

Cardputer ADV ma klawiaturę na kontrolerze **TCA8418** przez I²C (G8=SDA, G9=SCL, G4=INT).
Gotowy driver: crate `cardputer-adv-keyboard` (embedded-hal 1.0, kompatybilny z esp-hal).

## Zasoby

- [esp-hal docs — ESP32-S3](https://docs.espressif.com/projects/rust/esp-hal/latest/esp32s3/esp_hal/)
- [Rust on ESP Book](https://docs.espressif.com/projects/rust/book/)
- [Cardputer ADV — M5 docs](https://docs.m5stack.com/en/core/Cardputer-Adv)
- [espflash CLI](https://github.com/esp-rs/espflash)

## Licencja

MIT OR Apache-2.0
