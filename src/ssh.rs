//! SSH server: one session on TCP port 22, driven from the main loop.
//!
//! `sunset` is sans-io: the application feeds it received TCP bytes
//! (`Runner::input`), takes bytes to send (`Runner::output_buf`) and reacts
//! to events from `Runner::progress()` (host key request, authentication,
//! session/channel requests). That fits this firmware's executor-less loop
//! the same way the FTP server does: every socket future is polled once with
//! a no-op waker, guarded by `can_recv()`/`can_send()`, and nothing blocks.
//!
//! The only expensive step is key exchange (X25519 + an Ed25519 signature),
//! which runs synchronously inside one `progress()` call.
//!
//! This stage offers a tiny command shell (`help`, `status`, `ping`,
//! `exit`) and the SFTP subsystem. Logins use the server credentials from
//! `WIFI.CFG` (password only). The server accepts connections only while its
//! SSH+SFTP screen is open, exactly like the FTP server it replaces; its
//! socket buffers are still reserved at boot so restarting never allocates.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll, Waker};

use embassy_net::tcp::{State, TcpSocket};
use embassy_net::Stack;
use embassy_time::Duration as NetDuration;
use esp_hal::time::{Duration, Instant};
use sha2::{Digest, Sha256};
use sunset::{ChanData, ChanFail, ChanHandle, Event, Runner, ServEvent, Server, SignKey};

use crate::sftp::Sftp;
use crate::storage::{self, ClockConfig, SdVolume, ServerConfig};

const SSH_PORT: u16 = 22;
/// TCP buffers, leaked once at boot.
const SOCKET_RX_LEN: usize = 8192;
const SOCKET_TX_LEN: usize = 8192;
/// sunset packet buffers. They must hold the largest packet either side
/// sends; sunset advertises 1000-byte channel packets, and OpenSSH's
/// handshake packets (KEXINIT, user auth) stay well below 4 KiB.
const PACKET_BUF_LEN: usize = 4096;
/// `/RATPUTER/SSHHOST.KEY`: the raw 32-byte Ed25519 seed of the host key.
const HOSTKEY_FILE: &str = "SSHHOST.KEY";
/// Unauthenticated connections are dropped after this long.
const AUTH_TIMEOUT: Duration = Duration::from_secs(60);
/// Authenticated sessions without any traffic are dropped after this long.
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// Log an SSH poll slower than this (see `poll`).
const SLOW_POLL_MS: u64 = 500;
/// Time a gracefully closed connection may linger in FIN states.
const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// Bound the work done per main-loop pass.
const MAX_PROGRESS_STEPS: usize = 16;
const MAX_LINE: usize = 128;
/// Socket/protocol/channel rounds per poll (see `poll_session`).
const MAX_ROUNDS: usize = 8;
/// SFTP process/reply steps per round.
const SFTP_STEPS: usize = 8;

// The packet buffers live in .bss, not on the heap. `Runner` borrows them for
// `'static`; `BUFFERS_IN_USE` guarantees a single borrower at a time.
static mut INBUF: [u8; PACKET_BUF_LEN] = [0; PACKET_BUF_LEN];
static mut OUTBUF: [u8; PACKET_BUF_LEN] = [0; PACKET_BUF_LEN];
static BUFFERS_IN_USE: AtomicBool = AtomicBool::new(false);

/// getrandom 0.4 custom backend (selected in `.cargo/config.toml`). The
/// hardware RNG is truly random while the radio is on; the SSH server only
/// runs with Wi-Fi associated, and the host key is generated then too.
#[no_mangle]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    // SAFETY: getrandom passes a valid, writable buffer of `len` bytes.
    let buffer = unsafe { core::slice::from_raw_parts_mut(dest, len) };
    esp_hal::rng::Rng::new().read(buffer);
    Ok(())
}

/// Poll a future exactly once with a no-op waker. `None` = would block.
fn poll_once<F: Future>(future: F) -> Option<F::Output> {
    let mut future = core::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(output) => Some(output),
        Poll::Pending => None,
    }
}

/// Values the shell's `status` command reports; filled in by the main loop.
pub struct ShellStatus {
    pub heap_free: usize,
    pub heap_free_min: usize,
    pub uptime_ms: u64,
    pub loop_max_ms: u64,
}

struct Session {
    runner: Runner<'static, Server>,
    shell: Shell,
}

/// Everything except the runner, so event handlers (which borrow the runner)
/// can still update session state.
struct Shell {
    peer: String,
    channel: Option<ChanHandle>,
    authenticated: bool,
    /// Terminal requested: echo input and translate newlines.
    pty: bool,
    /// `ssh host command`: close the channel once the reply is sent.
    close_after_reply: bool,
    line: Vec<u8>,
    reply: Vec<u8>,
    started: Instant,
    last_activity: Instant,
    /// The host key signature has been logged (handshake timing).
    kex_done_logged: bool,
    /// `ssh host command`: run `line` on the next channel poll.
    exec_pending: bool,
    /// Reported to the client when the channel closes (127 = unknown).
    exit_status: u32,
    /// The channel runs the `sftp` subsystem instead of the shell.
    sftp: Option<Box<Sftp>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // The runner borrowed the static buffers; it is dropped right after
        // this, before any new session can take them.
        BUFFERS_IN_USE.store(false, Ordering::Release);
    }
}

pub struct SshServer {
    socket: TcpSocket<'static>,
    /// The server accepts connections only while its screen is open (like the
    /// old FTP screen); buffers stay reserved so restarting never allocates.
    active: bool,
    session: Option<Box<Session>>,
    hostkey: Option<SignKey>,
    fingerprint: String,
    /// A generated key is waiting for the SD card (USB DISK owned it).
    unsaved_seed: Option<[u8; 32]>,
    pub sessions_total: u32,
    /// Graceful close started; teardown states are given `CLOSE_GRACE`.
    closing_since: Option<Instant>,
}

impl SshServer {
    /// Create the listener at boot, while the heap is still unfragmented, and
    /// load the persisted host key if one exists.
    pub fn new(stack: Stack<'static>, volume: Option<&SdVolume>) -> Self {
        let mut socket = TcpSocket::new(
            stack,
            Box::leak(Box::new([0_u8; SOCKET_RX_LEN])),
            Box::leak(Box::new([0_u8; SOCKET_TX_LEN])),
        );
        socket.set_timeout(Some(NetDuration::from_secs(120)));
        let mut server = Self {
            socket,
            active: false,
            session: None,
            hostkey: None,
            fingerprint: String::new(),
            unsaved_seed: None,
            sessions_total: 0,
            closing_since: None,
        };
        if let Some(volume) = volume {
            match storage::read_config_file(volume, HOSTKEY_FILE, 64) {
                Ok(bytes) if bytes.len() == 32 => {
                    let mut seed = [0_u8; 32];
                    seed.copy_from_slice(&bytes);
                    server.install_hostkey(&seed);
                    log::info!("SSH host key loaded: {}", server.fingerprint);
                }
                Ok(_) => log::warn!("SSH host key file has the wrong size; ignoring it"),
                Err(_) => log::info!("No SSH host key yet; one is generated on first use"),
            }
        }
        server
    }

    fn install_hostkey(&mut self, seed: &[u8; 32]) {
        let key = ed25519_dalek::SigningKey::from_bytes(seed);
        self.fingerprint = fingerprint(key.verifying_key().as_bytes());
        self.hostkey = Some(SignKey::Ed25519(key));
    }

    /// Generate a key (radio is up, so the RNG is a TRNG) and persist it.
    /// Called on the first connection and from the screen once Wi-Fi is up,
    /// so the fingerprint can be verified before the first login.
    pub fn ensure_hostkey(&mut self, volume: Option<&SdVolume>) {
        if self.hostkey.is_none() {
            let mut seed = [0_u8; 32];
            esp_hal::rng::Rng::new().read(&mut seed);
            self.install_hostkey(&seed);
            self.unsaved_seed = Some(seed);
            log::info!("SSH host key generated: {}", self.fingerprint);
        }
        if let (Some(seed), Some(volume)) = (self.unsaved_seed, volume) {
            match storage::write_config_file(volume, HOSTKEY_FILE, &seed) {
                Ok(()) => {
                    self.unsaved_seed = None;
                    log::info!("SSH host key saved to /RATPUTER/{HOSTKEY_FILE}");
                }
                Err(error) => log::warn!("SSH host key not saved: {error:?}"),
            }
        }
    }

    /// Start accepting connections (the SSH+SFTP screen was opened).
    pub fn start(&mut self) {
        self.active = true;
    }

    /// Stop the listener and end any session. The client sees a TCP reset;
    /// SFTP writes were already committed per chunk, so partial files stay
    /// valid. Buffers remain reserved, the server can start again at once.
    pub fn stop(&mut self) {
        self.active = false;
        if self.session.take().is_some() {
            log::info!("SSH session dropped: the SSH+SFTP screen was closed");
        }
        self.closing_since = None;
        self.socket.abort();
    }

    /// The screen is open: the listener or a session needs polling.
    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn fingerprint(&self) -> &str {
        if self.fingerprint.is_empty() {
            "none"
        } else {
            &self.fingerprint
        }
    }

    /// The connected peer's `ip:port`, for the screen and STATUS.
    pub fn peer(&self) -> Option<&str> {
        self.session
            .as_ref()
            .map(|session| session.shell.peer.as_str())
    }

    /// Short state for STATUS and the UI.
    pub fn state(&self) -> String {
        if !self.active {
            return String::from("off");
        }
        match &self.session {
            None if self.socket.state() == State::Listen => String::from("listening"),
            None => format!("{:?}", self.socket.state()).to_ascii_lowercase(),
            Some(session) if session.shell.authenticated => {
                format!("session peer={}", session.shell.peer)
            }
            Some(session) => format!("handshake peer={}", session.shell.peer),
        }
    }

    /// A session is open: the main loop should poll quickly.
    pub fn busy(&self) -> bool {
        self.session.is_some()
    }

    /// Advance the listener and the session once. Never blocks.
    pub fn poll(&mut self, context: &PollContext<'_>) {
        if !self.active {
            return;
        }
        if self.session.is_none() {
            self.poll_listener(context.volume);
        }
        let Some(mut session) = self.session.take() else {
            return;
        };
        let poll_started = Instant::now();
        let keep = self.poll_session(&mut session, context);
        // Diagnostics: one poll should cost one card chunk at most. Expected
        // outliers are key exchange (~0.13 s) and replacing a large file on
        // OPEN with TRUNC (its whole cluster chain is freed at once).
        let poll_ms = poll_started.elapsed().as_millis();
        if poll_ms > SLOW_POLL_MS {
            log::warn!(
                "Slow SSH poll: {poll_ms} ms (authenticated={} sftp={})",
                session.shell.authenticated,
                session.shell.sftp.is_some()
            );
        }
        if keep {
            self.session = Some(session);
        } else {
            if let Some(sftp) = session.shell.sftp.as_ref() {
                log::info!(
                    "SFTP session ended: {} requests, {} B read, {} B written",
                    sftp.stats.requests,
                    sftp.stats.bytes_read,
                    sftp.stats.bytes_written
                );
            }
            log::info!(
                "SSH session from {} closed after {} ms",
                session.shell.peer,
                session.shell.started.elapsed().as_millis()
            );
            // FIN rather than RST: the client may still be reading our last
            // packets. `poll_listener` aborts if the teardown stalls.
            self.socket.close();
            self.closing_since = Some(Instant::now());
            drop(session);
        }
    }

    /// An SFTP transfer is in flight: the main loop should burst-poll.
    pub fn transferring(&self) -> bool {
        self.session
            .as_ref()
            .and_then(|session| session.shell.sftp.as_ref())
            .is_some_and(|sftp| sftp.transferring())
    }

    /// SFTP holds open files or directories: the SD card must not move to
    /// USB DISK under it.
    pub fn holds_sd(&self) -> bool {
        self.session
            .as_ref()
            .and_then(|session| session.shell.sftp.as_ref())
            .is_some_and(|sftp| sftp.has_open_handles() || sftp.transferring())
    }

    fn poll_listener(&mut self, volume: Option<&SdVolume>) {
        match self.socket.state() {
            State::Closed => {
                self.closing_since = None;
                // listen() is idempotent on an already-listening socket.
                let _ = poll_once(self.socket.accept(SSH_PORT));
            }
            State::Established => {
                if BUFFERS_IN_USE.swap(true, Ordering::Acquire) {
                    self.socket.abort();
                    return;
                }
                self.ensure_hostkey(volume);
                // SAFETY: BUFFERS_IN_USE was false, so no other Runner holds
                // the buffers; `Session::drop` clears the flag again.
                let (inbuf, outbuf) = unsafe {
                    (
                        &mut *core::ptr::addr_of_mut!(INBUF),
                        &mut *core::ptr::addr_of_mut!(OUTBUF),
                    )
                };
                let peer = self
                    .socket
                    .remote_endpoint()
                    .map(|endpoint| format!("{}", endpoint))
                    .unwrap_or_else(|| String::from("?"));
                log::info!("SSH connection from {peer}");
                let now = Instant::now();
                self.sessions_total += 1;
                self.session = Some(Box::new(Session {
                    runner: Runner::new_server(inbuf, outbuf),
                    shell: Shell {
                        peer,
                        channel: None,
                        authenticated: false,
                        pty: false,
                        close_after_reply: false,
                        line: Vec::new(),
                        reply: Vec::new(),
                        started: now,
                        last_activity: now,
                        kex_done_logged: false,
                        exec_pending: false,
                        exit_status: 0,
                        sftp: None,
                    },
                }));
            }
            State::Listen | State::SynReceived => {}
            _ if self
                .closing_since
                .is_some_and(|since| since.elapsed() < CLOSE_GRACE) => {}
            _ => {
                self.closing_since = None;
                self.socket.abort();
            }
        }
    }

    /// Returns `false` when the session is over.
    fn poll_session(&mut self, session: &mut Session, context: &PollContext<'_>) -> bool {
        let now = Instant::now();
        if !session.shell.authenticated && now - session.shell.started > AUTH_TIMEOUT {
            log::warn!("SSH: authentication timeout for {}", session.shell.peer);
            return false;
        }
        if now - session.shell.last_activity > IDLE_TIMEOUT {
            log::info!("SSH: idle timeout for {}", session.shell.peer);
            return false;
        }
        if let Some(sftp) = session.shell.sftp.as_mut() {
            sftp.begin_poll();
        }
        // Several rounds per call: the sunset output buffer holds only one
        // packet, so during a transfer it must be drained to TCP often.
        for _ in 0..MAX_ROUNDS {
            match self.round(session, context) {
                Ok(true) => {}
                Ok(false) => break,
                Err(()) => return false,
            }
        }
        if !self.socket.may_send() && !self.socket.may_recv() {
            return false;
        }
        true
    }

    /// One pass of socket input, protocol progress, channel I/O and socket
    /// output. `Ok(true)` if anything moved, `Err` to end the session.
    fn round(&mut self, session: &mut Session, context: &PollContext<'_>) -> Result<bool, ()> {
        let now = Instant::now();
        let mut moved = false;

        // 1. Network -> sunset, straight out of the socket's receive ring.
        if self.socket.can_recv() && session.runner.is_input_ready() {
            let runner = &mut session.runner;
            match poll_once(self.socket.read_with(|data| match runner.input(data) {
                Ok(used) => (used, Ok(used)),
                Err(error) => (0, Err(error)),
            })) {
                Some(Ok(Ok(used))) if used > 0 => {
                    session.shell.last_activity = now;
                    moved = true;
                }
                Some(Ok(Ok(_))) | None => {}
                Some(Ok(Err(error))) => {
                    log::warn!("SSH input error from {}: {error:?}", session.shell.peer);
                    return Err(());
                }
                Some(Err(_)) => return Err(()),
            }
        }
        if !self.socket.may_recv() && !self.socket.can_recv() {
            session.runner.close_input();
        }

        // 2. Protocol progress and events.
        for _ in 0..MAX_PROGRESS_STEPS {
            let event = match session.runner.progress() {
                Ok(event) => event,
                Err(error) => {
                    log::warn!("SSH protocol error from {}: {error:?}", session.shell.peer);
                    return Err(());
                }
            };
            match event {
                Event::None => break,
                Event::Progressed => moved = true,
                Event::Cli(_) => return Err(()),
                Event::Serv(event) => {
                    moved = true;
                    let Some(hostkey) = self.hostkey.as_ref() else {
                        return Err(());
                    };
                    match handle_event(event, hostkey, context.credentials, &mut session.shell) {
                        Ok(true) => {}
                        Ok(false) => return Err(()),
                        Err(error) => {
                            log::warn!("SSH event error: {error:?}");
                            return Err(());
                        }
                    }
                }
            }
        }

        // 3. Channel data in both directions.
        if session.shell.sftp.is_some() {
            if channel_sftp(session, context)? {
                session.shell.last_activity = now;
                moved = true;
            }
        } else if channel_shell(session, context.status)? {
            session.shell.last_activity = now;
            moved = true;
        }

        // 4. sunset -> network.
        while self.socket.can_send() {
            let pending = session.runner.output_buf();
            if pending.is_empty() {
                break;
            }
            match poll_once(self.socket.write(pending)) {
                Some(Ok(written)) if written > 0 => {
                    session.runner.consume_output(written);
                    moved = true;
                }
                Some(Ok(_)) | None => break,
                Some(Err(_)) => return Err(()),
            }
        }
        Ok(moved)
    }
}

/// Everything a poll needs from the main loop.
pub struct PollContext<'a> {
    pub volume: Option<&'a SdVolume>,
    pub credentials: &'a ServerConfig,
    pub clock: &'a ClockConfig,
    pub status: &'a dyn Fn() -> ShellStatus,
}

/// Interactive shell / exec channel. `Ok(true)` if bytes moved.
fn channel_shell(session: &mut Session, status: &dyn Fn() -> ShellStatus) -> Result<bool, ()> {
    let mut moved = false;
    if session.shell.exec_pending {
        session.shell.exec_pending = false;
        let line = core::mem::take(&mut session.shell.line);
        run_command(&mut session.shell, &line, status);
        moved = true;
    }
    let Some(channel) = session.shell.channel.as_ref() else {
        return Ok(moved);
    };
    let mut input = [0_u8; 128];
    match session
        .runner
        .read_channel(channel, ChanData::Normal, &mut input)
    {
        Ok(0) => {}
        Ok(count) => {
            moved = true;
            shell_input(&mut session.shell, &input[..count], status);
        }
        Err(sunset::Error::ChannelEOF) => session.shell.close_after_reply = true,
        Err(error) => {
            log::warn!("SSH channel read error: {error:?}");
            return Err(());
        }
    }
    let Some(channel) = session.shell.channel.as_ref() else {
        return Ok(moved);
    };
    if !session.shell.reply.is_empty() {
        match session
            .runner
            .write_channel(channel, ChanData::Normal, &session.shell.reply)
        {
            Ok(written) => {
                moved |= written > 0;
                session.shell.reply.drain(..written);
            }
            Err(sunset::Error::ChannelEOF) => session.shell.reply.clear(),
            Err(error) => {
                log::warn!("SSH channel write error: {error:?}");
                return Err(());
            }
        }
    }
    if session.shell.close_after_reply && session.shell.reply.is_empty() {
        if let Some(channel) = session.shell.channel.take() {
            // Vendor patch 0001: sunset can now end the channel itself.
            let _ = session
                .runner
                .close_channel(&channel, Some(session.shell.exit_status));
            let _ = session.runner.channel_done(channel);
            moved = true;
        }
    }
    Ok(moved)
}

/// SFTP subsystem channel. `Ok(true)` if bytes moved.
fn channel_sftp(session: &mut Session, context: &PollContext<'_>) -> Result<bool, ()> {
    let Some(channel) = session.shell.channel.as_ref() else {
        return Ok(false);
    };
    let Some(sftp) = session.shell.sftp.as_mut() else {
        return Ok(false);
    };
    let mut moved = false;

    // Requests in, as far as the SFTP input bound allows. Not reading leaves
    // the data in sunset, which then stops extending the channel window.
    let space = sftp.input_space();
    if space > 0 {
        let mut input = [0_u8; 1024];
        let wanted = space.min(input.len());
        match session
            .runner
            .read_channel(channel, ChanData::Normal, &mut input[..wanted])
        {
            Ok(0) => {}
            Ok(count) => {
                sftp.feed(&input[..count]);
                moved = true;
            }
            Err(sunset::Error::ChannelEOF) => {
                // The client ended the subsystem (sftp "bye", scp done):
                // report success and close the channel like sftp-server.
                let stats = sftp.stats;
                log::info!(
                    "SFTP closed by client: {} requests, {} B read, {} B written, \
                     card read {} ms, card write {} ms, session {} ms",
                    stats.requests,
                    stats.bytes_read,
                    stats.bytes_written,
                    stats.card_read_us / 1000,
                    stats.card_write_us / 1000,
                    session.shell.started.elapsed().as_millis()
                );
                session.shell.sftp = None;
                if let Some(channel) = session.shell.channel.take() {
                    let _ = session.runner.close_channel(&channel, Some(0));
                    let _ = session.runner.channel_done(channel);
                }
                return Ok(true);
            }
            Err(error) => {
                log::warn!("SFTP channel read error: {error:?}");
                return Err(());
            }
        }
    }

    // Process, then send replies, until the channel cannot take more.
    for _ in 0..SFTP_STEPS {
        let stepped = sftp.step(context.volume, context.clock);
        let mut wrote = false;
        if !sftp.output().is_empty() {
            match session
                .runner
                .write_channel(channel, ChanData::Normal, sftp.output())
            {
                Ok(written) => {
                    wrote = written > 0;
                    sftp.consume_output(written);
                }
                Err(sunset::Error::ChannelEOF) => return Err(()),
                Err(error) => {
                    log::warn!("SFTP channel write error: {error:?}");
                    return Err(());
                }
            }
        }
        if sftp.is_fatal() {
            return Err(());
        }
        moved |= stepped | wrote;
        if !stepped && !wrote {
            break;
        }
        // A full sunset output buffer needs the socket before more fits.
        if !sftp.output().is_empty() && !wrote {
            break;
        }
    }
    Ok(moved)
}

/// Returns `Ok(false)` to end the session.
fn handle_event(
    event: ServEvent<'_, '_>,
    hostkey: &SignKey,
    credentials: &ServerConfig,
    session: &mut Shell,
) -> sunset::Result<bool> {
    match event {
        ServEvent::Hostkeys(request) => {
            request.hostkeys(&[hostkey])?;
            if !session.kex_done_logged {
                session.kex_done_logged = true;
                log::info!(
                    "SSH key exchange signed {} ms after connect",
                    session.started.elapsed().as_millis()
                );
            }
        }
        ServEvent::FirstAuth(mut request) => {
            // Password only for now; dropping the request rejects "none".
            request.set_auth_methods(true, false)?;
        }
        ServEvent::PasswordAuth(request) => {
            // Evaluate both comparisons so timing does not reveal which failed.
            let user_ok = request.matches_username(&credentials.user);
            let password_ok = request.matches_password(&credentials.password);
            if user_ok & password_ok {
                request.allow()?;
            } else {
                log::warn!("SSH: rejected password login from {}", session.peer);
                request.reject()?;
            }
        }
        ServEvent::PubkeyAuth(request) => request.reject()?,
        ServEvent::Authenticated => {
            session.authenticated = true;
            log::info!(
                "SSH: {} authenticated after {} ms",
                session.peer,
                session.started.elapsed().as_millis()
            );
        }
        ServEvent::OpenSession(request) => {
            if session.channel.is_none() {
                session.channel = Some(request.accept()?);
            } else {
                request.reject(ChanFail::SSH_OPEN_RESOURCE_SHORTAGE)?;
            }
        }
        ServEvent::SessionPty(request) => {
            session.pty = true;
            request.succeed()?;
        }
        ServEvent::SessionShell(request) => {
            request.succeed()?;
            push_text(session, "RATPUTER SSH - type 'help' for commands\n");
            push_prompt(session);
        }
        ServEvent::SessionExec(request) => {
            session.line = String::from(request.command().unwrap_or("")).into_bytes();
            request.succeed()?;
            // No terminal: plain newlines, no prompt, close after the reply.
            session.pty = false;
            session.close_after_reply = true;
            session.exec_pending = true;
        }
        ServEvent::SessionSubsystem(request) => {
            if request.command().is_ok_and(|name| name == "sftp") && session.sftp.is_none() {
                request.succeed()?;
                session.sftp = Some(Box::new(Sftp::new()));
                log::info!("SFTP subsystem started for {}", session.peer);
            } else {
                request.fail()?;
            }
        }
        ServEvent::SessionEnv(request) => request.fail()?,
        ServEvent::Defunct => return Ok(false),
        ServEvent::PollAgain => {}
    }
    Ok(true)
}

fn push_text(session: &mut Shell, text: &str) {
    for &byte in text.as_bytes() {
        if byte == b'\n' && session.pty {
            session.reply.push(b'\r');
        }
        session.reply.push(byte);
    }
}

fn push_prompt(session: &mut Shell) {
    if !session.close_after_reply {
        session.reply.extend_from_slice(b"rat$ ");
    }
}

/// Line editing (with echo on a terminal) and command execution.
fn shell_input(session: &mut Shell, bytes: &[u8], status: &dyn Fn() -> ShellStatus) {
    for &byte in bytes {
        match byte {
            b'\r' | b'\n' => {
                if session.pty {
                    session.reply.extend_from_slice(b"\r\n");
                }
                let line = core::mem::take(&mut session.line);
                run_command(session, &line, status);
            }
            0x08 | 0x7f => {
                if session.line.pop().is_some() && session.pty {
                    session.reply.extend_from_slice(b"\x08 \x08");
                }
            }
            0x03 => {
                session.line.clear();
                push_text(session, "^C\n");
                push_prompt(session);
            }
            0x04 => session.close_after_reply = true,
            0x20..=0x7e if session.line.len() < MAX_LINE => {
                session.line.push(byte);
                if session.pty {
                    session.reply.push(byte);
                }
            }
            _ => {}
        }
    }
}

fn run_command(session: &mut Shell, line: &[u8], status: &dyn Fn() -> ShellStatus) {
    let line = core::str::from_utf8(line).unwrap_or("").trim();
    session.exit_status = 0;
    let reply = match line.to_ascii_lowercase().as_str() {
        "" => String::new(),
        "help" => String::from("commands: help status ping exit\n"),
        "ping" => String::from("pong\n"),
        "status" => {
            let status = status();
            format!(
                "uptime_ms={} heap_free={} heap_free_min={} loop_max_ms={}\n",
                status.uptime_ms, status.heap_free, status.heap_free_min, status.loop_max_ms
            )
        }
        "exit" | "logout" => {
            session.close_after_reply = true;
            String::from("bye\n")
        }
        other => {
            session.exit_status = 127;
            format!("unknown command: {other}\n")
        }
    };
    push_text(session, &reply);
    push_prompt(session);
}

/// OpenSSH-style `SHA256:...` fingerprint of an Ed25519 public key.
fn fingerprint(public_key: &[u8; 32]) -> String {
    let mut blob = Vec::with_capacity(51);
    blob.extend_from_slice(&11_u32.to_be_bytes());
    blob.extend_from_slice(b"ssh-ed25519");
    blob.extend_from_slice(&32_u32.to_be_bytes());
    blob.extend_from_slice(public_key);
    let digest = Sha256::digest(&blob);
    let mut text = String::from("SHA256:");
    base64_unpadded(&digest, &mut text);
    text
}

fn base64_unpadded(bytes: &[u8], out: &mut String) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let value = chunk.iter().enumerate().fold(0_u32, |acc, (index, byte)| {
            acc | (u32::from(*byte) << (16 - 8 * index))
        });
        for index in 0..=chunk.len() {
            out.push(ALPHABET[((value >> (18 - 6 * index)) & 0x3f) as usize] as char);
        }
    }
}
