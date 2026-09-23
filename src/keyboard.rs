//! Cardputer ADV keyboard — TCA8418RTWR over I2C0 (addr 0x34, SDA=G8, SCL=G9).
//!
//! Minimal driver covering what the UI needs: *press* events mapped to
//! navigation keys. The FIFO mechanics and the event-code table follow the
//! `cardputer` 0.2.0 crate (src/adv/keyboard.rs, MIT OR Apache-2.0) — ported
//! from esp-idf-hal to esp-hal blocking I2C (embedded-hal 1.0).

use esp_hal::i2c::master::I2c;
use esp_hal::Blocking;

const I2C_ADDRESS: u8 = 0x34;

// TCA8418 registers
const ADDR_CFG: u8 = 0x01;          // CFG — bit0 = KE_IEN (key event FIFO interrupt)
const REG_KEY_LCK_EC: u8 = 0x03;    // KEY_LCK_EC — low nibble = number of FIFO events
const REG_KEY_EVENT_A: u8 = 0x04;   // KEY_EVENT_A — reading pops one FIFO entry
const ADDR_KP_GPIO1: u8 = 0x1D;     // ROW0..7 as keypad inputs
const ADDR_KP_GPIO2: u8 = 0x1E;     // COL0..7 as keypad outputs
const ADDR_KP_GPIO3: u8 = 0x1F;     // COL8/COL9 (unused on the ADV)

/// Index into the ADV's 7x8 matrix (see `cardputer::keyboard::adv::KEY_MATRIX`).
/// The TCA8418 event code is 1-based with 10 columns per row; conversion:
/// `idx = code - (code / 10) * 2 - 1` — then:
///   1 = Tab, 52 = Backspace, 54 = Enter, 55 = Space;
///   43 = Left Arrow, 46 = Up Arrow, 47 = Down Arrow, 51 = Right Arrow
///   (physical ADV keys whose Fn layer yields `,` `;` `.` `/` — hence the indices).
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
    Other(u8), // matrix index (for future use)
}

pub struct Keyboard<'a> {
    i2c: I2c<'a, Blocking>,
}

impl<'a> Keyboard<'a> {
    /// Matrix setup — configuration identical to `cardputer::keyboard::adv`.
    pub fn new(i2c: I2c<'a, Blocking>) -> Self {
        let mut k = Self { i2c };
        // ROW0..6 as keypad (7 rows: 0x7F), COL0..7 as keypad (0xFF), COL8/9 disabled
        let _ = k.i2c.write(I2C_ADDRESS, &[ADDR_KP_GPIO1, 0x7F]);
        let _ = k.i2c.write(I2C_ADDRESS, &[ADDR_KP_GPIO2, 0xFF]);
        let _ = k.i2c.write(I2C_ADDRESS, &[ADDR_KP_GPIO3, 0x00]);
        // Enable key-event FIFO interrupts
        let _ = k.i2c.write(I2C_ADDRESS, &[ADDR_CFG, 0x01]);
        k.flush();
        k
    }

    /// Drain the FIFO (so stale boot-time events don't confuse the UI).
    fn flush(&mut self) {
        let mut b = [0u8; 1];
        // Read KEY_EVENT_A until the FIFO is empty (max 10 events)
        for _ in 0..11 {
            if self.i2c.write_read(I2C_ADDRESS, &[REG_KEY_EVENT_A], &mut b).is_err() {
                return;
            }
            if b[0] == 0xFF || b[0] == 0x00 {
                return;
            }
        }
    }

    /// Pops one event from the FIFO. Release events and empty slots are skipped.
    /// Returns a navigation key decoded from *press* events.
    pub fn next_nav_key(&mut self) -> Option<NavKey> {
        // Check the pending event count
        let mut ec = [0u8; 1];
        if self.i2c.write_read(I2C_ADDRESS, &[REG_KEY_LCK_EC], &mut ec).is_err() {
            return None;
        }
        if ec[0] & 0x0F == 0 {
            return None;
        }

        // Pop one event — log it for diagnostics
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
        // 1-based code, 10 columns per row — 8 columns actually wired
        let idx = code.wrapping_sub((code / 10) * 2).wrapping_sub(1);
        log::debug!("kbd event: raw=0x{:02x} pressed={} idx={}", raw, pressed, idx);

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
