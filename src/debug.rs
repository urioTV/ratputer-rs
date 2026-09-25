//! Interactive command shell over the ESP32-S3 USB Serial/JTAG CDC port.
//!
//! Open it with `espflash monitor` or any serial terminal. The console shares
//! the stream with normal firmware logs, so its own output is prefixed with
//! `[rat]`. All transmission is queued and non-blocking: a disconnected host
//! must never stall the UI or network loop.

use core::fmt::{self, Write};

use esp_hal::{Blocking, peripherals::USB_DEVICE, usb::usb_serial_jtag::UsbSerialJtag};

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
    Ping,
    Help,
    Status,
    Key { key: DebugKey },
    Text { text: DebugText },
    Clear,
    Reboot,
    Invalid { reason: &'static str },
}

pub struct DebugConsole {
    serial: UsbSerialJtag<'static, Blocking>,
    rx_line: [u8; RX_LINE_LEN],
    rx_len: usize,
    rx_overflow: bool,
    /// 0 = ordinary input, 1 = ESC received, 2 = CSI sequence in progress.
    escape_state: u8,
    /// Suppress LF after a CR so CRLF terminals submit one command, not two.
    last_was_cr: bool,
    tx_queue: [u8; TX_QUEUE_LEN],
    tx_head: usize,
    tx_len: usize,
    tx_dirty: bool,
}

impl DebugConsole {
    pub fn new(peripheral: USB_DEVICE<'static>) -> Self {
        let mut console = Self {
            serial: UsbSerialJtag::new(peripheral),
            rx_line: [0; RX_LINE_LEN],
            rx_len: 0,
            rx_overflow: false,
            escape_state: 0,
            last_was_cr: false,
            tx_queue: [0; TX_QUEUE_LEN],
            tx_head: 0,
            tx_len: 0,
            tx_dirty: false,
        };
        let _ = write!(
            console,
            "\r\n[rat] RATPUTER USB console - type 'help' for commands\r\nrat> "
        );
        console
    }

    /// Flush queued output and return at most one complete command line.
    pub fn poll(&mut self) -> Option<DebugCommand> {
        self.service();
        loop {
            match self.serial.read_byte() {
                Ok(b'\r') => {
                    self.last_was_cr = true;
                    if let Some(command) = self.finish_line() {
                        return Some(command);
                    }
                }
                Ok(b'\n') => {
                    if self.last_was_cr {
                        self.last_was_cr = false;
                    } else if let Some(command) = self.finish_line() {
                        return Some(command);
                    }
                }
                Ok(byte) => {
                    self.last_was_cr = false;
                    self.handle_byte(byte);
                }
                Err(nb::Error::WouldBlock) => return None,
                Err(nb::Error::Other(error)) => match error {},
            }
        }
    }

    fn finish_line(&mut self) -> Option<DebugCommand> {
        let _ = self.write_str("\r\n");
        let command = if self.rx_overflow {
            Some(DebugCommand::Invalid {
                reason: "line_too_long",
            })
        } else {
            parse_command(&self.rx_line[..self.rx_len])
        };
        self.rx_len = 0;
        self.rx_overflow = false;
        self.escape_state = 0;
        if command.is_none() {
            self.prompt();
        }
        command
    }

    fn handle_byte(&mut self, byte: u8) {
        if self.escape_state != 0 {
            self.escape_state = match (self.escape_state, byte) {
                (1, b'[') => 2,
                (2, 0x40..=0x7e) => 0,
                (2, _) => 2,
                _ => 0,
            };
            return;
        }

        match byte {
            0x1b => self.escape_state = 1,
            0x08 | 0x7f => {
                if self.rx_len > 0 {
                    self.rx_len -= 1;
                    let _ = self.write_str("\x08 \x08");
                }
            }
            // Ctrl+U clears the current command line in terminals that send it.
            0x15 => {
                while self.rx_len > 0 {
                    self.rx_len -= 1;
                    let _ = self.write_str("\x08 \x08");
                }
                self.rx_overflow = false;
            }
            0x20..=0x7e => {
                if self.rx_len < self.rx_line.len() {
                    self.rx_line[self.rx_len] = byte;
                    self.rx_len += 1;
                    let _ = self.write_char(byte as char);
                } else if !self.rx_overflow {
                    self.rx_overflow = true;
                    let _ = self.write_char('\x07');
                }
            }
            _ => {}
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

    pub fn ok(&mut self, message: fmt::Arguments<'_>) {
        let _ = writeln!(self, "[rat] OK {message}\r");
    }

    pub fn data(&mut self, message: fmt::Arguments<'_>) {
        let _ = writeln!(self, "[rat] {message}\r");
    }

    pub fn error(&mut self, message: fmt::Arguments<'_>) {
        let _ = writeln!(self, "[rat] ERROR {message}\r");
    }

    pub fn end(&mut self) {
        self.prompt();
    }

    fn prompt(&mut self) {
        let _ = self.write_str("rat> ");
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
            reason: "non_ascii_command",
        });
    };
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    let (name, argument) = line
        .split_once(' ')
        .map_or((line, None), |(name, argument)| (name, Some(argument)));

    if name.eq_ignore_ascii_case("PING") {
        Some(DebugCommand::Ping)
    } else if name.eq_ignore_ascii_case("HELP") {
        Some(DebugCommand::Help)
    } else if name.eq_ignore_ascii_case("STATUS") {
        Some(DebugCommand::Status)
    } else if name.eq_ignore_ascii_case("CLEAR") {
        Some(DebugCommand::Clear)
    } else if name.eq_ignore_ascii_case("REBOOT") {
        Some(DebugCommand::Reboot)
    } else if name.eq_ignore_ascii_case("KEY") {
        let key = match argument.map(str::trim) {
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
                return Some(DebugCommand::Invalid { reason: "bad_key" });
            }
        };
        Some(DebugCommand::Key { key })
    } else if name.eq_ignore_ascii_case("TEXT") {
        let Some(argument) = argument else {
            return Some(DebugCommand::Invalid {
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
                reason: "text_must_be_1_96_printable_ascii",
            });
        }
        let mut bytes = [0; TEXT_LEN];
        bytes[..argument.len()].copy_from_slice(argument.as_bytes());
        Some(DebugCommand::Text {
            text: DebugText {
                bytes,
                len: argument.len(),
            },
        })
    } else {
        Some(DebugCommand::Invalid {
            reason: "unknown_command",
        })
    }
}
