//! Line-oriented debug control over the ESP32-S3 USB Serial/JTAG CDC port.
//!
//! Host commands use `RAT <id> <command>\n`. Responses are prefixed with
//! `@RAT <id>` so tooling can separate them from the normal `esp-println` log
//! stream carried by the same USB endpoint.

use core::fmt::{self, Write};

use esp_hal::{peripherals::USB_DEVICE, usb::usb_serial_jtag::UsbSerialJtag, Blocking};

const RX_LINE_LEN: usize = 192;
const TEXT_LEN: usize = 96;
const TX_QUEUE_LEN: usize = 4096;

#[derive(Clone, Copy, Debug)]
pub enum DebugKey {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Back,
    Backspace,
    Delete,
    Tab,
    Space,
}

#[derive(Clone, Copy)]
pub struct DebugText {
    bytes: [u8; TEXT_LEN],
    len: usize,
}

impl DebugText {
    pub fn as_str(&self) -> &str {
        // The parser accepts ASCII only.
        unsafe { core::str::from_utf8_unchecked(&self.bytes[..self.len]) }
    }
}

pub enum DebugCommand {
    Ping { id: u32 },
    Help { id: u32 },
    Status { id: u32 },
    Key { id: u32, key: DebugKey },
    Text { id: u32, text: DebugText },
    Clear { id: u32 },
    Reboot { id: u32 },
    Invalid { id: u32, reason: &'static str },
}

pub struct DebugConsole {
    serial: UsbSerialJtag<'static, Blocking>,
    rx_line: [u8; RX_LINE_LEN],
    rx_len: usize,
    rx_overflow: bool,
    tx_queue: [u8; TX_QUEUE_LEN],
    tx_head: usize,
    tx_len: usize,
    tx_dirty: bool,
}

impl DebugConsole {
    pub fn new(peripheral: USB_DEVICE<'static>) -> Self {
        Self {
            serial: UsbSerialJtag::new(peripheral),
            rx_line: [0; RX_LINE_LEN],
            rx_len: 0,
            rx_overflow: false,
            tx_queue: [0; TX_QUEUE_LEN],
            tx_head: 0,
            tx_len: 0,
            tx_dirty: false,
        }
    }

    /// Flush queued responses and return at most one complete host command.
    pub fn poll(&mut self) -> Option<DebugCommand> {
        self.service();
        loop {
            match self.serial.read_byte() {
                Ok(b'\n') => {
                    let command = if self.rx_overflow {
                        Some(DebugCommand::Invalid {
                            id: 0,
                            reason: "line_too_long",
                        })
                    } else {
                        parse_command(&self.rx_line[..self.rx_len])
                    };
                    self.rx_len = 0;
                    self.rx_overflow = false;
                    if command.is_some() {
                        return command;
                    }
                }
                Ok(b'\r') => {}
                Ok(byte) => {
                    if self.rx_len < self.rx_line.len() {
                        self.rx_line[self.rx_len] = byte;
                        self.rx_len += 1;
                    } else {
                        self.rx_overflow = true;
                    }
                }
                Err(nb::Error::WouldBlock) => return None,
                Err(nb::Error::Other(error)) => match error {},
            }
        }
    }

    /// Progress non-blocking USB transmission. Safe to call every main-loop pass.
    pub fn service(&mut self) {
        while self.tx_len > 0 {
            let byte = self.tx_queue[self.tx_head];
            match self.serial.write_byte_nb(byte) {
                Ok(()) => {
                    self.tx_head = (self.tx_head + 1) % self.tx_queue.len();
                    self.tx_len -= 1;
                    self.tx_dirty = true;
                }
                Err(nb::Error::WouldBlock) => break,
                Err(nb::Error::Other(error)) => match error {},
            }
        }

        if self.tx_len == 0 && self.tx_dirty {
            match self.serial.flush_tx_nb() {
                Ok(()) => self.tx_dirty = false,
                Err(nb::Error::WouldBlock) => {}
                Err(nb::Error::Other(error)) => match error {},
            }
        }
    }

    pub fn output_idle(&self) -> bool {
        self.tx_len == 0 && !self.tx_dirty
    }

    pub fn ok(&mut self, id: u32, message: fmt::Arguments<'_>) {
        let _ = writeln!(self, "@RAT {id} OK {message}\r");
    }

    pub fn data(&mut self, id: u32, message: fmt::Arguments<'_>) {
        let _ = writeln!(self, "@RAT {id} DATA {message}\r");
    }

    pub fn error(&mut self, id: u32, message: fmt::Arguments<'_>) {
        let _ = writeln!(self, "@RAT {id} ERR {message}\r");
    }

    pub fn end(&mut self, id: u32) {
        let _ = writeln!(self, "@RAT {id} END\r");
    }
}

impl Write for DebugConsole {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if value.len() > self.tx_queue.len() - self.tx_len {
            return Err(fmt::Error);
        }
        for byte in value.bytes() {
            let tail = (self.tx_head + self.tx_len) % self.tx_queue.len();
            self.tx_queue[tail] = byte;
            self.tx_len += 1;
        }
        Ok(())
    }
}

fn parse_command(line: &[u8]) -> Option<DebugCommand> {
    let Ok(line) = core::str::from_utf8(line) else {
        return Some(DebugCommand::Invalid {
            id: 0,
            reason: "non_ascii_command",
        });
    };
    let mut prefix_parts = line.splitn(3, ' ');
    if prefix_parts.next()? != "RAT" {
        // Ignore unrelated bytes. This lets the CDC endpoint remain usable by
        // espflash and ordinary serial terminals without producing errors.
        return None;
    }
    let id = match prefix_parts.next().and_then(|value| value.parse().ok()) {
        Some(id) => id,
        None => {
            return Some(DebugCommand::Invalid {
                id: 0,
                reason: "bad_request_id",
            });
        }
    };
    let Some(request) = prefix_parts.next() else {
        return Some(DebugCommand::Invalid {
            id,
            reason: "missing_command",
        });
    };
    let (name, argument) = request
        .split_once(' ')
        .map_or((request, None), |(name, argument)| (name, Some(argument)));

    if name.eq_ignore_ascii_case("PING") {
        Some(DebugCommand::Ping { id })
    } else if name.eq_ignore_ascii_case("HELP") {
        Some(DebugCommand::Help { id })
    } else if name.eq_ignore_ascii_case("STATUS") {
        Some(DebugCommand::Status { id })
    } else if name.eq_ignore_ascii_case("CLEAR") {
        Some(DebugCommand::Clear { id })
    } else if name.eq_ignore_ascii_case("REBOOT") {
        Some(DebugCommand::Reboot { id })
    } else if name.eq_ignore_ascii_case("KEY") {
        let key = match argument {
            Some(value) if value.eq_ignore_ascii_case("up") => DebugKey::Up,
            Some(value) if value.eq_ignore_ascii_case("down") => DebugKey::Down,
            Some(value) if value.eq_ignore_ascii_case("left") => DebugKey::Left,
            Some(value) if value.eq_ignore_ascii_case("right") => DebugKey::Right,
            Some(value) if value.eq_ignore_ascii_case("enter") => DebugKey::Enter,
            Some(value) if value.eq_ignore_ascii_case("back") => DebugKey::Back,
            Some(value) if value.eq_ignore_ascii_case("backspace") => DebugKey::Backspace,
            Some(value) if value.eq_ignore_ascii_case("delete") => DebugKey::Delete,
            Some(value) if value.eq_ignore_ascii_case("tab") => DebugKey::Tab,
            Some(value) if value.eq_ignore_ascii_case("space") => DebugKey::Space,
            _ => {
                return Some(DebugCommand::Invalid {
                    id,
                    reason: "bad_key",
                });
            }
        };
        Some(DebugCommand::Key { id, key })
    } else if name.eq_ignore_ascii_case("TEXT") {
        let Some(argument) = argument else {
            return Some(DebugCommand::Invalid {
                id,
                reason: "missing_text",
            });
        };
        if argument.is_empty()
            || argument.len() > TEXT_LEN
            || !argument
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        {
            return Some(DebugCommand::Invalid {
                id,
                reason: "text_must_be_1_96_printable_ascii",
            });
        }
        let mut bytes = [0; TEXT_LEN];
        bytes[..argument.len()].copy_from_slice(argument.as_bytes());
        Some(DebugCommand::Text {
            id,
            text: DebugText {
                bytes,
                len: argument.len(),
            },
        })
    } else {
        Some(DebugCommand::Invalid {
            id,
            reason: "unknown_command",
        })
    }
}
