//! Minimal FTP server (RFC 959, passive mode only) exposing the SD card.
//!
//! There is no async executor: every socket call is a future polled exactly
//! once per step with a no-op waker, guarded by `can_recv()`/`can_send()`.
//! The main loop polls this module every iteration, and burst-polls it while a
//! transfer is in flight. One control connection at a time; the data channel
//! uses a fixed passive listener on port 20.
//!
//! Limits (deliberate, from `embedded-sdmmc`): uploads and MKD accept 8.3 short
//! names only (reading/listing long names works), no rename, and only empty
//! directories can be removed. Timestamps are local time from the NTP clock.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::future::Future;
use core::ops::ControlFlow;
use core::task::{Context, Poll, Waker};

use embassy_net::tcp::{State, TcpSocket};
use embassy_net::{IpAddress, Ipv4Address, Stack};
use embassy_time::Duration as NetDuration;
use embedded_sdmmc::{
    DirEntry, LfnBuffer, Mode, RawDirectory, RawFile, RawVolume, ShortFileName, Timestamp,
    VolumeIdx,
};
use esp_hal::time::Instant;

use crate::storage::{FtpConfig, SdVolumeManager};

pub const CONTROL_PORT: u16 = 21;
/// Fixed passive-mode data listener, right next to the control port.
const DATA_PORT: u16 = 20;
const CONTROL_RX_LEN: usize = 1024;
const CONTROL_TX_LEN: usize = 1024;
const DATA_RX_LEN: usize = 4096;
const DATA_TX_LEN: usize = 4096;
/// Directory listings are sent in runs of this size to keep RAM flat.
const LIST_CHUNK_TARGET: usize = 3072;
const MAX_COMMAND_LINE: usize = 512;
/// Close idle control connections after 5 minutes.
const IDLE_TIMEOUT_SECS: u64 = 300;
/// Give the client this long to open its data connection after PASV.
const DATA_ACCEPT_TIMEOUT_SECS: u64 = 20;
/// Worst-case UTF-8 expansion of a 255-code-unit FAT long name.
const LFN_UTF8_LEN: usize = 1024;

type Manager = SdVolumeManager;

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
        /// `None` = single-entry listing of one file (`done` after one chunk).
        dir: Option<RawDirectory>,
        kind: ListKind,
        /// Entries already consumed from the directory across chunks.
        skip: usize,
        done: bool,
        chunk: Vec<u8>,
        chunk_sent: usize,
    },
    Retrieve {
        file: RawFile,
    },
    Store {
        file: RawFile,
    },
}

struct Transfer {
    topic: String,
    kind: TransferKind,
    /// Wait until the preliminary 150 reply has left the control socket before
    /// sending any data (required FTP ordering).
    announced: bool,
    /// Data production ended; waiting for the TCP queue to drain.
    finishing: bool,
    failed: Option<&'static str>,
}

struct Session {
    cwd: Vec<String>,
    user: Option<String>,
    logged_in: bool,
    rest: Option<u64>,
    line: Vec<u8>,
    out: String,
    peer: String,
    pending: Option<Pending>,
    transfer: Option<Transfer>,
    /// PASV/EPSV given; waiting for the client to pick a data transfer.
    passive_requested: bool,
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
            line: Vec::new(),
            out: String::new(),
            peer,
            pending: None,
            transfer: None,
            passive_requested: false,
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
    volume: Option<RawVolume>,
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
    /// FTP screen; `start`/`stop` only own the volume.
    pub fn new(stack: Stack<'static>) -> Self {
        let control_rx = Box::leak(Box::new([0_u8; CONTROL_RX_LEN]));
        let control_tx = Box::leak(Box::new([0_u8; CONTROL_TX_LEN]));
        let data_rx = Box::leak(Box::new([0_u8; DATA_RX_LEN]));
        let data_tx = Box::leak(Box::new([0_u8; DATA_TX_LEN]));
        let mut control = TcpSocket::new(stack, control_rx, control_tx);
        let mut data = TcpSocket::new(stack, data_rx, data_tx);
        // Reclaim sockets wedged in FIN_WAIT instead of leaking the fixed port.
        control.set_timeout(Some(NetDuration::from_secs(IDLE_TIMEOUT_SECS + 60)));
        data.set_timeout(Some(NetDuration::from_secs(60)));
        Self {
            control,
            data,
            volume: None,
            session: None,
            control_listening: false,
            data_listening: false,
            ip: None,
            stats: FtpStats::default(),
            status: FtpStatus::Stopped,
            activity: String::new(),
        }
    }

    /// The caller must guarantee that the card is not exported over USB.
    pub fn start(&mut self, manager: &Manager, stack: Stack<'static>) -> Result<(), ()> {
        if self.volume.is_some() {
            return Ok(());
        }
        let volume = manager.open_raw_volume(VolumeIdx(0)).map_err(|_| ())?;
        self.volume = Some(volume);
        self.ip = stack.config_v4().map(|config| config.address.address());
        self.stats = FtpStats::default();
        self.status = FtpStatus::Offline;
        self.activity.clear();
        Ok(())
    }

    pub fn stop(&mut self, manager: &Manager) {
        self.end_session(manager);
        self.control.abort();
        self.data.abort();
        self.control_listening = false;
        self.data_listening = false;
        if let Some(volume) = self.volume.take() {
            let _ = manager.close_volume(volume);
        }
        self.ip = None;
        self.status = FtpStatus::Stopped;
        self.activity.clear();
    }

    pub fn status(&self) -> FtpStatus {
        self.status
    }

    pub fn ip(&self) -> Option<Ipv4Address> {
        self.ip
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

    fn end_session(&mut self, manager: &Manager) {
        if let Some(mut session) = self.session.take() {
            if let Some(volume) = self.volume {
                let _ = volume;
                Self::close_transfer(manager, &mut session);
            }
            self.control.close();
            self.control_listening = false;
            self.data.abort();
            self.data_listening = false;
        }
    }

    fn close_transfer(manager: &Manager, session: &mut Session) {
        session.pending = None;
        if let Some(mut transfer) = session.transfer.take() {
            match &mut transfer.kind {
                TransferKind::List { dir, .. } => {
                    if let Some(dir) = dir.take() {
                        let _ = manager.close_dir(dir);
                    }
                }
                TransferKind::Retrieve { file } | TransferKind::Store { file } => {
                    let _ = manager.close_file(*file);
                }
            }
        }
    }

    /// One non-blocking round of server work per main-loop iteration.
    pub fn poll(&mut self, manager: &Manager, stack: Stack<'static>, config: &FtpConfig) {
        if self.volume.is_none() {
            return;
        }
        self.ip = stack.config_v4().map(|config| config.address.address());
        let online = stack.is_link_up() && stack.is_config_up();

        if !online {
            if self.session.is_some() {
                log::warn!("FTP: Wi-Fi link lost, dropping the client");
                self.end_session(manager);
            } else if self.control_listening {
                self.control.abort();
                self.control_listening = false;
            }
            if self.data_listening {
                self.data.abort();
                self.data_listening = false;
            }
            self.status = FtpStatus::Offline;
            return;
        }

        // (Re)arm the control listener.
        if !self.control_listening {
            match self.control.state() {
                State::Closed => {
                    let _ = poll_once(self.control.accept(CONTROL_PORT));
                    self.control_listening = true;
                }
                // smoltcp refuses listen() from TIME_WAIT; abort skips it.
                State::TimeWait | State::LastAck | State::FinWait1 | State::FinWait2 => {
                    self.control.abort();
                }
                _ => {}
            }
        }

        if self.session.is_none() && self.control.state() == State::Established {
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
            .unwrap_or(FtpStatus::Listening);

        if self.session.is_some() {
            let mut session = self.session.take().unwrap();
            let dead = self.poll_session(manager, &mut session, config);
            if dead {
                if let Some(volume) = self.volume {
                    let _ = volume;
                }
                Self::close_transfer(manager, &mut session);
                if !matches!(
                    self.control.state(),
                    State::Closed | State::TimeWait | State::LastAck
                ) {
                    self.control.abort();
                }
                self.control_listening = false;
                self.data.abort();
                self.data_listening = false;
                log::info!("FTP client {} left", session.peer);
            } else {
                self.session = Some(session);
            }
        }
    }

    /// Returns `true` when the session is over and its handles are released.
    fn poll_session(
        &mut self,
        manager: &Manager,
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
                self.handle_command_line(manager, session, &line, config);
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

        // 5. Data channel and transfers.
        if !fatal {
            self.poll_data(manager, session);
        }

        // 6. Clean shutdown after QUIT: wait for queued replies to leave.
        if session.quit && session.out.is_empty() && self.control.send_queue() == 0 {
            self.control.close();
            self.control_listening = false;
            fatal = true;
        }

        // 7. The client hung up on us.
        if !session.quit
            && !self.control.may_send()
            && session.out.is_empty()
            && self.control.send_queue() == 0
        {
            fatal = true;
        }
        fatal
    }

    fn poll_data(&mut self, manager: &Manager, session: &mut Session) {
        // Nothing to transfer: keep the passive socket armed if PASV was given,
        // otherwise reset it.
        if session.pending.is_none() && session.transfer.is_none() {
            if session.passive_requested {
                if !self.data_listening {
                    match self.data.state() {
                        State::Closed => {
                            let _ = poll_once(self.data.accept(DATA_PORT));
                            self.data_listening = true;
                        }
                        // smoltcp cannot re-listen from teardown states.
                        State::TimeWait
                        | State::LastAck
                        | State::FinWait1
                        | State::FinWait2
                        | State::CloseWait => self.data.abort(),
                        _ => {}
                    }
                }
            } else if self.data_listening {
                self.data.abort();
                self.data_listening = false;
            }
            self.activity.clear();
            return;
        }

        // Accept phase: the data connection has not arrived yet.
        if let Some(pending) = session.pending.take() {
            if self.data.state() == State::Established {
                match self.start_transfer(manager, session, &pending) {
                    Ok(greeting) => {
                        session.passive_requested = false;
                        session.reply(&greeting);
                    }
                    Err(reply) => {
                        session.transfer = None;
                        session.reply(&reply);
                        Self::abort_data(&mut self.data, &mut self.data_listening);
                    }
                }
            } else if Instant::now() - pending.since
                >= esp_hal::time::Duration::from_secs(DATA_ACCEPT_TIMEOUT_SECS)
            {
                session.reply("425 No data connection");
                Self::abort_data(&mut self.data, &mut self.data_listening);
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

        // A completed/failed transfer finishes when the TCP queue is drained.
        if transfer.finishing || transfer.failed.is_some() {
            let drained = self.data.send_queue() == 0;
            let dead = !(self.data.may_send() || self.data.may_recv());
            if drained || dead {
                if transfer.failed.is_some() {
                    session.reply("426 Transfer aborted");
                    session.reply("226 Data closed");
                } else {
                    let mut line = String::from("226 Finished");
                    write!(line, " {}", transfer.topic).ok();
                    session.reply(&line);
                }
                match &mut transfer.kind {
                    TransferKind::List { dir, .. } => {
                        if let Some(dir) = dir.take() {
                            let _ = manager.close_dir(dir);
                        }
                    }
                    TransferKind::Retrieve { file } | TransferKind::Store { file } => {
                        let _ = manager.close_file(*file);
                    }
                }
                self.data.close();
                self.data_listening = false;
                return;
            }
            session.transfer = Some(transfer);
            return;
        }

        // Active transfer: one pump per poll.
        let socket_dead = !self.data.may_send() && !self.data.may_recv();
        match &mut transfer.kind {
            TransferKind::Retrieve { file } => {
                if self.data.can_send() {
                    match poll_once(self.data.write_with(
                        |buffer| match manager.read(*file, buffer) {
                            Ok(count) => (count, count),
                            Err(_) => (0, usize::MAX),
                        },
                    )) {
                        Some(Ok(usize::MAX)) | Some(Err(_)) => {
                            transfer.failed = Some("read");
                        }
                        Some(Ok(0)) => {
                            transfer.finishing = true;
                        }
                        Some(Ok(count)) => {
                            self.stats.sent_bytes += count as u64;
                        }
                        None => {}
                    }
                } else if socket_dead {
                    transfer.failed = Some("pipe");
                }
            }
            TransferKind::Store { file } => {
                while self.data.can_recv() {
                    match poll_once(self.data.read_with(
                        |buffer| match manager.write(*file, buffer) {
                            Ok(()) => (buffer.len(), buffer.len()),
                            Err(_) => (0, usize::MAX),
                        },
                    )) {
                        Some(Ok(usize::MAX)) | Some(Err(_)) => {
                            transfer.failed = Some("write");
                            while self.data.can_recv() {
                                let _ = poll_once(self.data.read_with(|buf| (buf.len(), ())));
                            }
                            break;
                        }
                        Some(Ok(count)) => {
                            self.stats.received_bytes += count as u64;
                        }
                        None => break,
                    }
                }
                if transfer.failed.is_none() && !self.data.may_recv() && !self.data.can_recv() {
                    transfer.finishing = true;
                }
            }
            TransferKind::List {
                dir,
                kind,
                skip,
                done,
                chunk,
                chunk_sent,
            } => {
                if *chunk_sent < chunk.len() {
                    if self.data.can_send() {
                        if let Some(Ok(count)) = poll_once(self.data.write(&chunk[*chunk_sent..])) {
                            *chunk_sent += count;
                            self.stats.sent_bytes += count as u64;
                        }
                    } else if socket_dead {
                        transfer.failed = Some("pipe");
                    }
                } else if *done {
                    transfer.finishing = true;
                } else if let Some(dir) = *dir {
                    match fill_listing(manager, dir, *kind, *skip, chunk) {
                        Ok(produced) => {
                            *skip += produced;
                            *done = produced == 0;
                            *chunk_sent = 0;
                        }
                        Err(()) => {
                            transfer.failed = Some("list");
                        }
                    }
                } else {
                    // Single-file listing: chunk was pre-filled once.
                    transfer.finishing = true;
                }
            }
        }
        if transfer.failed.is_none() && socket_dead && !transfer.finishing {
            transfer.failed = Some("pipe");
        }
        self.activity = transfer.topic.clone();
        session.transfer = Some(transfer);
    }

    fn abort_data(data: &mut TcpSocket<'static>, listening: &mut bool) {
        data.abort();
        *listening = false;
    }

    /// Open the transfer's file/directory once the data connection is up.
    fn start_transfer(
        &mut self,
        manager: &Manager,
        session: &mut Session,
        pending: &Pending,
    ) -> Result<String, String> {
        let volume = self.volume.unwrap();
        let _ = volume;
        match &pending.action {
            PendingAction::Retrieve {
                parent,
                leaf,
                offset,
            } => {
                let parent_dir = open_dir_path(manager, volume, parent)
                    .ok_or_else(|| String::from("550 Path not found"))?;
                let result = Self::open_read_transfer(manager, parent_dir, leaf, *offset);
                let _ = manager.close_dir(parent_dir);
                let (file, size) = result?;
                session.transfer = Some(Transfer {
                    topic: format!("RETR {leaf}"),
                    kind: TransferKind::Retrieve { file },
                    announced: false,
                    finishing: false,
                    failed: None,
                });
                Ok(format!("150 Sending {size} bytes"))
            }
            PendingAction::Store { parent, leaf } => {
                let parent_dir = open_dir_path(manager, volume, parent)
                    .ok_or_else(|| String::from("550 Path not found"))?;
                let result = Self::open_store_transfer(manager, parent_dir, leaf);
                let _ = manager.close_dir(parent_dir);
                let file = result?;
                session.transfer = Some(Transfer {
                    topic: format!("STOR {leaf}"),
                    kind: TransferKind::Store { file },
                    announced: false,
                    finishing: false,
                    failed: None,
                });
                Ok(format!("150 Ready for {leaf}"))
            }
            PendingAction::List { parent, leaf, kind } => {
                let setup = self.setup_listing(manager, parent, leaf.as_deref(), *kind)?;
                session.transfer = Some(Transfer {
                    topic: format!("LIST /{}", parent.join("/")),
                    kind: setup,
                    announced: false,
                    finishing: false,
                    failed: None,
                });
                Ok(String::from("150 Listing"))
            }
        }
    }

    fn open_read_transfer(
        manager: &Manager,
        parent: RawDirectory,
        leaf: &str,
        offset: u64,
    ) -> Result<(RawFile, u64), String> {
        let entry = find_entry(manager, parent, leaf).ok_or_else(not_found)?;
        if entry.attributes.is_directory() {
            return Err(String::from("550 Is a directory"));
        }
        let length = u64::from(entry.size);
        if offset > length || offset > u64::from(u32::MAX) {
            return Err(String::from("550 Bad restart offset"));
        }
        let file = manager
            .open_file_in_dir(parent, &entry.name, Mode::ReadOnly)
            .map_err(|_| not_found())?;
        if offset > 0 {
            manager
                .file_seek_from_start(file, offset as u32)
                .map_err(|_| {
                    let _ = manager.close_file(file);
                    String::from("550 Bad restart offset")
                })?;
        }
        Ok((file, length - offset))
    }

    fn open_store_transfer(
        manager: &Manager,
        parent: RawDirectory,
        leaf: &str,
    ) -> Result<RawFile, String> {
        if leaf.trim().is_empty() {
            return Err(String::from("501 Empty file name"));
        }
        let sfn = match find_entry(manager, parent, leaf) {
            Some(entry) if entry.attributes.is_directory() => {
                return Err(String::from("550 Is a directory"));
            }
            Some(entry) => entry.name,
            None => ShortFileName::create_from_str(leaf)
                .map_err(|_| String::from("553 Use 8.3 names (e.g. PHOTO.JPG)"))?,
        };
        manager
            .open_file_in_dir(parent, &sfn, Mode::ReadWriteCreateOrTruncate)
            .map_err(|_| String::from("550 Cannot create file"))
    }

    fn setup_listing(
        &mut self,
        manager: &Manager,
        parent: &[String],
        leaf: Option<&str>,
        kind: ListKind,
    ) -> Result<TransferKind, String> {
        let parent_dir = open_dir_path(manager, self.volume.unwrap(), parent)
            .ok_or_else(|| String::from("550 Path not found"))?;
        let result = match leaf {
            // LIST with no argument lists the directory itself.
            None => Ok(TransferKind::List {
                dir: Some(parent_dir),
                kind,
                skip: 0,
                done: false,
                chunk: Vec::new(),
                chunk_sent: 0,
            }),
            Some(name) => match find_entry(manager, parent_dir, name) {
                None => Err(String::from("550 Path not found")),
                Some(entry) if entry.attributes.is_directory() => {
                    let subdir = manager
                        .open_dir(parent_dir, &entry.name)
                        .map_err(|_| not_found())?;
                    let _ = manager.close_dir(parent_dir);
                    Ok(TransferKind::List {
                        dir: Some(subdir),
                        kind,
                        skip: 0,
                        done: false,
                        chunk: Vec::new(),
                        chunk_sent: 0,
                    })
                }
                Some(entry) => {
                    let _ = manager.close_dir(parent_dir);
                    // Single-file listing: emit one pre-formatted chunk.
                    let mut chunk = Vec::new();
                    let line = listing_line(kind, &entry, name);
                    chunk.extend_from_slice(line.as_bytes());
                    chunk.extend_from_slice(b"\r\n");
                    Ok(TransferKind::List {
                        dir: None,
                        kind,
                        skip: 0,
                        done: true,
                        chunk,
                        chunk_sent: 0,
                    })
                }
            },
        };
        match result {
            Err(err) => {
                let _ = manager.close_dir(parent_dir);
                Err(err)
            }
            ok => ok,
        }
    }
}

// ---------------------------------------------------------------------------
// Command dispatcher
// ---------------------------------------------------------------------------

/// Commands allowed before login (USER/PASS handled by the dispatcher).
const PRE_LOGIN: &[&str] = &[
    "SYST", "FEAT", "AUTH", "PBSZ", "PROT", "OPTS", "NOOP", "HELP", "QUIT", "STAT",
];

impl FtpServer {
    fn handle_command_line(
        &mut self,
        manager: &Manager,
        session: &mut Session,
        line: &[u8],
        config: &FtpConfig,
    ) {
        let Ok(text) = core::str::from_utf8(line) else {
            session.reply("500 Bad command encoding");
            return;
        };
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let (verb_raw, arg) = match text.find(' ') {
            Some(space) => (&text[..space], text[space + 1..].trim()),
            None => (text, ""),
        };
        let verb = verb_raw.to_ascii_uppercase();
        log::info!("FTP < {}", if verb == "PASS" { "PASS ***" } else { text });
        let volume = self.volume.unwrap();

        // While a transfer runs, only urgent/no-op commands are processed.
        if session.busy() {
            match verb.as_str() {
                "ABOR" => {
                    Self::close_transfer(manager, session);
                    Self::abort_data(&mut self.data, &mut self.data_listening);
                    session.reply("426 Transfer aborted by ABOR");
                    session.reply("226 Abort done");
                }
                "STAT" => {
                    let activity = self.activity.clone();
                    session.reply(&format!("211 {activity}"));
                }
                "NOOP" => session.reply("200 OK"),
                "QUIT" => session.quit = true,
                _ => session.reply("450 Transfer in progress"),
            }
            return;
        }

        match verb.as_str() {
            "USER" => {
                session.user = Some(arg.to_string());
                session.logged_in = false;
                session.reply("331 Password required");
                return;
            }
            "PASS" => {
                let user_ok = session.user.as_deref() == Some(config.user.as_str());
                if user_ok && arg == config.password {
                    session.logged_in = true;
                    session.reply("230 Logged in, SD card shared");
                    log::info!("FTP: {} logged in", session.peer);
                } else {
                    session.reply("530 Wrong user or password");
                }
                return;
            }
            _ => {}
        }

        if !session.logged_in && !PRE_LOGIN.contains(&verb.as_str()) {
            session.reply("530 Log in first");
            return;
        }

        match verb.as_str() {
            "QUIT" => {
                session.reply("221 Bye");
                session.quit = true;
            }
            "SYST" => session.reply("215 UNIX Type: L8"),
            "FEAT" => {
                session.reply("211-Features:");
                session.reply(" UTF8");
                session.reply(" MLSD");
                session.reply(" MDTM");
                session.reply(" SIZE");
                session.reply(" REST STREAM");
                session.reply(" PASV");
                session.reply("211 End");
            }
            "OPTS" => session.reply("200 OK"),
            // Plain FTP only; no TLS. Tell clients to stop negotiating security.
            "AUTH" | "PBSZ" | "PROT" => session.reply("502 Security not supported"),
            "NOOP" => session.reply("200 OK"),
            "HELP" => {
                session.reply("214-Commands: USER PASS PWD CWD CDUP LIST NLST MLSD MLST");
                session.reply(" RETR STOR DELE RMD MKD SIZE MDTM REST PASV EPSV QUIT");
                session.reply("214 Uploads need 8.3 names; no rename");
            }
            "STAT" => {
                session.reply(&format!(
                    "211 OK, sent {} B, received {} B",
                    self.stats.sent_bytes, self.stats.received_bytes
                ));
            }
            "PWD" | "XPWD" => {
                session.reply(&format!("257 \"/{}\" is current", session.cwd.join("/")));
            }
            "CWD" | "XCWD" => {
                let target = normalize(&session.cwd, arg);
                match open_dir_path(manager, volume, &target) {
                    Some(dir) => {
                        let _ = manager.close_dir(dir);
                        session.cwd = target;
                        session.reply("250 Directory changed");
                    }
                    None => session.reply("550 No such directory"),
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
                    Self::abort_data(&mut self.data, &mut self.data_listening);
                    session.passive_requested = true;
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
                    Self::abort_data(&mut self.data, &mut self.data_listening);
                    session.passive_requested = true;
                    session.reply(&format!("229 (|||{DATA_PORT}|)"));
                }
                None => session.reply("425 No IP yet"),
            },
            "LIST" | "NLST" | "MLSD" => {
                let path_arg = if verb == "LIST" || verb == "NLST" {
                    strip_list_options(arg)
                } else {
                    arg
                };
                let (parent, leaf) = split_path(&session.cwd, path_arg);
                if !session.passive_requested {
                    session.reply("425 Use PASV or EPSV first");
                    return;
                }
                let kind = match verb.as_str() {
                    "NLST" => ListKind::Names,
                    "MLSD" => ListKind::Machine,
                    _ => ListKind::List,
                };
                session.pending = Some(Pending {
                    action: PendingAction::List { parent, leaf, kind },
                    since: Instant::now(),
                });
            }
            "MLST" => {
                let (parent, leaf) = split_path(&session.cwd, arg);
                match open_dir_path(manager, volume, &parent) {
                    None => session.reply("550 Path not found"),
                    Some(dir) => {
                        let result = match &leaf {
                            None => Some(format!(
                                "Type=dir;Modify={}; /{}",
                                now_ymdhms(),
                                parent.join("/")
                            )),
                            Some(name) => find_entry(manager, dir, name)
                                .map(|entry| listing_line(ListKind::Machine, &entry, name)),
                        };
                        let _ = manager.close_dir(dir);
                        match result {
                            Some(line) => {
                                session.reply(&format!("250- /{}", parent.join("/")));
                                session.reply(&format!(" {line}"));
                                session.reply("250 End");
                            }
                            None => session.reply("550 No such file or directory"),
                        }
                    }
                }
            }
            "RETR" => {
                let (parent, leaf) = match need_leaf(&session.cwd, arg) {
                    Some(path) => path,
                    None => {
                        session.reply("501 File name required");
                        return;
                    }
                };
                if !session.passive_requested {
                    session.reply("425 Use PASV or EPSV first");
                    return;
                }
                let offset = session.rest.take().unwrap_or(0);
                session.pending = Some(Pending {
                    action: PendingAction::Retrieve {
                        parent,
                        leaf,
                        offset,
                    },
                    since: Instant::now(),
                });
            }
            "STOR" => {
                let (parent, leaf) = match need_leaf(&session.cwd, arg) {
                    Some(path) => path,
                    None => {
                        session.reply("501 File name required");
                        return;
                    }
                };
                if !session.passive_requested {
                    session.reply("425 Use PASV or EPSV first");
                    return;
                }
                if session.rest.take().is_some() {
                    session.reply("504 Cannot resume uploads");
                    return;
                }
                session.pending = Some(Pending {
                    action: PendingAction::Store { parent, leaf },
                    since: Instant::now(),
                });
            }
            "REST" => match arg.parse::<u64>() {
                Ok(offset) => {
                    session.rest = Some(offset);
                    session.reply("350 Restart stored");
                }
                Err(_) => session.reply("501 Bad offset"),
            },
            "SIZE" => match find_for(&session.cwd, manager, volume, arg) {
                Some(entry) if !entry.attributes.is_directory() => {
                    session.reply(&format!("213 {}", entry.size));
                }
                Some(_) => session.reply("550 Is a directory"),
                None => session.reply(&not_found()),
            },
            "MDTM" => match find_for(&session.cwd, manager, volume, arg) {
                Some(entry) => session.reply(&format!("213 {}", timestamp_ymdhms(entry.mtime))),
                None => session.reply(&not_found()),
            },
            "DELE" => {
                let Some((parent, leaf)) = need_leaf(&session.cwd, arg) else {
                    session.reply("501 File name required");
                    return;
                };
                let removed = open_dir_path(manager, volume, &parent).map(|dir| {
                    let result = match find_entry(manager, dir, &leaf) {
                        Some(entry) if entry.attributes.is_directory() => false,
                        Some(entry) => manager.delete_entry_in_dir(dir, entry.name).is_ok(),
                        None => false,
                    };
                    let _ = manager.close_dir(dir);
                    result
                });
                if removed == Some(true) {
                    session.reply("250 Deleted");
                } else {
                    session.reply("550 Cannot delete");
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
                let removed = open_dir_path(manager, volume, &parent).is_some_and(|dir| {
                    let entry = find_entry(manager, dir, &leaf)
                        .filter(|entry| entry.attributes.is_directory());
                    let ok = match entry {
                        Some(entry) => manager.delete_entry_in_dir(dir, entry.name).is_ok(),
                        None => false,
                    };
                    let _ = manager.close_dir(dir);
                    ok
                });
                if removed {
                    session.reply("250 Directory removed");
                } else {
                    session.reply("550 Not empty or not found");
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
                let created = open_dir_path(manager, volume, &parent).is_some_and(|dir| {
                    let ok = match ShortFileName::create_from_str(&leaf) {
                        Ok(name) => manager.make_dir_in_dir(dir, name).is_ok(),
                        Err(_) => false,
                    };
                    let _ = manager.close_dir(dir);
                    ok
                });
                if created {
                    session.reply(&format!(
                        "257 \"/{}\" created",
                        normalize(&session.cwd, arg).join("/")
                    ));
                } else if ShortFileName::create_from_str(&leaf).is_err() {
                    session.reply("553 Use 8.3 names for new directories");
                } else {
                    session.reply("550 Cannot create directory");
                }
            }
            "RNFR" | "RNTO" => session.reply("502 Rename not supported (8.3)"),
            "SITE" | "CHMOD" => session.reply("502 Not implemented"),
            _ => session.reply("502 Not implemented"),
        }
    }
}

/// Split a path argument into (parent components, optional leaf name).
fn split_path(cwd: &[String], arg: &str) -> (Vec<String>, Option<String>) {
    let mut components = normalize(cwd, arg);
    match arg.trim() {
        "" | "." => (components, None),
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

fn find_for(cwd: &[String], manager: &Manager, volume: RawVolume, arg: &str) -> Option<DirEntry> {
    let (parent, leaf) = split_path(cwd, arg);
    let leaf = leaf?;
    let dir = open_dir_path(manager, volume, &parent)?;
    let entry = find_entry(manager, dir, &leaf);
    let _ = manager.close_dir(dir);
    entry
}

/// Current local time for MLST/listing fallbacks, 1980-01-01 before NTP sync.
fn now_ymdhms() -> String {
    match crate::storage::local_now_seconds() {
        Some(now) => {
            let time = crate::clock::date_time(now);
            format!(
                "{:04}{:02}{:02}{:02}{:02}{:02}",
                time.year, time.month, time.day, time.hour, time.minute, time.second
            )
        }
        None => String::from("19800101000000"),
    }
}

fn not_found() -> String {
    String::from("550 No such file or directory")
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

fn strip_list_options(mut arg: &str) -> &str {
    arg = arg.trim();
    while arg.starts_with('-') {
        arg = arg
            .split_once(' ')
            .map(|(_, rest)| rest.trim_start())
            .unwrap_or("");
    }
    arg
}

// ---------------------------------------------------------------------------
// Path and directory helpers
// ---------------------------------------------------------------------------

/// Normalize `arg` against `cwd`: absolute when it starts with `/`, `..` pops.
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

/// Open the directory at absolute components. The caller must close it.
fn open_dir_path(
    manager: &Manager,
    volume: RawVolume,
    components: &[String],
) -> Option<RawDirectory> {
    let mut current = manager.open_root_dir(volume).ok()?;
    for component in components {
        let entry = find_entry(manager, current, component);
        let next = match entry {
            Some(entry) if entry.attributes.is_directory() => {
                manager.open_dir(current, &entry.name).ok()
            }
            _ => None,
        };
        let _ = manager.close_dir(current);
        current = next?;
    }
    Some(current)
}

/// Find an entry by long or short name (case-insensitive) in an open directory.
fn find_entry(manager: &Manager, dir: RawDirectory, name: &str) -> Option<DirEntry> {
    let mut found = None;
    let mut storage = [0_u8; LFN_UTF8_LEN];
    let mut lfn = LfnBuffer::new(&mut storage);
    let mut short = String::new();
    let _ = manager.iterate_dir_lfn(dir, &mut lfn, |entry, long| {
        if found.is_some() || entry.attributes.is_volume() {
            return ControlFlow::Continue(());
        }
        short.clear();
        write!(short, "{}", entry.name).ok();
        let matches_short = short.eq_ignore_ascii_case(name);
        let matches_long = matches!(long, Some(long_name) if long_name.eq_ignore_ascii_case(name));
        if matches_short || matches_long {
            found = Some(entry.clone());
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    found
}

/// `true` when `target` is the current directory or one of its ancestors.
fn path_covers_cwd(cwd: &[String], target: &[String]) -> bool {
    target.len() <= cwd.len()
        && cwd
            .iter()
            .zip(target.iter())
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

// ---------------------------------------------------------------------------
// Listing formatting
// ---------------------------------------------------------------------------

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn timestamp_ymdhms(time: Timestamp) -> String {
    format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        i32::from(time.year_since_1970) + 1970,
        time.zero_indexed_month + 1,
        time.zero_indexed_day + 1,
        time.hours,
        time.minutes,
        time.seconds
    )
}

/// Format one listing line; the year/time rule follows the BSD ls convention.
fn listing_line(kind: ListKind, entry: &DirEntry, name: &str) -> String {
    let is_dir = entry.attributes.is_directory();
    match kind {
        ListKind::Names => name.to_string(),
        ListKind::Machine => {
            let mut line = String::new();
            if is_dir {
                line.push_str("Type=dir;");
            } else {
                write!(line, "Type=file;Size={};", entry.size).ok();
            }
            write!(line, "Modify={}; {}", timestamp_ymdhms(entry.mtime), name).ok();
            line
        }
        ListKind::List => {
            let permissions = if is_dir { "drwxr-xr-x" } else { "-rw-r--r--" };
            let month = MONTHS[(entry.mtime.zero_indexed_month.min(11)) as usize];
            // RFC 959 has no date rules; hosts use the BSD convention where a
            // file from a different year shows the year instead of the time.
            let year = i32::from(entry.mtime.year_since_1970) + 1970;
            let when = match crate::storage::local_now_seconds() {
                Some(now) => {
                    let current = crate::clock::date_time(now).year as i32;
                    if current == year {
                        format!("{:02}:{:02}", entry.mtime.hours, entry.mtime.minutes)
                    } else {
                        format!("{year:>5}")
                    }
                }
                None => format!("{year:>5}"),
            };
            format!(
                "{} 1 rat rat {:>13} {} {:>2} {} {}",
                permissions,
                entry.size,
                month,
                entry.mtime.zero_indexed_day + 1,
                when,
                name
            )
        }
    }
}

/// Append up to `LIST_CHUNK_TARGET` bytes of entry lines, skipping the first
/// `skip` directory entries. Returns how many entries were consumed; 0 = done.
fn fill_listing(
    manager: &Manager,
    dir: RawDirectory,
    kind: ListKind,
    mut skip: usize,
    chunk: &mut Vec<u8>,
) -> Result<usize, ()> {
    let mut produced = 0_usize;
    let mut storage = [0_u8; LFN_UTF8_LEN];
    let mut lfn = LfnBuffer::new(&mut storage);
    let mut short = String::new();
    manager
        .iterate_dir_lfn(dir, &mut lfn, |entry, long| {
            if entry.attributes.is_volume() {
                return ControlFlow::Continue(());
            }
            short.clear();
            write!(short, "{}", entry.name).ok();
            if short == "." || short == ".." {
                return ControlFlow::Continue(());
            }
            if skip > 0 {
                skip -= 1;
                return ControlFlow::Continue(());
            }
            let name: String = match long {
                Some(long_name) => long_name.to_string(),
                None => short.clone(),
            };
            if name.contains('\r') || name.contains('\n') {
                return ControlFlow::Continue(());
            }
            let line = listing_line(kind, entry, &name);
            if chunk.len() + line.len() + 2 > LIST_CHUNK_TARGET && produced > 0 {
                return ControlFlow::Break(());
            }
            chunk.extend_from_slice(line.as_bytes());
            chunk.extend_from_slice(b"\r\n");
            produced += 1;
            ControlFlow::Continue(())
        })
        .map_err(|_| ())?;
    Ok(produced)
}
