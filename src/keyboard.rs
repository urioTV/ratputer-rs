//! Klawiatura Cardputer ADV — TCA8418RTWR przez I2C0 (addr 0x34, SDA=G8, SCL=G9).
//!
//! Sterownik ograniczony do tego co potrzebne w UI: zdarzenia *naciśnięcia*
//! zmapowane na klawisze nawigacyjne. Metodologia FIFO i tablica kodów
//! odpowiadają crate'owi `cardputer` 0.2.0 (src/adv/keyboard.rs, MIT OR Apache-2.0)
//! — przepisane z esp-idf-hal na esp-hal blocking I2C (embedded-hal 1.0).

use esp_hal::i2c::master::I2c;
use esp_hal::Blocking;

const I2C_ADDRESS: u8 = 0x34;

// Rejestry TCA8418
const ADDR_CFG: u8 = 0x01;          // CFG — bit0 = KE_IEN (interrupt z kolejki klawiszy)
const REG_KEY_LCK_EC: u8 = 0x03;    // KEY_LCK_EC — dół = liczba zdarzeń w FIFO
const REG_KEY_EVENT_A: u8 = 0x04;   // KEY_EVENT_A / KP_GPIO — czytanie zdejmuje z FIFO
const ADDR_KP_GPIO1: u8 = 0x1D;     // ROW0..7 jako wejścia keypadu
const ADDR_KP_GPIO2: u8 = 0x1E;     // COL0..7 jako wyjścia keypadu
const ADDR_KP_GPIO3: u8 = 0x1F;     // COL8/COL9 (nie używane w ADV)

/// Indeks w macierzy 7×8 (wg talerzy `cardputer::keyboard::adv::KEY_MATRIX`).
/// Kod zdarzenia TCA8418 jest 1-based z 10 kolumnami (kampany n-ty rząd). Przeliczenie:
/// `idx = code - (code / 10) * 2 - 1` — wtedy:
///   1 = Tab, 52 = Backspace, 54 = Enter, 55 = Spacja;
///   43 = ←, 46 = ↑, 47 = ↓, 51 = → (fizyczne strzałki ADV as klawisze, których
///   fn-warstwa to `,` `;` `.` `/` — stąd ich indeksy).
const IDX_TAB: u8 = 1;
const IDX_ARROW_LEFT: u8 = 43;
const IDX_ARROW_UP: u8 = 46;
const IDX_ARROW_DOWN: u8 = 47;
const IDX_ARROW_RIGHT: u8 = 51;
const IDX_BACKSPACE: u8 = 52;
const IDX_ENTER: u8 = 54;
const IDX_SPACE: u8 = 55;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NavKey {
    Tab,
    Enter,
    Backspace,
    Space,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Other(u8), // kod indeksu macierzy (do przyszłego wykorzystania)
}

pub struct Keyboard<'a> {
    i2c: I2c<'a, Blocking>,
}

impl<'a> Keyboard<'a> {
    /// Inicjalizacja macierzy — konfiguracja identyczna jak w `cardputer::adv`.
    pub fn new(i2c: I2c<'a, Blocking>) -> Self {
        let mut k = Self { i2c };
        // ROW0..6 jako keypad (7 wierszy: 0x7F), COL0..7 jako keypad (0xFF), COL8/9 wyłączone
        let _ = k.i2c.write(I2C_ADDRESS, &[ADDR_KP_GPIO1, 0x7F]);
        let _ = k.i2c.write(I2C_ADDRESS, &[ADDR_KP_GPIO2, 0xFF]);
        let _ = k.i2c.write(I2C_ADDRESS, &[ADDR_KP_GPIO3, 0x00]);
        // Włącz interrupt fifo klawiszy
        let _ = k.i2c.write(I2C_ADDRESS, &[ADDR_CFG, 0x01]);
        k.flush();
        k
    }

    /// Opróżnij FIFO (żeby stare zdarzenia z bootu nie przeszkadzały).
    fn flush(&mut self) {
        let mut b = [0u8; 1];
        // Czytamy KEY_EVENT_A do exhaustion FIFO (max 10 zdarzeń)
        for _ in 0..11 {
            if self.i2c.write_read(I2C_ADDRESS, &[REG_KEY_EVENT_A], &mut b).is_err() {
                return;
            }
            if b[0] == 0xFF || b[0] == 0x00 {
                return;
            }
        }
    }

    /// Zdejmuje jedno zdarzenie z FIFO. Zdarzenia puści (release) i puste są przewalane.
    /// Zwraca kod nawigacyjny dla zdarzenia *naciśnięcia*.
    pub fn next_nav_key(&mut self) -> Option<NavKey> {
        // Sprawdź liczbę zdarzeń
        let mut ec = [0u8; 1];
        if self.i2c.write_read(I2C_ADDRESS, &[REG_KEY_LCK_EC], &mut ec).is_err() {
            return None;
        }
        if ec[0] & 0x0F == 0 {
            return None;
        }

        // Zdejmij jedno zdarzenie — serializujemy go do dziennika diagnostycznego
        let mut ev = [0u8; 1];
        if self.i2c.write_read(I2C_ADDRESS, &[REG_KEY_EVENT_A], &mut ev).is_err() {
            return None;
        }
        let raw = ev[0];
        if raw == 0xFF {
            return None;
        }

        let pressed = raw & 0x80 != 0;
        let code = raw & 0x7F;
        // 1-based code, kolumny 10 w rzędzie — 8 kolumn używanych
        let idx = code.wrapping_sub((code / 10) * 2).wrapping_sub(1);
        log::info!("kbd event: raw=0x{:02x} pressed={} idx={}", raw, pressed, idx);

        if !pressed {
            return None;
        }
        Some(match idx {
            IDX_TAB => NavKey::Tab,
            IDX_BACKSPACE => NavKey::Backspace,
            IDX_ENTER => NavKey::Enter,
            IDX_SPACE => NavKey::Space,
            IDX_ARROW_UP => NavKey::ArrowUp,
            IDX_ARROW_DOWN => NavKey::ArrowDown,
            IDX_ARROW_LEFT => NavKey::ArrowLeft,
            IDX_ARROW_RIGHT => NavKey::ArrowRight,
            other => NavKey::Other(other),
        })
    }
}
