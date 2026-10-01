//! SFTP version 3 server (draft-ietf-secsh-filexfer-02), the protocol behind
//! OpenSSH `sftp`/`scp`, WinSCP and FileZilla, on the SSH `sftp` subsystem.
//!
//! The module is sans-io like the SSH layer: `ssh.rs` feeds channel bytes in
//! (`feed`), calls `step` and drains `output`. Memory stays bounded although
//! clients send 32 KiB writes and ask for 32 KiB reads, many requests ahead:
//!
//! - Ordinary requests are parsed once complete (at most `MAX_REQUEST`).
//! - `WRITE` payloads are not buffered: after the fixed header, data goes to
//!   the card in chunks as it arrives (`WriteStream`).
//! - `DATA` replies are streamed from the card in chunks (`ReadStream`) after
//!   a header that declares the length up front.
//! - Nothing new is parsed while a reply is still being sent, so the SSH
//!   channel window (not this module) throttles a pipelining client.
//!
//! FAT handles borrow the volume and never outlive one `step`, exactly as in
//! the FTP server. Open SFTP handles keep paths, offsets and the validated
//! hadris-fat `ReadCursor`/`AppendCursor` instead.
//!
//! Limitations (FAT has no Unix metadata): writes must be sequential at the
//! end of the file (what every client does for uploads and resumes);
//! permissions and times in SETSTAT are accepted and ignored; truncation via
//! SETSTAT, links and extensions are unsupported.

use alloc::string::String;
use alloc::vec::Vec;

use hadris_fat::sync::{
    read::{FileReader, ReadCursor},
    write::{AppendCursor, FileWriter},
    FileEntry, SeekFrom,
};

use crate::clock;
use crate::fspath::{display, find_in, long_listing_line, open_dir_path, resolve};
use crate::storage::{ClockConfig, SdVolume};

// Packet types.
const FXP_INIT: u8 = 1;
const FXP_VERSION: u8 = 2;
const FXP_OPEN: u8 = 3;
const FXP_CLOSE: u8 = 4;
const FXP_READ: u8 = 5;
const FXP_WRITE: u8 = 6;
const FXP_LSTAT: u8 = 7;
const FXP_FSTAT: u8 = 8;
const FXP_SETSTAT: u8 = 9;
const FXP_FSETSTAT: u8 = 10;
const FXP_OPENDIR: u8 = 11;
const FXP_READDIR: u8 = 12;
const FXP_REMOVE: u8 = 13;
const FXP_MKDIR: u8 = 14;
const FXP_RMDIR: u8 = 15;
const FXP_REALPATH: u8 = 16;
const FXP_STAT: u8 = 17;
const FXP_RENAME: u8 = 18;
const FXP_STATUS: u8 = 101;
const FXP_HANDLE: u8 = 102;
const FXP_DATA: u8 = 103;
const FXP_NAME: u8 = 104;
const FXP_ATTRS: u8 = 105;

// Status codes.
const FX_OK: u32 = 0;
const FX_EOF: u32 = 1;
const FX_NO_SUCH_FILE: u32 = 2;
const FX_FAILURE: u32 = 4;
const FX_BAD_MESSAGE: u32 = 5;
const FX_OP_UNSUPPORTED: u32 = 8;

// OPEN flags.
const FXF_READ: u32 = 0x01;
const FXF_WRITE: u32 = 0x02;
const FXF_CREAT: u32 = 0x08;
const FXF_TRUNC: u32 = 0x10;
const FXF_EXCL: u32 = 0x20;

// Attribute flags.
const ATTR_SIZE: u32 = 0x01;
const ATTR_UIDGID: u32 = 0x02;
const ATTR_PERMISSIONS: u32 = 0x04;
const ATTR_ACMODTIME: u32 = 0x08;
const ATTR_EXTENDED: u32 = 0x8000_0000;

/// Largest non-WRITE request accepted (RENAME of two long VFAT paths fits).
const MAX_REQUEST: usize = 4096;
/// Input bytes held at most: one WRITE chunk plus the next request header
/// (or pipelined small requests).
const MAX_INPUT: usize = WRITE_CHUNK + 2048;
/// Initial request/reply buffer capacity (enough for metadata requests).
const SMALL_BUFFER: usize = 1024;
/// Reply buffer: one card chunk plus a DATA header, or a READDIR batch.
const MAX_OUTPUT: usize = IO_CHUNK + 64;
/// Largest `DATA` reply; clients ask again for the rest (short reads).
const MAX_READ_REPLY: u32 = 32 * 1024;
/// Card I/O per step.
const IO_CHUNK: usize = 8192;
/// Bytes of WRITE payload committed per step at most.
const WRITE_CHUNK: usize = 8192;
/// Open file and directory handles per session.
const MAX_HANDLES: usize = 8;
/// READDIR reply budget: entries and payload bytes.
const READDIR_MAX_ENTRIES: usize = 24;
const READDIR_MAX_BYTES: usize = 3072;

/// Error a request handler reports as an `SSH_FXP_STATUS`.
struct Status(u32, &'static str);

impl Status {
    fn failure(message: &'static str) -> Self {
        Self(FX_FAILURE, message)
    }
    fn no_such_file() -> Self {
        Self(FX_NO_SUCH_FILE, "No such file or directory")
    }
    fn bad_message() -> Self {
        Self(FX_BAD_MESSAGE, "Malformed request")
    }
}

type Reply = Result<(), Status>;

struct FileHandle {
    parent: Vec<String>,
    leaf: String,
    readable: bool,
    writable: bool,
    /// Committed size: the only offset a WRITE may target.
    size: u64,
    /// Validated tail position for the next WRITE (None before the first).
    append: Option<AppendCursor>,
    /// Cached reader position (offset, cursor) for sequential reads.
    read_at: Option<(u64, ReadCursor)>,
}

struct DirHandle {
    components: Vec<String>,
    /// Visible entries already returned.
    next: usize,
}

enum Handle {
    File(FileHandle),
    Dir(DirHandle),
}

/// A `DATA` reply whose header is already queued.
struct ReadStream {
    slot: usize,
    offset: u64,
    remaining: u32,
}

/// A `WRITE` request whose payload is still arriving.
struct WriteStream {
    id: u32,
    slot: usize,
    remaining: u32,
    /// First error; the rest of the payload is discarded.
    failed: Option<Status>,
}

#[derive(Clone, Copy, Default)]
pub struct SftpStats {
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub requests: u32,
    /// Microseconds spent in card reads / writes (the rest of a transfer is
    /// crypto, TCP and the main loop); logged when the session ends.
    pub card_read_us: u64,
    pub card_write_us: u64,
}

pub struct Sftp {
    input: Vec<u8>,
    out: Vec<u8>,
    read_stream: Option<ReadStream>,
    write_stream: Option<WriteStream>,
    /// Bytes of an oversized request still to discard.
    skip: usize,
    handles: [Option<Handle>; MAX_HANDLES],
    /// Mixed into handle strings so a closed handle is not mistaken for a
    /// later one in the same slot.
    generation: u32,
    fatal: bool,
    /// One card read or write per `begin_poll()`: a single 8 KiB FAT write
    /// can take tens of milliseconds, and many of them in one main-loop pass
    /// froze the UI for over a second.
    card_budget: bool,
    pub stats: SftpStats,
}

impl Sftp {
    pub fn new() -> Self {
        // Small buffers first; each grows to its full size in ONE allocation
        // when a transfer needs it (`reserve_full`). Doubling growth (12 KiB
        // input -> 16 KiB) fragmented the heap the Slint renderer needs
        // contiguous blocks from, and always reserving both full buffers cost
        // ~11 KiB more than a download-only session uses.
        Self {
            input: Vec::with_capacity(SMALL_BUFFER),
            out: Vec::with_capacity(SMALL_BUFFER),
            read_stream: None,
            write_stream: None,
            skip: 0,
            handles: Default::default(),
            generation: 0,
            fatal: false,
            card_budget: true,
            stats: SftpStats::default(),
        }
    }

    /// The peer violated the protocol in a way that cannot be answered
    /// (for example a DATA reply could not deliver its promised length).
    pub fn is_fatal(&self) -> bool {
        self.fatal
    }

    /// How many channel bytes the caller may read and `feed` now.
    pub fn input_space(&self) -> usize {
        MAX_INPUT.saturating_sub(self.input.len())
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        if self.input.len() + bytes.len() > self.input.capacity() {
            reserve_full(&mut self.input, MAX_INPUT);
        }
        self.input.extend_from_slice(bytes);
    }

    pub fn output(&self) -> &[u8] {
        &self.out
    }

    pub fn consume_output(&mut self, count: usize) {
        self.out.drain(..count.min(self.out.len()));
    }

    /// A transfer is in flight: worth burst-polling the network.
    pub fn transferring(&self) -> bool {
        self.read_stream.is_some()
            || self.write_stream.is_some()
            || !self.out.is_empty()
            || !self.input.is_empty()
    }

    /// Any file or directory handle is open (USB DISK must wait).
    pub fn has_open_handles(&self) -> bool {
        self.handles.iter().any(Option::is_some)
    }

    /// Allow one more card read or write (call once per SSH poll).
    pub fn begin_poll(&mut self) {
        self.card_budget = true;
    }

    /// Do one bounded unit of work. Returns `true` if anything happened.
    pub fn step(&mut self, volume: Option<&SdVolume>, clock: &ClockConfig) -> bool {
        if self.fatal {
            return false;
        }
        // Replies leave first: no parsing while output is pending, so a
        // pipelining client is throttled by the channel window.
        if !self.out.is_empty() {
            return false;
        }
        if self.read_stream.is_some() {
            if !self.card_budget {
                return false;
            }
            self.card_budget = false;
            self.continue_read(volume);
            return true;
        }
        if self.write_stream.is_some() {
            return self.continue_write(volume);
        }
        if self.skip > 0 {
            let count = self.skip.min(self.input.len());
            self.input.drain(..count);
            self.skip -= count;
            return count > 0;
        }
        self.parse_next(volume, clock)
    }

    // ---- request framing ------------------------------------------------

    fn parse_next(&mut self, volume: Option<&SdVolume>, clock: &ClockConfig) -> bool {
        if self.input.len() < 5 {
            return false;
        }
        let length =
            u32::from_be_bytes([self.input[0], self.input[1], self.input[2], self.input[3]])
                as usize;
        let kind = self.input[4];
        if length == 0 {
            self.fatal = true;
            return false;
        }
        if kind == FXP_WRITE {
            return self.begin_write(length);
        }
        if length > MAX_REQUEST {
            // Answer if the id is readable, then discard the body.
            if self.input.len() < 9 {
                return false;
            }
            let id =
                u32::from_be_bytes([self.input[5], self.input[6], self.input[7], self.input[8]]);
            self.status(id, FX_BAD_MESSAGE, "Request too large");
            let have = self.input.len().min(4 + length);
            self.input.drain(..have);
            self.skip = 4 + length - have;
            return true;
        }
        if self.input.len() < 4 + length {
            return false;
        }
        let packet: Vec<u8> = self.input.drain(..4 + length).collect();
        self.stats.requests += 1;
        self.dispatch(kind, &packet[5..], volume, clock);
        true
    }

    fn dispatch(&mut self, kind: u8, body: &[u8], volume: Option<&SdVolume>, clock: &ClockConfig) {
        if kind == FXP_INIT {
            // Reply version 3 whatever the client offers; no extensions.
            let start = self.begin(FXP_VERSION);
            put_u32(&mut self.out, 3);
            self.finish(start);
            return;
        }
        let mut reader = Reader::new(body);
        let Some(id) = reader.u32() else {
            self.fatal = true;
            return;
        };
        let result = match volume {
            None => Err(Status::failure("SD card is in use by USB DISK")),
            Some(volume) => match kind {
                FXP_REALPATH => self.realpath(id, &mut reader),
                FXP_STAT | FXP_LSTAT => self.stat_path(id, &mut reader, volume, clock),
                FXP_FSTAT => self.fstat(id, &mut reader, volume, clock),
                FXP_OPENDIR => self.opendir(id, &mut reader, volume),
                FXP_READDIR => self.readdir(id, &mut reader, volume, clock),
                FXP_OPEN => self.open(id, &mut reader, volume),
                FXP_CLOSE => self.close(id, &mut reader),
                FXP_READ => self.read(id, &mut reader, volume),
                FXP_REMOVE => self.remove(id, &mut reader, volume),
                FXP_MKDIR => self.mkdir(id, &mut reader, volume),
                FXP_RMDIR => self.rmdir(id, &mut reader, volume),
                FXP_RENAME => self.rename(id, &mut reader, volume),
                FXP_SETSTAT => self.setstat(id, &mut reader, volume),
                FXP_FSETSTAT => self.fsetstat(id, &mut reader),
                // READLINK, SYMLINK, EXTENDED and anything newer.
                _ => Err(Status(FX_OP_UNSUPPORTED, "Operation not supported")),
            },
        };
        if let Err(Status(code, message)) = result {
            self.status(id, code, message);
        }
    }

    // ---- replies ---------------------------------------------------------

    /// Start a reply: placeholder length + type. Returns its offset.
    fn begin(&mut self, kind: u8) -> usize {
        if kind == FXP_DATA || kind == FXP_NAME {
            reserve_full(&mut self.out, MAX_OUTPUT);
        }
        let start = self.out.len();
        put_u32(&mut self.out, 0);
        self.out.push(kind);
        start
    }

    fn finish(&mut self, start: usize) {
        let length = (self.out.len() - start - 4) as u32;
        self.out[start..start + 4].copy_from_slice(&length.to_be_bytes());
    }

    fn status(&mut self, id: u32, code: u32, message: &str) {
        let start = self.begin(FXP_STATUS);
        put_u32(&mut self.out, id);
        put_u32(&mut self.out, code);
        put_string(&mut self.out, message.as_bytes());
        put_string(&mut self.out, b"");
        self.finish(start);
    }

    fn ok(&mut self, id: u32) -> Reply {
        self.status(id, FX_OK, "OK");
        Ok(())
    }

    fn handle_reply(&mut self, id: u32, slot: usize) {
        let token = self.generation << 8 | slot as u32;
        let start = self.begin(FXP_HANDLE);
        put_u32(&mut self.out, id);
        put_string(&mut self.out, &token.to_be_bytes());
        self.finish(start);
    }

    // ---- handles ----------------------------------------------------------

    fn allocate(&mut self, handle: Handle) -> Result<usize, Status> {
        let slot = self
            .handles
            .iter()
            .position(Option::is_none)
            .ok_or(Status::failure("Too many open handles"))?;
        self.generation = self.generation.wrapping_add(1) & 0x00ff_ffff;
        self.handles[slot] = Some(handle);
        Ok(slot)
    }

    /// Validate a handle string from the client and return its slot.
    fn slot_of(&self, token: &[u8]) -> Result<usize, Status> {
        let bad = || Status::failure("Invalid handle");
        let token: [u8; 4] = token.try_into().map_err(|_| bad())?;
        let token = u32::from_be_bytes(token);
        let slot = (token & 0xff) as usize;
        if slot >= MAX_HANDLES || self.handles[slot].is_none() {
            return Err(bad());
        }
        Ok(slot)
    }

    fn file_mut(&mut self, slot: usize) -> Result<&mut FileHandle, Status> {
        match self.handles[slot].as_mut() {
            Some(Handle::File(file)) => Ok(file),
            _ => Err(Status::failure("Not a file handle")),
        }
    }

    // ---- request handlers --------------------------------------------------

    fn realpath(&mut self, id: u32, reader: &mut Reader) -> Reply {
        let path = reader.path().ok_or_else(Status::bad_message)?;
        let canonical = display(&resolve(&[], &path));
        let start = self.begin(FXP_NAME);
        put_u32(&mut self.out, id);
        put_u32(&mut self.out, 1);
        put_string(&mut self.out, canonical.as_bytes());
        put_string(&mut self.out, canonical.as_bytes());
        put_u32(&mut self.out, 0); // no attributes
        self.finish(start);
        Ok(())
    }

    fn stat_path(
        &mut self,
        id: u32,
        reader: &mut Reader,
        volume: &SdVolume,
        clock: &ClockConfig,
    ) -> Reply {
        let path = reader.path().ok_or_else(Status::bad_message)?;
        let components = resolve(&[], &path);
        self.stat_components(id, &components, volume, clock)
    }

    fn stat_components(
        &mut self,
        id: u32,
        components: &[String],
        volume: &SdVolume,
        clock: &ClockConfig,
    ) -> Reply {
        let attrs = match components.split_last() {
            None => Attrs::root(),
            Some((leaf, parent)) => {
                let (entry, _dir) =
                    find_in(volume, parent, leaf).ok_or_else(Status::no_such_file)?;
                Attrs::of(&entry, clock)
            }
        };
        let start = self.begin(FXP_ATTRS);
        put_u32(&mut self.out, id);
        attrs.encode(&mut self.out);
        self.finish(start);
        Ok(())
    }

    fn fstat(
        &mut self,
        id: u32,
        reader: &mut Reader,
        volume: &SdVolume,
        clock: &ClockConfig,
    ) -> Reply {
        let token = reader.string().ok_or_else(Status::bad_message)?;
        let slot = self.slot_of(token)?;
        let components = match self.handles[slot].as_ref() {
            Some(Handle::File(file)) => {
                let mut components = file.parent.clone();
                components.push(file.leaf.clone());
                components
            }
            Some(Handle::Dir(dir)) => dir.components.clone(),
            None => return Err(Status::failure("Invalid handle")),
        };
        self.stat_components(id, &components, volume, clock)
    }

    fn opendir(&mut self, id: u32, reader: &mut Reader, volume: &SdVolume) -> Reply {
        let path = reader.path().ok_or_else(Status::bad_message)?;
        let components = resolve(&[], &path);
        if open_dir_path(volume, &components).is_none() {
            return Err(match components.split_last() {
                Some((leaf, parent)) if find_in(volume, parent, leaf).is_some() => {
                    Status::failure("Not a directory")
                }
                _ => Status::no_such_file(),
            });
        }
        let slot = self.allocate(Handle::Dir(DirHandle {
            components,
            next: 0,
        }))?;
        self.handle_reply(id, slot);
        Ok(())
    }

    fn readdir(
        &mut self,
        id: u32,
        reader: &mut Reader,
        volume: &SdVolume,
        clock: &ClockConfig,
    ) -> Reply {
        let token = reader.string().ok_or_else(Status::bad_message)?;
        let slot = self.slot_of(token)?;
        let Some(Handle::Dir(dir)) = self.handles[slot].as_mut() else {
            return Err(Status::failure("Not a directory handle"));
        };
        let fat_dir = open_dir_path(volume, &dir.components).ok_or_else(Status::no_such_file)?;
        // Re-walk from the start and skip what was already sent: no FAT
        // handle may survive between steps.
        let mut entries = Vec::new();
        let mut count = 0_u32;
        let mut skip = dir.next;
        let mut iter = fat_dir.entries();
        loop {
            match iter.next_entry() {
                Some(Ok(hadris_fat::dir::DirectoryEntry::Entry(entry))) => {
                    let name = entry.name();
                    if name == "." || name == ".." {
                        continue;
                    }
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    if count as usize >= READDIR_MAX_ENTRIES || entries.len() >= READDIR_MAX_BYTES {
                        break;
                    }
                    put_string(&mut entries, name.as_bytes());
                    put_string(&mut entries, long_listing_line(&entry).as_bytes());
                    Attrs::of(&entry, clock).encode(&mut entries);
                    count += 1;
                }
                Some(Err(_)) => return Err(Status::failure("Directory read error")),
                None => break,
            }
        }
        dir.next += count as usize;
        if count == 0 {
            self.status(id, FX_EOF, "End of directory");
            return Ok(());
        }
        let start = self.begin(FXP_NAME);
        put_u32(&mut self.out, id);
        put_u32(&mut self.out, count);
        self.out.extend_from_slice(&entries);
        self.finish(start);
        Ok(())
    }

    fn open(&mut self, id: u32, reader: &mut Reader, volume: &SdVolume) -> Reply {
        let path = reader.path().ok_or_else(Status::bad_message)?;
        let flags = reader.u32().ok_or_else(Status::bad_message)?;
        let mut components = resolve(&[], &path);
        let leaf = components.pop().ok_or(Status::failure("Is a directory"))?;
        let parent = components;
        let writable = flags & FXF_WRITE != 0;
        let readable = flags & FXF_READ != 0 || !writable;
        let dir = open_dir_path(volume, &parent).ok_or_else(Status::no_such_file)?;
        let existing = dir
            .find(&leaf)
            .map_err(|_| Status::failure("Directory read error"))?;
        let size = match existing {
            Some(entry) if entry.is_directory() => return Err(Status::failure("Is a directory")),
            Some(_) if flags & FXF_CREAT != 0 && flags & FXF_EXCL != 0 => {
                return Err(Status::failure("File exists"));
            }
            Some(entry) if writable && flags & FXF_TRUNC != 0 => {
                // Overwrite: start from a fresh, empty cluster chain.
                volume
                    .delete(&entry)
                    .map_err(|_| Status::failure("Cannot replace file"))?;
                volume
                    .create_file(&dir, &leaf)
                    .map_err(|_| Status::failure("Cannot create file"))?;
                0
            }
            Some(entry) => entry.len(),
            None if writable && flags & FXF_CREAT != 0 => {
                volume
                    .create_file(&dir, &leaf)
                    .map_err(|_| Status::failure("Cannot create file"))?;
                0
            }
            None => return Err(Status::no_such_file()),
        };
        let slot = self.allocate(Handle::File(FileHandle {
            parent,
            leaf,
            readable,
            writable,
            size,
            append: None,
            read_at: None,
        }))?;
        self.handle_reply(id, slot);
        Ok(())
    }

    fn close(&mut self, id: u32, reader: &mut Reader) -> Reply {
        let token = reader.string().ok_or_else(Status::bad_message)?;
        let slot = self.slot_of(token)?;
        // Every WRITE chunk was committed with FileWriter::finish(); there is
        // nothing left to flush.
        self.handles[slot] = None;
        self.ok(id)
    }

    fn read(&mut self, id: u32, reader: &mut Reader, volume: &SdVolume) -> Reply {
        let token = reader.string().ok_or_else(Status::bad_message)?;
        let offset = reader.u64().ok_or_else(Status::bad_message)?;
        let wanted = reader.u32().ok_or_else(Status::bad_message)?;
        let slot = self.slot_of(token)?;
        let file = self.file_mut(slot)?;
        if !file.readable {
            return Err(Status::failure("Handle not open for reading"));
        }
        let (entry, _dir) =
            find_in(volume, &file.parent, &file.leaf).ok_or_else(Status::no_such_file)?;
        let size = entry.len();
        if offset >= size || wanted == 0 {
            self.status(id, FX_EOF, "End of file");
            return Ok(());
        }
        let length = wanted
            .min(MAX_READ_REPLY)
            .min((size - offset).min(u64::from(u32::MAX)) as u32);
        let start = self.begin(FXP_DATA);
        put_u32(&mut self.out, id);
        put_u32(&mut self.out, length);
        // Declared length = header + payload; patch it by hand because the
        // payload is appended later, chunk by chunk.
        let total = (self.out.len() - start - 4) as u32 + length;
        self.out[start..start + 4].copy_from_slice(&total.to_be_bytes());
        self.read_stream = Some(ReadStream {
            slot,
            offset,
            remaining: length,
        });
        Ok(())
    }

    fn continue_read(&mut self, volume: Option<&SdVolume>) {
        let Some(mut stream) = self.read_stream.take() else {
            return;
        };
        let chunk_len = (stream.remaining as usize).min(IO_CHUNK);
        let result = match (volume, self.handles[stream.slot].as_mut()) {
            (Some(volume), Some(Handle::File(file))) => {
                let started = esp_hal::time::Instant::now();
                let result = read_chunk(volume, file, stream.offset, chunk_len, &mut self.out);
                self.stats.card_read_us += started.elapsed().as_micros();
                result
            }
            _ => Err(()),
        };
        match result {
            Ok(read) if read > 0 => {
                stream.offset += read as u64;
                stream.remaining -= read as u32;
                self.stats.bytes_read += read as u64;
                if stream.remaining > 0 {
                    self.read_stream = Some(stream);
                }
            }
            // The header already promised `remaining` more bytes; the reply
            // cannot be completed, so the session must end.
            _ => {
                log::warn!("SFTP: read failed inside a DATA reply");
                self.fatal = true;
            }
        }
    }

    fn begin_write(&mut self, length: usize) -> bool {
        // [len][type][id u32][handle string][offset u64][data length u32]
        if self.input.len() < 13 {
            return false;
        }
        let handle_len = u32::from_be_bytes([
            self.input[9],
            self.input[10],
            self.input[11],
            self.input[12],
        ]) as usize;
        let header = 13 + handle_len + 12;
        if handle_len > 64 {
            self.fatal = true;
            return false;
        }
        if self.input.len() < header {
            return false;
        }
        let header_bytes: Vec<u8> = self.input.drain(..header).collect();
        self.stats.requests += 1;
        let mut reader = Reader::new(&header_bytes[5..]);
        let id = reader.u32().unwrap_or(0);
        let token = reader.string().unwrap_or(&[]);
        let offset = reader.u64().unwrap_or(0);
        let data_len = reader.u32().unwrap_or(0);
        if length != header - 4 + data_len as usize {
            self.fatal = true;
            return false;
        }
        let check = (|| {
            let slot = self.slot_of(token)?;
            let file = self.file_mut(slot)?;
            if !file.writable {
                return Err(Status::failure("Handle not open for writing"));
            }
            if offset != file.size {
                return Err(Status(
                    FX_OP_UNSUPPORTED,
                    "Only sequential writes at the end of the file",
                ));
            }
            Ok(slot)
        })();
        let (slot, failed) = match check {
            Ok(slot) => (slot, None),
            Err(status) => (0, Some(status)),
        };
        self.write_stream = Some(WriteStream {
            id,
            slot,
            remaining: data_len,
            failed,
        });
        true
    }

    fn continue_write(&mut self, volume: Option<&SdVolume>) -> bool {
        let Some(mut stream) = self.write_stream.take() else {
            return false;
        };
        if stream.remaining > 0 {
            let available = self.input.len().min(stream.remaining as usize);
            // Wait for a full chunk unless this is the tail of the payload:
            // fewer, larger card writes are much faster.
            if available == 0 || (available < WRITE_CHUNK && available < stream.remaining as usize)
            {
                self.write_stream = Some(stream);
                return false;
            }
            let count = available.min(WRITE_CHUNK);
            if stream.failed.is_none() && !self.card_budget {
                self.write_stream = Some(stream);
                return false;
            }
            if stream.failed.is_none() {
                self.card_budget = false;
                let result = match (volume, self.handles[stream.slot].as_mut()) {
                    (Some(volume), Some(Handle::File(file))) => {
                        let started = esp_hal::time::Instant::now();
                        let result = write_chunk(volume, file, &self.input[..count]);
                        self.stats.card_write_us += started.elapsed().as_micros();
                        result
                    }
                    (None, _) => Err(Status::failure("SD card is in use by USB DISK")),
                    _ => Err(Status::failure("Invalid handle")),
                };
                match result {
                    Ok(()) => self.stats.bytes_written += count as u64,
                    Err(status) => stream.failed = Some(status),
                }
            }
            self.input.drain(..count);
            stream.remaining -= count as u32;
        }
        if stream.remaining == 0 {
            match stream.failed.take() {
                None => self.status(stream.id, FX_OK, "OK"),
                Some(Status(code, message)) => self.status(stream.id, code, message),
            }
        } else {
            self.write_stream = Some(stream);
        }
        true
    }

    fn remove(&mut self, id: u32, reader: &mut Reader, volume: &SdVolume) -> Reply {
        let path = reader.path().ok_or_else(Status::bad_message)?;
        let components = resolve(&[], &path);
        let (leaf, parent) = components
            .split_last()
            .ok_or(Status::failure("Is a directory"))?;
        let (entry, _dir) = find_in(volume, parent, leaf).ok_or_else(Status::no_such_file)?;
        if entry.is_directory() {
            return Err(Status::failure("Is a directory"));
        }
        volume
            .delete(&entry)
            .map_err(|_| Status::failure("Cannot delete"))?;
        self.ok(id)
    }

    fn mkdir(&mut self, id: u32, reader: &mut Reader, volume: &SdVolume) -> Reply {
        let path = reader.path().ok_or_else(Status::bad_message)?;
        let components = resolve(&[], &path);
        let (leaf, parent) = components.split_last().ok_or(Status::failure("Exists"))?;
        let dir = open_dir_path(volume, parent).ok_or_else(Status::no_such_file)?;
        match volume.create_dir(&dir, leaf) {
            Ok(_) => self.ok(id),
            Err(hadris_fat::Error::AlreadyExists) => Err(Status::failure("File exists")),
            Err(_) => Err(Status::failure("Cannot create directory")),
        }
    }

    fn rmdir(&mut self, id: u32, reader: &mut Reader, volume: &SdVolume) -> Reply {
        let path = reader.path().ok_or_else(Status::bad_message)?;
        let components = resolve(&[], &path);
        let (leaf, parent) = components
            .split_last()
            .ok_or(Status::failure("Cannot remove the root"))?;
        let (entry, _dir) = find_in(volume, parent, leaf).ok_or_else(Status::no_such_file)?;
        if !entry.is_directory() {
            return Err(Status::failure("Not a directory"));
        }
        match volume.delete(&entry) {
            Ok(()) => self.ok(id),
            Err(hadris_fat::Error::DirectoryNotEmpty) => {
                Err(Status::failure("Directory not empty"))
            }
            Err(_) => Err(Status::failure("Cannot remove directory")),
        }
    }

    fn rename(&mut self, id: u32, reader: &mut Reader, volume: &SdVolume) -> Reply {
        let from = reader.path().ok_or_else(Status::bad_message)?;
        let to = reader.path().ok_or_else(Status::bad_message)?;
        let source = resolve(&[], &from);
        let target = resolve(&[], &to);
        let (source_leaf, source_parent) = source
            .split_last()
            .ok_or(Status::failure("Cannot rename the root"))?;
        let (target_leaf, target_parent) = target
            .split_last()
            .ok_or(Status::failure("Invalid target"))?;
        // Moving a directory below itself would detach a subtree.
        if target.len() > source.len()
            && target[..source.len()]
                .iter()
                .zip(source.iter())
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
        {
            return Err(Status::failure("Cannot move a directory into itself"));
        }
        let (entry, _dir) =
            find_in(volume, source_parent, source_leaf).ok_or_else(Status::no_such_file)?;
        let dest = open_dir_path(volume, target_parent).ok_or_else(Status::no_such_file)?;
        match volume.rename(&entry, &dest, target_leaf) {
            Ok(_) => self.ok(id),
            Err(hadris_fat::Error::AlreadyExists) => Err(Status::failure("Target exists")),
            Err(_) => Err(Status::failure("Rename failed")),
        }
    }

    fn setstat(&mut self, id: u32, reader: &mut Reader, volume: &SdVolume) -> Reply {
        let path = reader.path().ok_or_else(Status::bad_message)?;
        let requested_size = reader.attrs_size().ok_or_else(Status::bad_message)?;
        let components = resolve(&[], &path);
        let current = match components.split_last() {
            None => 0,
            Some((leaf, parent)) => find_in(volume, parent, leaf)
                .ok_or_else(Status::no_such_file)?
                .0
                .len(),
        };
        self.apply_setstat(id, requested_size, current)
    }

    fn fsetstat(&mut self, id: u32, reader: &mut Reader) -> Reply {
        let token = reader.string().ok_or_else(Status::bad_message)?;
        let requested_size = reader.attrs_size().ok_or_else(Status::bad_message)?;
        let slot = self.slot_of(token)?;
        let current = match self.handles[slot].as_ref() {
            Some(Handle::File(file)) => file.size,
            _ => 0,
        };
        self.apply_setstat(id, requested_size, current)
    }

    /// FAT stores no Unix permissions or owners, and clients only use times
    /// for "preserve" options: accept those so uploads do not fail. A size
    /// change (truncate/extend) is refused rather than silently ignored.
    fn apply_setstat(&mut self, id: u32, requested_size: Option<u64>, current: u64) -> Reply {
        match requested_size {
            Some(size) if size != current => Err(Status(
                FX_OP_UNSUPPORTED,
                "Changing the file size is not supported",
            )),
            _ => self.ok(id),
        }
    }
}

/// Grow `buffer` to `capacity` in a single allocation (no-op once there).
fn reserve_full(buffer: &mut Vec<u8>, capacity: usize) {
    if buffer.capacity() < capacity {
        buffer.reserve_exact(capacity - buffer.len());
    }
}

/// Read up to `count` bytes at `offset`, appending them to `out`.
fn read_chunk(
    volume: &SdVolume,
    file: &mut FileHandle,
    offset: u64,
    count: usize,
    out: &mut Vec<u8>,
) -> Result<usize, ()> {
    let dir = open_dir_path(volume, &file.parent).ok_or(())?;
    let entry = dir.find(&file.leaf).map_err(|_| ())?.ok_or(())?;
    let mut reader = match file.read_at.take() {
        Some((at, cursor)) if at == offset => {
            FileReader::new_from_cursor(volume, &entry, cursor).map_err(|_| ())?
        }
        _ => {
            let mut reader = FileReader::new(volume, &entry).map_err(|_| ())?;
            reader.seek(SeekFrom::Start(offset)).map_err(|_| ())?;
            reader
        }
    };
    let mut buffer = [0_u8; IO_CHUNK];
    let read = reader.read(&mut buffer[..count]).map_err(|_| ())?;
    file.read_at = Some((offset + read as u64, reader.cursor()));
    out.extend_from_slice(&buffer[..read]);
    Ok(read)
}

/// Append `data` at the end of the file and commit it to the card.
fn write_chunk(volume: &SdVolume, file: &mut FileHandle, data: &[u8]) -> Result<(), Status> {
    let fail = |_| Status::failure("SD write failed");
    let dir = open_dir_path(volume, &file.parent).ok_or_else(Status::no_such_file)?;
    let entry = dir
        .find(&file.leaf)
        .map_err(fail)?
        .ok_or_else(Status::no_such_file)?;
    let mut writer = match file.append.take() {
        Some(cursor) => FileWriter::new_append_from_cursor(volume, &entry, cursor).map_err(fail)?,
        None if file.size == 0 => FileWriter::new(volume, &entry).map_err(fail)?,
        // First write into an existing file (resume): walk the chain once.
        None => FileWriter::new_append(volume, &entry).map_err(fail)?,
    };
    if writer.write(data).map_err(fail)? != data.len() {
        return Err(Status::failure("SD card full"));
    }
    let cursor = writer.append_cursor();
    writer.finish().map_err(fail)?;
    file.append = Some(cursor);
    file.size += data.len() as u64;
    Ok(())
}

/// SFTP v3 attributes as this server reports them.
struct Attrs {
    size: u64,
    permissions: u32,
    mtime: Option<u32>,
}

impl Attrs {
    fn root() -> Self {
        Self {
            size: 0,
            permissions: 0o040_755,
            mtime: None,
        }
    }

    fn of(entry: &FileEntry, clock: &ClockConfig) -> Self {
        let (date, time, _) = entry.modified().to_raw();
        let local = clock::seconds_from_civil(
            i64::from(((date >> 9) & 0x7f) + 1980),
            u32::from((date >> 5) & 0x0f).max(1),
            u32::from(date & 0x1f).max(1),
            u32::from((time >> 11) & 0x1f),
            u32::from((time >> 5) & 0x3f),
            u32::from(time & 0x1f) * 2,
        );
        let mtime = clock::unix_from_local(local, clock).clamp(0, i64::from(u32::MAX)) as u32;
        Self {
            size: if entry.is_directory() { 0 } else { entry.len() },
            permissions: if entry.is_directory() {
                0o040_755
            } else {
                0o100_644
            },
            mtime: Some(mtime),
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        let flags = ATTR_SIZE | ATTR_PERMISSIONS | self.mtime.map_or(0, |_| ATTR_ACMODTIME);
        put_u32(out, flags);
        out.extend_from_slice(&self.size.to_be_bytes());
        put_u32(out, self.permissions);
        if let Some(mtime) = self.mtime {
            put_u32(out, mtime); // atime: FAT keeps no time of day for it
            put_u32(out, mtime);
        }
    }
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_string(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

/// Bounds-checked big-endian reader over one request body.
struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        if self.bytes.len() < count {
            return None;
        }
        let (head, tail) = self.bytes.split_at(count);
        self.bytes = tail;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn string(&mut self) -> Option<&'a [u8]> {
        let length = self.u32()? as usize;
        self.take(length)
    }

    /// A UTF-8 path without NUL bytes (FAT names cannot hold them).
    fn path(&mut self) -> Option<String> {
        let text = core::str::from_utf8(self.string()?).ok()?;
        (!text.contains('\0')).then(|| String::from(text))
    }

    /// Parse an ATTRS block and return the requested size, if any. The
    /// outer `None` means malformed.
    fn attrs_size(&mut self) -> Option<Option<u64>> {
        let flags = self.u32()?;
        let size = if flags & ATTR_SIZE != 0 {
            Some(self.u64()?)
        } else {
            None
        };
        if flags & ATTR_UIDGID != 0 {
            self.take(8)?;
        }
        if flags & ATTR_PERMISSIONS != 0 {
            self.take(4)?;
        }
        if flags & ATTR_ACMODTIME != 0 {
            self.take(8)?;
        }
        if flags & ATTR_EXTENDED != 0 {
            let count = self.u32()?;
            for _ in 0..count {
                self.string()?;
                self.string()?;
            }
        }
        Some(size)
    }
}
