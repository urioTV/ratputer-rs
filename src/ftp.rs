//! Minimal FTP server (RFC 959, passive mode only) exposing the SD card.
//!
//! There is no async executor: every socket call is a future polled exactly
//! once per step with a no-op waker, guarded by `can_recv()`/`can_send()`.
//! The main loop polls this module every iteration, and burst-polls it while a
//! transfer is in flight. One control connection at a time; the data channel
//! uses a fixed passive listener on port 50000.
//!
//! FAT access goes through `hadris-fat`. Open handles (`FatDir`, `FileReader`,
//! `FileWriter`) borrow the volume, so they are created, used, and dropped
//! within a single poll step; transfer state between polls is paths, offsets,
//! and opaque validated FAT cursors (append + read). Writes are write-through
//! (no FAT cache), so the 226 reply
//! means the data physically reached the card.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::future::Future;
use core::task::{Context, Poll, Waker};

use embassy_net::tcp::{State, TcpSocket};
use embassy_net::{IpAddress, Ipv4Address, Stack};
use embassy_time::Duration as NetDuration;
use esp_hal::time::Instant;
use hadris_fat::sync::{
    read::{FileReader, ReadCursor},
    write::{AppendCursor, FileWriter},
    FatDir, FileEntry, SeekFrom,
};
use hadris_fat::time::FatDateTime;

use crate::storage::{fmt_fat_mtime, now_ymdhms, FtpConfig, SdBlock, SdVolume};

pub const CONTROL_PORT: u16 = 21;
/// Fixed passive-mode data listener.
const DATA_PORT: u16 = 50_000;
const CONTROL_RX_LEN: usize = 1024;
const CONTROL_TX_LEN: usize = 1024;
const DATA_RX_LEN: usize = 4096;
const DATA_TX_LEN: usize = 4096;
/// Directory listings are sent in runs of this size to keep RAM flat.
const LIST_CHUNK_TARGET: usize = 3072;
const MAX_COMMAND_LINE: usize = 512;
/// Close idle control connections after 5 minutes.
const IDLE_TIMEOUT_SECS: u64 = 300;
/// Drop any session with no control or data progress for this long. Data
/// progress refreshes the timer, so large healthy transfers are not capped.
const LIVEN_TIMEOUT_SECS: u64 = 120;
/// Give the client this long to open its data connection after PASV.
const DATA_ACCEPT_TIMEOUT_SECS: u64 = 20;
/// Bytes processed per poll for bulk file transfers.
const FILE_CHUNK_LEN: usize = 4096;

/// Poll a future exactly once with a no-op waker. `None` = would block.
fn poll_once<F, O>(future: F) -> Option<O>
where
    F: Future<Output = O>,
{
    let mut future = core::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => Some(output),
        Poll::Pending => None,
    }
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub enum FtpStatus {
    #[default]
    Stopped,
    /// The stack has no DHCP lease yet.
    Offline,
    Listening,
    Connected,
    Transfer,
}

impl FtpStatus {
    pub fn label(self) -> &'static str {
        match self {
            FtpStatus::Stopped => "STOPPED",
            FtpStatus::Offline => "WAITING FOR WI-FI",
            FtpStatus::Listening => "LISTENING :21",
            FtpStatus::Connected => "CLIENT CONNECTED",
            FtpStatus::Transfer => "TRANSFER IN PROGRESS",
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct FtpStats {
    pub sent_bytes: u64,
    pub received_bytes: u64,
}

#[derive(Clone, Copy, Default)]
enum ListKind {
    #[default]
    List,
    Names,
    Machine,
}

/// A transfer the client ordered, waiting on the passive connection.
struct Pending {
    action: PendingAction,
    since: Instant,
}

enum PendingAction {
    List {
        parent: Vec<String>,
        leaf: Option<String>,
        kind: ListKind,
    },
    Retrieve {
        parent: Vec<String>,
        leaf: String,
        offset: u64,
    },
    Store {
        parent: Vec<String>,
        leaf: String,
    },
}

enum TransferKind {
    List {
        parent: Vec<String>,
        kind: ListKind,
        /// Entries already consumed from the directory across chunks.
        skip: usize,
        done: bool,
    },
    Retrieve {
        parent: Vec<String>,
        leaf: String,
        offset: u64,
        /// FAT cluster position after the last read chunk.
        cursor: Option<ReadCursor>,
    },
    Store {
        parent: Vec<String>,
        leaf: String,
        /// True once the entry exists and initial write succeeded.
        initialized: bool,
        /// FAT tail position after the last committed chunk. This avoids an
        /// O(n) chain walk for every independently-polled append.
        cursor: Option<AppendCursor>,
    },
}

struct Transfer {
    topic: String,
    kind: TransferKind,
    /// Wait until the preliminary 150 reply has left the control socket before
    /// sending any data (required FTP ordering).
    announced: bool,
    /// Outbound payload pending on the data socket, plus its send offset.
    chunk: Vec<u8>,
    chunk_sent: usize,
    /// Data production ended; waiting to close the data stream.
    finishing: bool,
    failed: Option<&'static str>,
}

struct Session {
    cwd: Vec<String>,
    user: Option<String>,
    logged_in: bool,
    rest: Option<u64>,
    rename_from: Option<(Vec<String>, String)>,
    line: Vec<u8>,
    out: String,
    peer: String,
    pending: Option<Pending>,
    transfer: Option<Transfer>,
    /// PASV/EPSV given; waiting for the client to pick a data transfer.
    passive_requested: bool,
    /// Keep one full network-runner poll between queuing 227/229 and arming
    /// the second listening socket.
    passive_arm_delay: bool,
    quit: bool,
    last_activity: Instant,
}

impl Session {
    fn new(peer: String) -> Self {
        Self {
            cwd: Vec::new(),
            user: None,
            logged_in: false,
            rest: None,
            rename_from: None,
            line: Vec::new(),
            out: String::new(),
            peer,
            pending: None,
            transfer: None,
            passive_requested: false,
            passive_arm_delay: false,
            quit: false,
            last_activity: Instant::now(),
        }
    }

    fn reply(&mut self, text: &str) {
        self.out.push_str(text);
        self.out.push_str("\r\n");
    }

    fn busy(&self) -> bool {
        self.pending.is_some() || self.transfer.is_some()
    }
}

pub struct FtpServer {
    control: TcpSocket<'static>,
    data: TcpSocket<'static>,
    session: Option<Session>,
    control_listening: bool,
    /// The fixed passive listener is armed for this session.
    data_listening: bool,
    ip: Option<Ipv4Address>,
    stats: FtpStats,
    status: FtpStatus,
    activity: String,
}

impl FtpServer {
    /// Socket buffers (18 KiB) live in the heap; the caller checks
    /// `esp_alloc::HEAP.free()` and constructs the server only when there is
    /// enough room. The server (and its buffers) persist across opens of the
    /// FTP screen; `start`/`stop` only open and close listeners.
    pub fn new(stack: Stack<'static>) -> Self {
        let control_rx = Box::leak(Box::new([0_u8; CONTROL_RX_LEN]));
        let control_tx = Box::leak(Box::new([0_u8; CONTROL_TX_LEN]));
        let data_rx = Box::leak(Box::new([0_u8; DATA_RX_LEN]));
        let data_tx = Box::leak(Box::new([0_u8; DATA_TX_LEN]));
        let mut control = TcpSocket::new(stack, control_rx, control_tx);
        let mut data = TcpSocket::new(stack, data_rx, data_tx);
        // Session-level idle/liveness checks reclaim dead control clients and
        // are refreshed by data progress. A transport timeout on the control
        // socket would incorrectly reset any transfer lasting over 45 seconds.
        control.set_timeout(None);
        // A stalled passive data connection still needs a bounded recovery.
        data.set_timeout(Some(NetDuration::from_secs(45)));
        Self {
            control,
            data,
            session: None,
            control_listening: false,
            data_listening: false,
            ip: None,
            stats: FtpStats::default(),
            status: FtpStatus::Stopped,
            activity: String::new(),
        }
    }

    pub fn start(&mut self, stack: Stack<'static>) -> Result<(), ()> {
        self.stats = FtpStats::default();
        self.activity.clear();
        self.ip = stack.config_v4().and_then(|config| {
            (config.address.address() != Ipv4Address::UNSPECIFIED)
                .then_some(config.address.address())
        });
        if self.ip.is_some() {
            self.status = FtpStatus::Listening;
        } else {
            self.status = FtpStatus::Offline;
        }
        self.control_listening = false;
        self.control.abort();
        self.data_listening = false;
        self.data.abort();
        self.session = None;
        Ok(())
    }

    pub fn stop(&mut self) {
        self.control.abort();
        self.data.abort();
        self.control_listening = false;
        self.data_listening = false;
        self.session = None;
        self.status = FtpStatus::Stopped;
        self.activity.clear();
    }

    pub fn status(&self) -> FtpStatus {
        self.status
    }

    pub fn stats(&self) -> FtpStats {
        self.stats
    }

    pub fn activity(&self) -> &str {
        &self.activity
    }

    pub fn peer(&self) -> Option<&str> {
        self.session.as_ref().map(|session| session.peer.as_str())
    }

    pub fn ip(&self) -> Option<Ipv4Address> {
        self.ip
    }

    /// Advance every socket and the session once. Called from the main loop;
    /// additionally burst-called while a transfer is in flight.
    pub fn poll(&mut self, volume: &SdVolume, stack: Stack<'static>, config: &FtpConfig) {
        self.ip = stack.config_v4().and_then(|config| {
            (config.address.address() != Ipv4Address::UNSPECIFIED)
                .then_some(config.address.address())
        });

        if !self.control_listening {
            match self.control.state() {
                State::Closed => {
                    let _ = poll_once(self.control.accept(CONTROL_PORT));
                    self.control_listening = true;
                }
                // smoltcp cannot listen from teardown states; abort back to Closed.
                State::TimeWait
                | State::LastAck
                | State::FinWait1
                | State::FinWait2
                | State::CloseWait => self.control.abort(),
                _ => {}
            }
        } else if self.session.is_none() && self.control.state() == State::Established {
            let peer = self
                .control
                .remote_endpoint()
                .map(|endpoint| format_tcp_endpoint(endpoint.addr, endpoint.port))
                .unwrap_or_else(|| String::from("?"));
            log::info!("FTP client connected from {peer}");
            let mut session = Session::new(peer);
            session.reply("220 RATPUTER SD");
            self.session = Some(session);
        }

        self.status = self
            .session
            .as_ref()
            .map(|session| {
                if session.busy() {
                    FtpStatus::Transfer
                } else {
                    FtpStatus::Connected
                }
            })
            .unwrap_or(if self.ip.is_some() {
                FtpStatus::Listening
            } else {
                FtpStatus::Offline
            });

        if self.session.is_some() {
            let mut session = self.session.take().unwrap();
            if self.poll_session(volume, &mut session, config) {
                // Session over: discard teardown states and arm the single
                // listener immediately. This removes the brief refused window
                // seen by clients that reconnect right after QUIT.
                let _ = session.transfer.take();
                let _ = session.pending.take();
                self.control.abort();
                let _ = poll_once(self.control.accept(CONTROL_PORT));
                self.control_listening = true;
                self.data.abort();
                self.data_listening = false;
                log::info!("FTP client {} left", session.peer);
            } else {
                self.session = Some(session);
            }
        }
    }

    /// Returns `true` when the session is over.
    fn poll_session(
        &mut self,
        volume: &SdVolume,
        session: &mut Session,
        config: &FtpConfig,
    ) -> bool {
        // 1. Flush queued replies; one write per poll is plenty.
        let mut fatal = false;
        if !session.out.is_empty() && self.control.can_send() {
            match poll_once(self.control.write(session.out.as_bytes())) {
                Some(Ok(written)) if written > 0 => {
                    session.out.drain(..written);
                }
                Some(Ok(_)) => {}
                Some(Err(_)) => fatal = true,
                None => {}
            }
        }

        // 2. Drain command bytes.
        if !fatal && self.control.can_recv() {
            let mut buffer = [0_u8; 256];
            match poll_once(self.control.read(&mut buffer)) {
                Some(Ok(0)) | Some(Err(_)) => fatal = true,
                Some(Ok(count)) => {
                    session.last_activity = Instant::now();
                    // Strip Telnet IAC bytes (FileZilla urgency trick on ABOR).
                    session
                        .line
                        .extend(buffer[..count].iter().filter(|byte| **byte != 0xFF));
                    if session.line.len() > MAX_COMMAND_LINE {
                        session.reply("500 Command line too long");
                        session.line.clear();
                    }
                }
                None => {}
            }
        }

        // 3. Handle complete command lines in arrival order.
        if !fatal {
            while let Some(end) = session.line.iter().position(|byte| *byte == b'\n') {
                let mut line: Vec<u8> = session.line.drain(..=end).collect();
                if line.ends_with(b"\n") {
                    line.pop();
                }
                if line.ends_with(b"\r") {
                    line.pop();
                }
                self.handle_command_line(volume, session, &line, config);
            }
        }

        // 4. Idle timeout (no transfer or pending data channel in flight).
        if !fatal
            && !session.busy()
            && Instant::now() - session.last_activity
                >= esp_hal::time::Duration::from_secs(IDLE_TIMEOUT_SECS)
        {
            session.reply("421 Idle timeout");
            session.quit = true;
        }

        // 4b. Hard liveness limit: a client that vanished without FIN/RST
        // (WSL mirrored-network artifacts, powered-off laptop) leaves the
        // Established socket with an undeliverable reply forever. The
        // smoltcp socket timeout did not prove reliable here, so drop the
        // session after LIVEN_TIMEOUT_SECS without any received bytes.
        if !fatal
            && Instant::now() - session.last_activity
                >= esp_hal::time::Duration::from_secs(LIVEN_TIMEOUT_SECS)
        {
            log::warn!("FTP session idle past limit; dropping client");
            self.control.abort();
            self.control_listening = false;
            fatal = true;
        }

        // 5. Data channel and transfers.
        if !fatal {
            self.poll_data(volume, session);
        }

        // 6. Clean shutdown after QUIT: wait for queued replies to leave.
        // If the client went away first, the TX queue may never drain — abort
        // instead of waiting forever.
        if session.quit && session.out.is_empty() {
            if self.control.send_queue() == 0 {
                self.control.close();
                self.control_listening = false;
                fatal = true;
            } else if !self.control.may_send() || !self.control.may_recv() {
                self.control.abort();
                self.control_listening = false;
                fatal = true;
            }
        }

        // 7. The client hung up on us. After a peer FIN smoltcp enters
        // CloseWait: may_send() is still true, but may_recv() is false and an
        // empty receive queue never makes can_recv() true again.
        if !session.quit
            && ((!self.control.may_recv() && !self.control.can_recv())
                || (!self.control.may_send()
                    && session.out.is_empty()
                    && self.control.send_queue() == 0))
        {
            log::info!(
                "FTP ctrl peer gone: state={:?} out={} queue={}",
                self.control.state(),
                session.out.len(),
                self.control.send_queue()
            );
            fatal = true;
        }
        fatal
    }

    fn poll_data(&mut self, volume: &SdVolume, session: &mut Session) {
        // Queue 227/229 into the established control socket *before* putting
        // the second socket into Listen, then keep one full network poll
        // between those two events.
        if session.passive_requested && !self.data_listening {
            if !session.out.is_empty() {
                return;
            }
            if session.passive_arm_delay {
                session.passive_arm_delay = false;
                return;
            }
            match self.data.state() {
                State::Closed => {
                    let _ = poll_once(self.data.accept(DATA_PORT));
                    self.data_listening = true;
                }
                State::TimeWait
                | State::LastAck
                | State::FinWait1
                | State::FinWait2
                | State::CloseWait => self.data.abort(),
                _ => {}
            }
        }

        // Nothing to transfer: keep the passive socket armed if PASV was
        // given, otherwise reset it.
        if session.pending.is_none() && session.transfer.is_none() {
            if !session.passive_requested && self.data_listening {
                self.data.abort();
                self.data_listening = false;
            }
            self.activity.clear();
            return;
        }

        // Accept phase: the data connection has not arrived yet.
        if let Some(pending) = session.pending.take() {
            if self.data.state() == State::Established {
                match self.start_transfer(volume, session, &pending) {
                    Ok(greeting) => {
                        session.passive_requested = false;
                        session.reply(&greeting);
                    }
                    Err(reply) => {
                        session.transfer = None;
                        session.reply(&reply);
                        self.abort_data();
                    }
                }
            } else if Instant::now() - pending.since
                >= esp_hal::time::Duration::from_secs(DATA_ACCEPT_TIMEOUT_SECS)
            {
                session.reply("425 No data connection");
                self.abort_data();
            } else {
                session.pending = Some(pending);
            }
            return;
        }

        let Some(mut transfer) = session.transfer.take() else {
            return;
        };

        // The preliminary 150 reply must leave the control connection first.
        if !transfer.announced {
            if session.out.is_empty() && self.control.send_queue() == 0 {
                transfer.announced = true;
            }
            session.transfer = Some(transfer);
            return;
        }

        // All outbound bytes are queued. Close immediately: smoltcp sends FIN
        // *after* the queued payload, giving the client its required
        // data-channel EOF.
        if transfer.finishing || transfer.failed.is_some() {
            let failed = transfer.failed.is_some();
            if failed {
                self.data.abort();
                session.reply("426 Transfer aborted");
                session.reply("226 Data closed");
                log::warn!("FTP transfer failed: {}", transfer.topic);
            } else {
                self.data.close();
                session.reply(&format!("226 Finished {}", transfer.topic));
                log::info!("FTP transfer finished: {}", transfer.topic);
            }
            self.data_listening = false;
            return;
        }

        self.step_transfer(volume, &mut transfer, session);
        self.activity = transfer.topic.clone();
        session.transfer = Some(transfer);
    }

    fn abort_data(&mut self) {
        self.data.abort();
        self.data_listening = false;
    }

    /// Validate the requested transfer once the data connection is up and
    /// produce the 150 greeting. No handles survive this call.
    fn start_transfer(
        &mut self,
        volume: &SdVolume,
        session: &mut Session,
        pending: &Pending,
    ) -> Result<String, String> {
        match &pending.action {
            PendingAction::Retrieve {
                parent,
                leaf,
                offset,
            } => {
                let (entry, _dir) = find_in(volume, parent, leaf).ok_or_else(not_found)?;
                if entry.is_directory() {
                    return Err(String::from("550 Is a directory"));
                }
                let length = u64::from(entry.len());
                if *offset > length || *offset > u64::from(u32::MAX) {
                    return Err(String::from("550 Bad restart offset"));
                }
                session.transfer = Some(Transfer {
                    topic: format!("RETR {leaf}"),
                    kind: TransferKind::Retrieve {
                        parent: parent.clone(),
                        leaf: leaf.clone(),
                        offset: *offset,
                        cursor: None,
                    },
                    announced: false,
                    chunk: Vec::new(),
                    chunk_sent: 0,
                    finishing: false,
                    failed: None,
                });
                Ok(format!("150 Sending {} bytes", length - offset))
            }
            PendingAction::Store { parent, leaf } => {
                if leaf.trim().is_empty() {
                    return Err(String::from("501 Empty file name"));
                }
                if open_dir_path(volume, parent).is_none() {
                    return Err(String::from("550 Path not found"));
                }
                session.transfer = Some(Transfer {
                    topic: format!("STOR {leaf}"),
                    kind: TransferKind::Store {
                        parent: parent.clone(),
                        leaf: leaf.clone(),
                        initialized: false,
                        cursor: None,
                    },
                    announced: false,
                    chunk: Vec::new(),
                    chunk_sent: 0,
                    finishing: false,
                    failed: None,
                });
                Ok(format!("150 Ready for {leaf}"))
            }
            PendingAction::List { parent, leaf, kind } => {
                if let Some(leaf) = leaf {
                    let (entry, _dir) = find_in(volume, parent, leaf).ok_or_else(not_found)?;
                    let line = listing_line(*kind, &entry);
                    let mut chunk = Vec::with_capacity(line.len() + 2);
                    chunk.extend_from_slice(line.as_bytes());
                    chunk.extend_from_slice(b"\r\n");
                    session.transfer = Some(Transfer {
                        topic: format!("LIST {leaf}"),
                        kind: TransferKind::List {
                            parent: parent.clone(),
                            kind: *kind,
                            skip: 0,
                            done: true,
                        },
                        announced: false,
                        chunk,
                        chunk_sent: 0,
                        finishing: false,
                        failed: None,
                    });
                } else {
                    if open_dir_path(volume, parent).is_none() {
                        return Err(String::from("550 No such directory"));
                    }
                    session.transfer = Some(Transfer {
                        topic: format!("LIST /{}", parent.join("/")),
                        kind: TransferKind::List {
                            parent: parent.clone(),
                            kind: *kind,
                            skip: 0,
                            done: false,
                        },
                        announced: false,
                        chunk: Vec::new(),
                        chunk_sent: 0,
                        finishing: false,
                        failed: None,
                    });
                }
                Ok(String::from("150 Listing"))
            }
        }
    }

    /// One slice of work per poll: either send queued bytes or produce more.
    fn step_transfer(&mut self, volume: &SdVolume, transfer: &mut Transfer, session: &mut Session) {
        let socket_dead = !self.data.may_send() && !self.data.may_recv();

        // Send what is already queued.
        if transfer.chunk_sent < transfer.chunk.len() {
            if self.data.can_send() {
                match poll_once(self.data.write(&transfer.chunk[transfer.chunk_sent..])) {
                    Some(Ok(count)) if count > 0 => {
                        transfer.chunk_sent += count;
                        self.stats.sent_bytes += count as u64;
                        session.last_activity = Instant::now();
                    }
                    Some(Ok(_)) | None => {}
                    Some(Err(_)) => transfer.failed = Some("pipe"),
                }
            } else if socket_dead {
                transfer.failed = Some("pipe");
            }
            if transfer.chunk_sent >= transfer.chunk.len() {
                transfer.chunk.clear();
                transfer.chunk_sent = 0;
            }
            return;
        }

        match &mut transfer.kind {
            TransferKind::List {
                parent,
                kind,
                skip,
                done,
                ..
            } => {
                if *done {
                    transfer.finishing = true;
                    return;
                }
                match fill_listing(volume, parent, *kind, *skip, &mut transfer.chunk) {
                    Ok(produced) => {
                        *skip += produced;
                        *done = produced == 0;
                        transfer.chunk_sent = 0;
                    }
                    Err(()) => transfer.failed = Some("list"),
                }
            }
            TransferKind::Retrieve {
                parent,
                leaf,
                offset,
                cursor,
            } => {
                match read_file_slice(volume, parent, leaf, *offset, cursor, &mut transfer.chunk) {
                    Ok(read) => {
                        *offset += read as u64;
                        transfer.chunk_sent = 0;
                        if read == 0 {
                            transfer.finishing = true;
                        }
                    }
                    Err(()) => transfer.failed = Some("read"),
                }
            }
            TransferKind::Store {
                parent,
                leaf,
                initialized,
                cursor,
            } => {
                // Receive first: the client pushes the stream.
                if self.data.can_recv() {
                    let mut buffer = [0_u8; FILE_CHUNK_LEN];
                    match poll_once(self.data.read(&mut buffer)) {
                        Some(Ok(count)) if count > 0 => {
                            match append_file_slice(
                                volume,
                                parent,
                                leaf,
                                *initialized,
                                cursor.take(),
                                &buffer[..count],
                            ) {
                                Ok(next_cursor) => {
                                    *initialized = true;
                                    *cursor = Some(next_cursor);
                                    self.stats.received_bytes += count as u64;
                                    session.last_activity = Instant::now();
                                }
                                Err(()) => transfer.failed = Some("write"),
                            }
                        }
                        Some(Err(_)) => transfer.failed = Some("pipe"),
                        _ => {
                            // EOF (Ok(0)) — fall through to the finishing check.
                        }
                    }
                }
                // A passive data connection half-closed by the client means
                // upload complete: smoltcp reports may_send but not may_recv.
                if !self.data.may_recv() && !self.data.can_recv() && transfer.failed.is_none() {
                    transfer.finishing = *initialized;
                    if !*initialized {
                        // Client closed without bytes; drop the empty stub.
                        let _ = delete_named(volume, parent, leaf);
                        transfer.failed = Some("empty");
                    }
                }
            }
        }

        if transfer.failed.is_none()
            && socket_dead
            && !matches!(transfer.kind, TransferKind::Store { .. })
        {
            transfer.failed = Some("pipe");
        }
    }

    fn handle_command_line(
        &mut self,
        volume: &SdVolume,
        session: &mut Session,
        line: &[u8],
        config: &FtpConfig,
    ) {
        let Ok(text) = core::str::from_utf8(line) else {
            session.reply("500 Bad encoding");
            return;
        };
        let text = text.trim();
        if text.is_empty() {
            session.reply("500 Empty command");
            return;
        }
        let (command, argument) = match text.split_once(' ') {
            Some((command, rest)) => (command, rest.trim()),
            None => (text, ""),
        };
        let command = command.to_ascii_uppercase();
        let arg = argument;
        // Options like `LIST -la` select the current directory.
        let arg = strip_list_options(&command, arg);
        log::debug!("FTP cmd {command} {arg}");

        match command.as_str() {
            "USER" => {
                if arg.eq_ignore_ascii_case(&config.user) {
                    session.user = Some(arg.to_string());
                    session.reply("331 Password required");
                } else {
                    let _ = arg;
                    session.user = Some(String::new());
                    session.reply("331 Password required");
                }
            }
            "PASS" => {
                if session
                    .user
                    .as_ref()
                    .is_some_and(|user| !user.is_empty())
                    && arg == config.password
                {
                    session.logged_in = true;
                    session.reply("230 Logged in, SD card shared");
                } else {
                    session.logged_in = false;
                    session.reply("530 Login failed");
                }
            }
            "SYST" => session.reply("215 UNIX Type: L8"),
            "FEAT" => {
                session.reply("211 Features:");
                session.reply("MLST;MLSD;SIZE;MDTM;REST STREAM;UTF8");
                session.reply("211 End");
            }
            "OPTS" => session.reply("200 OK"),
            "NOOP" => session.reply("200 OK"),
            "HELP" => session.reply("214 USER PASS QUIT PWD CWD CDUP PASV EPSV LIST NLST MLSD MLST SIZE MDTM RETR STOR DELE RMD MKD RNFR RNTO REST ABOR"),
            "QUIT" => {
                session.reply("221 Bye");
                session.quit = true;
            }
            "ABOR" if session.transfer.is_some() || session.pending.is_some() => {
                session.pending = None;
                let _ = session.transfer.take();
                self.abort_data();
                session.reply("226 Transfer aborted");
            }
            _ if !session.logged_in => session.reply("530 Please login"),
            "ABOR" => session.reply("226 Nothing to abort"),
            // Data-transfer prefixes: always schedule through PASV/EPSV.
            "LIST" | "NLST" | "MLSD" | "RETR" | "STOR" | "MLST" => {
                if session.pending.is_some() || session.transfer.is_some() {
                    session.reply("425 Transfer already in progress");
                    return;
                }
                if !session.passive_requested {
                    session.reply("425 Use PASV or EPSV first");
                    return;
                }
                // The passive listener may already have accepted the client.
                // Keep it; only PASV/EPSV aborts it when arming anew.
                let action = match command.as_str() {
                    "RETR" => {
                        let Some((parent, leaf)) = need_leaf(&session.cwd, arg) else {
                            session.reply("501 File name required");
                            return;
                        };
                        PendingAction::Retrieve {
                            parent,
                            leaf,
                            offset: session.rest.take().unwrap_or(0),
                        }
                    }
                    "STOR" => {
                        let Some((parent, leaf)) = need_leaf(&session.cwd, arg) else {
                            session.reply("501 File name required");
                            return;
                        };
                        session.rest = None;
                        PendingAction::Store { parent, leaf }
                    }
                    kind_cmd => {
                        session.rest = None;
                        let kind = match kind_cmd {
                            "NLST" => ListKind::Names,
                            "MLSD" | "MLST" => ListKind::Machine,
                            _ => ListKind::List,
                        };
                        let (parent, leaf) = split_path(&session.cwd, arg);
                        if kind_cmd == "MLST" {
                            PendingAction::List {
                                parent: session.cwd.clone(),
                                leaf: None,
                                kind,
                            }
                        } else {
                            PendingAction::List { parent, leaf, kind }
                        }
                    }
                };
                session.pending = Some(Pending {
                    action,
                    since: Instant::now(),
                });
            }
            "PWD" | "XPWD" => {
                session.reply(&format!("257 \"/{}\" is current", session.cwd.join("/")));
            }
            "CWD" | "XCWD" => {
                let target = normalize(&session.cwd, arg);
                if open_dir_path(volume, &target).is_some() {
                    session.cwd = target;
                    session.reply("250 Directory changed");
                } else {
                    session.reply("550 No such directory");
                }
            }
            "CDUP" | "XCUP" => {
                session.cwd.pop();
                session.reply("250 Directory changed");
            }
            "TYPE" => session.reply("200 Binary mode (transfer is always 8-bit)"),
            "MODE" | "STRU" => session.reply("200 OK"),
            "PORT" | "EPRT" => session.reply("502 Passive mode only"),
            "PASV" => match self.ip {
                Some(ip) => {
                    session.pending = None;
                    self.abort_data();
                    session.passive_requested = true;
                    session.passive_arm_delay = true;
                    let [a, b, c, d] = ip.octets();
                    session.reply(&format!(
                        "227 Entering Passive Mode ({a},{b},{c},{d},{},{})",
                        DATA_PORT / 256,
                        DATA_PORT % 256
                    ));
                }
                None => session.reply("425 No IP yet"),
            },
            "EPSV" => match self.ip {
                Some(_) => {
                    session.pending = None;
                    self.abort_data();
                    session.passive_requested = true;
                    session.passive_arm_delay = true;
                    session.reply(&format!("229 (|||{DATA_PORT}|)"));
                }
                None => session.reply("425 No IP yet"),
            },
            "REST" => match arg.parse::<u64>() {
                Ok(offset) => {
                    session.rest = Some(offset);
                    session.reply(&format!("350 Restart at {offset}"));
                }
                Err(_) => session.reply("501 Bad restart offset"),
            },
            "SIZE" => match find_for(&session.cwd, volume, arg) {
                Some(entry) if !entry.is_directory() => {
                    session.reply(&format!("213 {}", entry.len()));
                }
                Some(_) => session.reply("550 Is a directory"),
                None => session.reply(&not_found()),
            },
            "MDTM" => match find_for(&session.cwd, volume, arg) {
                Some(entry) => session.reply(&fmt_fat_mtime_prefixed(&entry)),
                None => session.reply(&not_found()),
            },
            "DELE" => {
                let Some((parent, leaf)) = need_leaf(&session.cwd, arg) else {
                    session.reply("501 File name required");
                    return;
                };
                match find_in(volume, &parent, &leaf) {
                    Some((entry, dir)) if entry.is_directory() => {
                        let _ = dir;
                        session.reply("550 Is a directory");
                    }
                    Some((entry, _dir)) => {
                        if volume.delete(&entry).is_ok() {
                            session.reply("250 Deleted");
                        } else {
                            session.reply("550 Cannot delete");
                        }
                    }
                    None => session.reply(&not_found()),
                }
            }
            "RMD" | "XRMD" => {
                let (parent, leaf) = match need_leaf(&session.cwd, arg) {
                    Some(path) => path,
                    None => {
                        session.reply("501 Directory name required");
                        return;
                    }
                };
                let target = normalize(&session.cwd, arg);
                if path_covers_cwd(&session.cwd, &target) {
                    session.reply("550 Cannot remove current directory");
                    return;
                }
                match find_in(volume, &parent, &leaf) {
                    Some((entry, _dir)) if entry.is_directory() => {
                        if volume.delete(&entry).is_ok() {
                            session.reply("250 Directory removed");
                        } else {
                            session.reply("550 Not empty or not found");
                        }
                    }
                    Some(_) => session.reply("550 Not a directory"),
                    None => session.reply(&not_found()),
                }
            }
            "MKD" | "XMKD" => {
                let (parent, leaf) = match need_leaf(&session.cwd, arg) {
                    Some(path) => path,
                    None => {
                        session.reply("501 Directory name required");
                        return;
                    }
                };
                let created = open_dir_path(volume, &parent).is_some_and(|dir| {
                    let result = volume.create_dir(&dir, &leaf);
                    result.is_ok()
                });
                if created {
                    session.reply(&format!(
                        "257 \"/{}\" created",
                        normalize(&session.cwd, arg).join("/")
                    ));
                } else {
                    session.reply("550 Cannot create directory");
                }
            }
            "RNFR" => {
                let Some((parent, leaf)) = need_leaf(&session.cwd, arg) else {
                    session.reply("501 File name required");
                    return;
                };
                if find_in(volume, &parent, &leaf).is_some() {
                    session.rename_from = Some((parent, leaf));
                    session.reply("350 Ready for RNTO");
                } else {
                    session.rename_from = None;
                    session.reply(&not_found());
                }
            }
            "RNTO" => {
                let Some((src_parent, src_leaf)) = session.rename_from.take() else {
                    session.reply("503 RNFR first");
                    return;
                };
                let Some((dst_parent, dst_leaf)) = need_leaf(&session.cwd, arg) else {
                    session.reply("501 Target name required");
                    return;
                };
                let renamed = (|| {
                    let (entry, _dir) = find_in(volume, &src_parent, &src_leaf)?;
                    let dest = open_dir_path(volume, &dst_parent)?;
                    volume.rename(&entry, &dest, &dst_leaf).ok()
                })();
                if renamed.is_some() {
                    session.reply("250 Renamed");
                } else {
                    session.reply("550 Rename failed");
                }
            }
            "SITE" | "CHMOD" => session.reply("502 Not implemented"),
            _ => session.reply("502 Not implemented"),
        }
    }
}

fn fmt_fat_mtime_prefixed(entry: &FileEntry) -> String {
    format!("213 {}", fmt_fat_mtime(entry.modified()))
}

fn not_found() -> String {
    String::from("550 No such file or directory")
}

/// Open the directory at absolute components. Handle is borrowed for this poll.
fn open_dir_path<'a>(volume: &'a SdVolume, components: &[String]) -> Option<FatDir<'a, SdBlock>> {
    let mut current = volume.root_dir();
    for component in components {
        current = current.open_dir(component).ok()?;
    }
    Some(current)
}

/// Find an entry by name inside a directory addressed by components.
fn find_in<'a>(
    volume: &'a SdVolume,
    parent: &[String],
    leaf: &str,
) -> Option<(FileEntry, FatDir<'a, SdBlock>)> {
    let dir = open_dir_path(volume, parent)?;
    let entry = dir.find(leaf).ok().flatten()?;
    Some((entry, dir))
}

fn find_for(cwd: &[String], volume: &SdVolume, arg: &str) -> Option<FileEntry> {
    let (parent, leaf) = split_path(cwd, arg);
    find_in(volume, &parent, &leaf?).map(|(entry, _)| entry)
}

/// Delete by name — used for STOR overwrite preparation and empty-STOR cleanup.
fn delete_named(volume: &SdVolume, parent: &[String], leaf: &str) -> Result<(), ()> {
    let (entry, _dir) = find_in(volume, parent, leaf).ok_or(())?;
    volume.delete(&entry).map_err(|_| ())
}

/// Read up to `FILE_CHUNK_LEN` bytes from a file at `offset`, appending them
/// to `chunk` (which must be empty). 0 bytes at offset means end of file.
fn read_file_slice(
    volume: &SdVolume,
    parent: &[String],
    leaf: &str,
    offset: u64,
    cursor: &mut Option<ReadCursor>,
    chunk: &mut Vec<u8>,
) -> Result<usize, ()> {
    let dir = open_dir_path(volume, parent).ok_or(())?;
    let entry = dir.find(leaf).map_err(|_| ())?.ok_or(())?;
    if entry.is_directory() {
        return Err(());
    }
    let mut reader = if let Some(cursor) = cursor.take() {
        FileReader::new_from_cursor(volume, &entry, cursor).map_err(|_| ())?
    } else {
        let mut reader = FileReader::new(volume, &entry).map_err(|_| ())?;
        reader.seek(SeekFrom::Start(offset)).map_err(|_| ())?;
        reader
    };
    let mut buffer = [0_u8; FILE_CHUNK_LEN];
    let read = reader.read(&mut buffer).map_err(|_| ())?;
    *cursor = Some(reader.cursor());
    chunk.extend_from_slice(&buffer[..read]);
    Ok(read)
}

/// Append bytes to a file, creating it (or truncating an existing entry) on
/// the first call. The writer commits size and timestamps before return, so a
/// 226 later means data reached the card.
fn append_file_slice(
    volume: &SdVolume,
    parent: &[String],
    leaf: &str,
    initialized: bool,
    cursor: Option<AppendCursor>,
    data: &[u8],
) -> Result<AppendCursor, ()> {
    let dir = open_dir_path(volume, parent).ok_or(())?;
    let entry = match dir.find(leaf) {
        Ok(Some(entry)) if !entry.is_directory() => {
            if initialized {
                entry
            } else {
                // Overwrite: start from a fresh empty chain.
                volume.delete(&entry).map_err(|_| ())?;
                volume.create_file(&dir, leaf).map_err(|_| ())?
            }
        }
        Ok(Some(_)) => return Err(()), // directory collision
        Ok(None) if !initialized => volume.create_file(&dir, leaf).map_err(|_| ())?,
        Ok(None) | Err(_) => return Err(()),
    };
    let mut writer = if initialized {
        FileWriter::new_append_from_cursor(volume, &entry, cursor.ok_or(())?).map_err(|_| ())?
    } else {
        FileWriter::new(volume, &entry).map_err(|_| ())?
    };
    if writer.write(data).map_err(|_| ())? != data.len() {
        return Err(());
    }
    let next_cursor = writer.append_cursor();
    writer.finish().map_err(|_| ())?;
    Ok(next_cursor)
}

/// Append up to `LIST_CHUNK_TARGET` bytes of entry lines, skipping the first
/// `skip` entries. Returns how many entries were consumed; 0 = done.
fn fill_listing(
    volume: &SdVolume,
    parent: &[String],
    kind: ListKind,
    mut skip: usize,
    chunk: &mut Vec<u8>,
) -> Result<usize, ()> {
    chunk.clear();
    let dir = open_dir_path(volume, parent).ok_or(())?;
    let mut produced = 0_usize;
    let mut iter = dir.entries();
    loop {
        match iter.next_entry() {
            Some(Ok(hadris_fat::dir::DirectoryEntry::Entry(entry))) => {
                let name = entry.name().into_owned();
                if name == "." || name == ".." || name.contains('\r') || name.contains('\n') {
                    continue;
                }
                // `skip` counts visible entries, just like `produced`. Applying
                // it before filtering FAT's dot entries makes every follow-up
                // chunk repeat the final two visible names.
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let line = listing_line(kind, &entry);
                if chunk.len() + line.len() + 2 > LIST_CHUNK_TARGET && produced > 0 {
                    break;
                }
                chunk.extend_from_slice(line.as_bytes());
                chunk.extend_from_slice(b"\r\n");
                produced += 1;
            }
            Some(Err(_)) => return Err(()),
            None => break,
        }
    }
    Ok(produced)
}

/// Unix-ish factory line for LIST / NLST / MLSD.
fn listing_line(kind: ListKind, entry: &FileEntry) -> String {
    let name = entry.name();
    match kind {
        ListKind::Names => name.into_owned(),
        ListKind::Machine => {
            let entry_type = if entry.is_directory() { "dir" } else { "file" };
            format!(
                "Type={};Size={};Modify={};Perm=ftp; {}",
                entry_type,
                entry.len(),
                fmt_fat_mtime(entry.modified()),
                name
            )
        }
        ListKind::List => {
            let (year, month, day, hour, minute) = unpack_fat(entry.modified());
            let permissions = if entry.is_directory() {
                "drwxr-xr-x"
            } else {
                "-rw-r--r--"
            };
            let month_name = MONTHS[(month as usize).saturating_sub(1).min(11)];
            let current_year = now_ymdhms();
            let same_year = current_year.starts_with(&format!("{year:04}"));
            let when = if same_year {
                format!("{hour:02}:{minute:02}")
            } else {
                format!("{year:>5}")
            };
            format!(
                "{} 1 rat rat {:>13} {} {:>2} {} {}",
                permissions,
                entry.len(),
                month_name,
                day,
                when,
                name
            )
        }
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Unpack a FAT datetime into (year, month, day, hour, minute).
fn unpack_fat(dt: FatDateTime) -> (u16, u8, u8, u8, u8) {
    let (date, time, _) = dt.to_raw();
    (
        ((date >> 9) & 0x7F) + 1980,
        ((date >> 5) & 0x0F) as u8,
        (date & 0x1F) as u8,
        ((time >> 11) & 0x1F) as u8,
        ((time >> 5) & 0x3F) as u8,
    )
}

/// Split a path argument into (parent components, optional leaf name).
fn split_path(cwd: &[String], arg: &str) -> (Vec<String>, Option<String>) {
    let mut components = normalize(cwd, arg);
    match arg.trim() {
        "" | "." | "/" => (components, None),
        _ => {
            let leaf = components.pop();
            (components, leaf)
        }
    }
}

fn need_leaf(cwd: &[String], arg: &str) -> Option<(Vec<String>, String)> {
    let (parent, leaf) = split_path(cwd, arg);
    leaf.map(|leaf| (parent, leaf))
}

/// Normalize an FTP path relative to the session cwd, respecting `..`.
fn normalize(cwd: &[String], arg: &str) -> Vec<String> {
    let arg = arg.trim().trim_matches('"').replace('\\', "/");
    let mut result: Vec<String> = if arg.starts_with('/') {
        Vec::new()
    } else {
        cwd.to_vec()
    };
    for component in arg.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                result.pop();
            }
            name => result.push(name.to_string()),
        }
    }
    result
}

/// `true` when `target` is the current directory or one of its ancestors.
fn path_covers_cwd(cwd: &[String], target: &[String]) -> bool {
    target.len() <= cwd.len()
        && cwd
            .iter()
            .zip(target.iter())
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

/// `LIST -a`, `LIST -la`, `LIST --full-time` etc. all mean "plain listing".
fn strip_list_options<'a>(command: &str, arg: &'a str) -> &'a str {
    if !matches!(command, "LIST" | "NLST" | "MLSD") {
        return arg;
    }
    if arg.starts_with('-') {
        match arg.split_once(' ') {
            Some((_options, rest)) => rest.trim(),
            None => "",
        }
    } else {
        arg
    }
}

fn format_tcp_endpoint(addr: IpAddress, port: u16) -> String {
    let IpAddress::Ipv4(ip) = addr;
    format!(
        "{}.{}.{}.{}:{port}",
        ip.octets()[0],
        ip.octets()[1],
        ip.octets()[2],
        ip.octets()[3]
    )
}
