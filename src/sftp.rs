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
//! FAT handles borrow the volume and never outlive one `step`.
//! Open SFTP handles keep paths, offsets and the validated
//! hadris-fat `ReadCursor`/`AppendCursor` instead.
//!
//! Random writes preserve the untouched tail. Growth and gaps are zero-filled
//! one chunk per poll. SETSTAT applies FAT timestamps and the read-only bit;
//! full Unix ownership, symlinks and atomic replace-rename cannot be represented
//! on FAT. OpenSSH fsync/statvfs/limits extensions are supported.

use alloc::string::String;
use alloc::vec::Vec;

use hadris_fat::sync::{
    read::{FileReader, ReadCursor},
    write::{AppendCursor, FileWriter},
    FatVolumeWriteExt, FileEntry, SeekFrom,
};

use hadris_fat::{raw::DirEntryAttrFlags, time::FatDateTime};

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
const FXP_EXTENDED: u8 = 200;
const FXP_EXTENDED_REPLY: u8 = 201;

// Status codes.
const FX_OK: u32 = 0;
const FX_EOF: u32 = 1;
const FX_NO_SUCH_FILE: u32 = 2;
const FX_PERMISSION_DENIED: u32 = 3;
const FX_FAILURE: u32 = 4;
const FX_BAD_MESSAGE: u32 = 5;
const FX_OP_UNSUPPORTED: u32 = 8;

// OPEN flags.
const FXF_READ: u32 = 0x01;
const FXF_WRITE: u32 = 0x02;
const FXF_APPEND: u32 = 0x04;
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
    append_mode: bool,
    /// Last observed committed size; revalidated against the entry on writes.
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
    offset: u64,
    /// First error; the rest of the payload is discarded.
    failed: Option<Status>,
}

/// Attributes requested by OPEN, SETSTAT or FSETSTAT.
#[derive(Clone, Default)]
struct RequestedAttrs {
    size: Option<u64>,
    permissions: Option<u32>,
    times: Option<(u32, u32)>,
    owner: bool,
}

/// Zero-filled growth is incremental, never an unbounded main-loop operation.
struct ResizeJob {
    id: u32,
    components: Vec<String>,
    attrs: RequestedAttrs,
    file: FileHandle,
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
    resize: Option<ResizeJob>,
    /// Bytes of an oversized request still to discard.
    skip: usize,
    handles: [Option<Handle>; MAX_HANDLES],
    /// Mixed into handle strings so a closed handle is not mistaken for a
    /// later one in the same slot.
    generation: u32,
    tokens: [u32; MAX_HANDLES],
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
            resize: None,
            skip: 0,
            handles: Default::default(),
            generation: 0,
            tokens: [0; MAX_HANDLES],
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
            || self.resize.is_some()
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
        if self.resize.is_some() {
            if !self.card_budget {
                return false;
            }
            self.card_budget = false;
            self.continue_resize(volume, clock);
            return true;
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
        if !self.card_budget {
            return false;
        }
        self.card_budget = false;
        let packet: Vec<u8> = self.input.drain(..4 + length).collect();
        self.stats.requests += 1;
        self.dispatch(kind, &packet[5..], volume, clock);
        true
    }

    fn dispatch(&mut self, kind: u8, body: &[u8], volume: Option<&SdVolume>, clock: &ClockConfig) {
        if kind == FXP_INIT {
            let start = self.begin(FXP_VERSION);
            put_u32(&mut self.out, 3);
            for (name, version) in [
                ("fsync@openssh.com", "1"),
                ("statvfs@openssh.com", "2"),
                ("fstatvfs@openssh.com", "2"),
                ("limits@openssh.com", "1"),
            ] {
                put_string(&mut self.out, name.as_bytes());
                put_string(&mut self.out, version.as_bytes());
            }
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
                FXP_OPEN => self.open(id, &mut reader, volume, clock),
                FXP_CLOSE => self.close(id, &mut reader),
                FXP_READ => self.read(id, &mut reader, volume),
                FXP_REMOVE => self.remove(id, &mut reader, volume),
                FXP_MKDIR => self.mkdir(id, &mut reader, volume, clock),
                FXP_RMDIR => self.rmdir(id, &mut reader, volume),
                FXP_RENAME => self.rename(id, &mut reader, volume),
                FXP_SETSTAT => self.setstat(id, &mut reader, volume, clock),
                FXP_FSETSTAT => self.fsetstat(id, &mut reader, volume, clock),
                FXP_EXTENDED => self.extended(id, &mut reader, volume),
                // READLINK, SYMLINK and anything newer.
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
        let token = self.tokens[slot];
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
        self.tokens[slot] = self.generation << 8 | slot as u32;
        self.handles[slot] = Some(handle);
        Ok(slot)
    }

    /// Validate a handle string from the client and return its slot.
    fn slot_of(&self, token: &[u8]) -> Result<usize, Status> {
        let bad = || Status::failure("Invalid handle");
        let token: [u8; 4] = token.try_into().map_err(|_| bad())?;
        let token = u32::from_be_bytes(token);
        let slot = (token & 0xff) as usize;
        if slot >= MAX_HANDLES || self.handles[slot].is_none() || self.tokens[slot] != token {
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

    fn open(
        &mut self,
        id: u32,
        reader: &mut Reader,
        volume: &SdVolume,
        clock: &ClockConfig,
    ) -> Reply {
        let path = reader.path().ok_or_else(Status::bad_message)?;
        let flags = reader.u32().ok_or_else(Status::bad_message)?;
        let attrs = reader.attrs().ok_or_else(Status::bad_message)?;
        validate_attrs(&attrs, clock)?;
        let mut components = resolve(&[], &path);
        let leaf = components.pop().ok_or(Status::failure("Is a directory"))?;
        let parent = components;
        let writable = flags & FXF_WRITE != 0;
        let readable = flags & FXF_READ != 0 || !writable;
        let dir = open_dir_path(volume, &parent).ok_or_else(Status::no_such_file)?;
        let existing = dir
            .find(&leaf)
            .map_err(|_| Status::failure("Directory read error"))?;
        let created = existing.is_none();
        let size = match existing {
            Some(entry) if entry.is_directory() => return Err(Status::failure("Is a directory")),
            Some(_) if flags & FXF_CREAT != 0 && flags & FXF_EXCL != 0 => {
                return Err(Status::failure("File exists"));
            }
            Some(entry)
                if writable && entry.attributes().contains(DirEntryAttrFlags::READ_ONLY) =>
            {
                return Err(Status(FX_PERMISSION_DENIED, "File is read-only"));
            }
            Some(entry) if writable && flags & FXF_TRUNC != 0 => {
                volume
                    .truncate(&entry, 0)
                    .map_err(|_| Status::failure("Cannot truncate file"))?;
                self.invalidate_file(&parent, &leaf);
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
        if created {
            let (entry, _) = find_in(volume, &parent, &leaf).ok_or_else(Status::no_such_file)?;
            apply_metadata(volume, &entry, &attrs, clock)?;
        }
        let slot = self.allocate(Handle::File(FileHandle {
            parent,
            leaf,
            readable,
            writable,
            append_mode: flags & FXF_APPEND != 0,
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
            if offset
                .checked_add(u64::from(data_len))
                .is_none_or(|end| end > u64::from(u32::MAX))
            {
                return Err(Status::failure("File exceeds FAT's 4 GiB limit"));
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
            offset,
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
                        let (entry, _) = match find_in(volume, &file.parent, &file.leaf) {
                            Some(found) => found,
                            None => {
                                stream.failed = Some(Status::no_such_file());
                                self.write_stream = Some(stream);
                                return true;
                            }
                        };
                        let offset = if file.append_mode {
                            entry.len()
                        } else {
                            stream.offset
                        };
                        if offset > entry.len() {
                            let zeros = [0_u8; WRITE_CHUNK];
                            let n = (offset - entry.len()).min(WRITE_CHUNK as u64) as usize;
                            let result = write_chunk(volume, file, entry.len(), &zeros[..n]);
                            self.stats.card_write_us += started.elapsed().as_micros();
                            if let Err(status) = result {
                                stream.failed = Some(status);
                            } else {
                                self.invalidate_written(stream.slot);
                            }
                            self.write_stream = Some(stream);
                            return true; // Payload stays queued until the gap is filled.
                        }
                        let result = write_chunk(volume, file, offset, &self.input[..count]);
                        self.stats.card_write_us += started.elapsed().as_micros();
                        result
                    }
                    (None, _) => Err(Status::failure("SD card is in use by USB DISK")),
                    _ => Err(Status::failure("Invalid handle")),
                };
                match result {
                    Ok(()) => {
                        self.invalidate_written(stream.slot);
                        self.stats.bytes_written += count as u64;
                        stream.offset += count as u64;
                    }
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
        if entry.attributes().contains(DirEntryAttrFlags::READ_ONLY) {
            return Err(Status(FX_PERMISSION_DENIED, "File is read-only"));
        }
        volume
            .delete(&entry)
            .map_err(|_| Status::failure("Cannot delete"))?;
        self.ok(id)
    }

    fn mkdir(
        &mut self,
        id: u32,
        reader: &mut Reader,
        volume: &SdVolume,
        clock: &ClockConfig,
    ) -> Reply {
        let path = reader.path().ok_or_else(Status::bad_message)?;
        let attrs = reader.attrs().ok_or_else(Status::bad_message)?;
        validate_attrs(&attrs, clock)?;
        if attrs.size.is_some() {
            return Err(Status::failure("Directory size cannot be set"));
        }
        let components = resolve(&[], &path);
        let (leaf, parent) = components.split_last().ok_or(Status::failure("Exists"))?;
        let dir = open_dir_path(volume, parent).ok_or_else(Status::no_such_file)?;
        match volume.create_dir(&dir, leaf) {
            Ok(_) => {
                let (entry, _) = find_in(volume, parent, leaf).ok_or_else(Status::no_such_file)?;
                apply_metadata(volume, &entry, &attrs, clock)?;
                self.ok(id)
            }
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

    fn setstat(
        &mut self,
        id: u32,
        reader: &mut Reader,
        volume: &SdVolume,
        clock: &ClockConfig,
    ) -> Reply {
        let path = reader.path().ok_or_else(Status::bad_message)?;
        let attrs = reader.attrs().ok_or_else(Status::bad_message)?;
        self.apply_setstat(id, resolve(&[], &path), attrs, volume, clock)
    }

    fn handle_components(&self, slot: usize) -> Result<Vec<String>, Status> {
        match self.handles[slot].as_ref() {
            Some(Handle::File(file)) => {
                let mut path = file.parent.clone();
                path.push(file.leaf.clone());
                Ok(path)
            }
            Some(Handle::Dir(dir)) => Ok(dir.components.clone()),
            None => Err(Status::failure("Invalid handle")),
        }
    }

    fn fsetstat(
        &mut self,
        id: u32,
        reader: &mut Reader,
        volume: &SdVolume,
        clock: &ClockConfig,
    ) -> Reply {
        let token = reader.string().ok_or_else(Status::bad_message)?;
        let attrs = reader.attrs().ok_or_else(Status::bad_message)?;
        let slot = self.slot_of(token)?;
        if attrs.size.is_some() && !self.file_mut(slot)?.writable {
            return Err(Status(FX_PERMISSION_DENIED, "Handle not open for writing"));
        }
        self.apply_setstat(id, self.handle_components(slot)?, attrs, volume, clock)
    }

    fn invalidate_file(&mut self, parent: &[String], leaf: &str) {
        for handle in &mut self.handles {
            if let Some(Handle::File(file)) = handle {
                if same_path(&file.parent, &file.leaf, parent, leaf) {
                    file.append = None;
                    file.read_at = None;
                }
            }
        }
    }

    fn invalidate_written(&mut self, slot: usize) {
        let (before, rest) = self.handles.split_at_mut(slot);
        let (current, after) = rest.split_first_mut().unwrap();
        if let Some(Handle::File(written)) = current {
            written.read_at = None;
            for handle in before.iter_mut().chain(after) {
                if let Some(Handle::File(other)) = handle {
                    if same_path(&written.parent, &written.leaf, &other.parent, &other.leaf) {
                        other.append = None;
                        other.read_at = None;
                    }
                }
            }
        }
    }

    fn apply_setstat(
        &mut self,
        id: u32,
        components: Vec<String>,
        attrs: RequestedAttrs,
        volume: &SdVolume,
        clock: &ClockConfig,
    ) -> Reply {
        validate_attrs(&attrs, clock)?;
        let (leaf, parent) = components
            .split_last()
            .ok_or(Status(FX_OP_UNSUPPORTED, "Root metadata cannot be changed"))?;
        let (entry, _) = find_in(volume, parent, leaf).ok_or_else(Status::no_such_file)?;
        if let Some(size) = attrs.size {
            if entry.is_directory() {
                return Err(Status::failure("Cannot resize a directory"));
            }
            if size != entry.len() && entry.attributes().contains(DirEntryAttrFlags::READ_ONLY) {
                return Err(Status(FX_PERMISSION_DENIED, "File is read-only"));
            }
            if size > entry.len() {
                self.resize = Some(ResizeJob {
                    id,
                    file: FileHandle {
                        parent: parent.to_vec(),
                        leaf: leaf.clone(),
                        readable: false,
                        writable: true,
                        append_mode: false,
                        size: entry.len(),
                        append: None,
                        read_at: None,
                    },
                    components,
                    attrs,
                });
                return Ok(()); // Answer only once all zero chunks are committed.
            }
            if size < entry.len() {
                volume
                    .truncate(&entry, size as usize)
                    .map_err(|_| Status::failure("Cannot truncate file"))?;
            }
        }
        let (entry, _) = find_in(volume, parent, leaf).ok_or_else(Status::no_such_file)?;
        apply_metadata(volume, &entry, &attrs, clock)?;
        self.invalidate_file(parent, leaf);
        self.ok(id)
    }

    fn continue_resize(&mut self, volume: Option<&SdVolume>, clock: &ClockConfig) {
        let mut job = self.resize.take().unwrap();
        let result = (|| {
            let volume = volume.ok_or(Status::failure("SD unavailable"))?;
            let (leaf, parent) = job.components.split_last().unwrap();
            let (entry, _) = find_in(volume, parent, leaf).ok_or_else(Status::no_such_file)?;
            let target = job.attrs.size.unwrap();
            if entry.len() < target {
                let zeros = [0_u8; WRITE_CHUNK];
                let count = (target - entry.len()).min(WRITE_CHUNK as u64) as usize;
                write_chunk(volume, &mut job.file, entry.len(), &zeros[..count])?;
                self.invalidate_file(parent, leaf);
                return Ok(false); // Metadata gets its own bounded poll.
            }
            apply_metadata(volume, &entry, &job.attrs, clock)?;
            self.invalidate_file(parent, leaf);
            Ok(true)
        })();
        match result {
            Ok(false) => self.resize = Some(job),
            Ok(true) => {
                self.status(job.id, FX_OK, "OK");
            }
            Err(Status(code, message)) => self.status(job.id, code, message),
        }
    }

    fn extended(&mut self, id: u32, reader: &mut Reader, volume: &SdVolume) -> Reply {
        let name = reader.string().ok_or_else(Status::bad_message)?;
        match name {
            b"limits@openssh.com" => {
                let start = self.begin(FXP_EXTENDED_REPLY);
                put_u32(&mut self.out, id);
                for value in [65536_u64, MAX_READ_REPLY as u64, 32768, MAX_HANDLES as u64] {
                    self.out.extend_from_slice(&value.to_be_bytes());
                }
                self.finish(start);
                Ok(())
            }
            b"fsync@openssh.com" => {
                let token = reader.string().ok_or_else(Status::bad_message)?;
                let slot = self.slot_of(token)?;
                self.file_mut(slot)?;
                // Every writer calls finish(), but also flush filesystem metadata.
                volume
                    .sync()
                    .map_err(|_| Status::failure("SD sync failed"))?;
                self.ok(id)
            }
            b"statvfs@openssh.com" | b"fstatvfs@openssh.com" => {
                let components = if name == b"statvfs@openssh.com" {
                    resolve(&[], &reader.path().ok_or_else(Status::bad_message)?)
                } else {
                    let token = reader.string().ok_or_else(Status::bad_message)?;
                    self.handle_components(self.slot_of(token)?)?
                };
                if let Some((leaf, parent)) = components.split_last() {
                    find_in(volume, parent, leaf).ok_or_else(Status::no_such_file)?;
                }
                let free = volume
                    .free_cluster_count()
                    .ok_or(Status(FX_OP_UNSUPPORTED, "FAT free-space count is unknown"))?;
                let total = volume.fat().max_cluster().saturating_sub(1) as u64;
                if u64::from(free) > total {
                    return Err(Status::failure("Invalid FAT free-space count"));
                }
                let start = self.begin(FXP_EXTENDED_REPLY);
                put_u32(&mut self.out, id);
                // f_bsize, f_frsize, blocks/free/available, inodes (not applicable),
                // fsid, flags, namemax (VFAT UTF-16 units).
                for value in [
                    volume.cluster_size() as u64,
                    volume.cluster_size() as u64,
                    total,
                    free as u64,
                    free as u64,
                    0,
                    0,
                    0,
                    volume.volume_info().volume_id() as u64,
                    0,
                    255,
                ] {
                    self.out.extend_from_slice(&value.to_be_bytes());
                }
                self.finish(start);
                Ok(())
            }
            _ => Err(Status(FX_OP_UNSUPPORTED, "Extension not supported")),
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

/// Write at an existing offset (preserving the tail), or append at EOF.
fn write_chunk(
    volume: &SdVolume,
    file: &mut FileHandle,
    offset: u64,
    data: &[u8],
) -> Result<(), Status> {
    let fail = |_| Status::failure("SD write failed");
    let (entry, _) = find_in(volume, &file.parent, &file.leaf).ok_or_else(Status::no_such_file)?;
    if entry.attributes().contains(DirEntryAttrFlags::READ_ONLY) {
        return Err(Status(FX_PERMISSION_DENIED, "File is read-only"));
    }
    let end = offset
        .checked_add(data.len() as u64)
        .filter(|end| *end <= u32::MAX as u64)
        .ok_or(Status::failure("File exceeds FAT's 4 GiB limit"))?;
    if offset > entry.len() {
        return Err(Status::failure("Gap must be zero-filled first"));
    }
    if file.size != entry.len() {
        file.append = None;
    }
    let mut writer = if offset == entry.len() {
        match file.append.take() {
            Some(cursor) => {
                FileWriter::new_append_from_cursor(volume, &entry, cursor).map_err(fail)?
            }
            None => FileWriter::new_append(volume, &entry).map_err(fail)?,
        }
    } else {
        file.append = None;
        FileWriter::new_at(volume, &entry, offset).map_err(fail)?
    };
    if writer.write(data).map_err(fail)? != data.len() {
        return Err(Status::failure("SD card full"));
    }
    let cursor = if end >= entry.len() {
        Some(writer.append_cursor())
    } else {
        None
    };
    writer.finish().map_err(fail)?;
    file.append = cursor;
    file.read_at = None;
    file.size = entry.len().max(end);
    Ok(())
}

fn same_path(a_parent: &[String], a_leaf: &str, b_parent: &[String], b_leaf: &str) -> bool {
    a_parent.len() == b_parent.len()
        && a_leaf.eq_ignore_ascii_case(b_leaf)
        && a_parent
            .iter()
            .zip(b_parent)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// Reject unrepresentable metadata before changing any file data.
fn validate_attrs(attrs: &RequestedAttrs, config: &ClockConfig) -> Reply {
    if attrs.owner {
        return Err(Status(FX_OP_UNSUPPORTED, "FAT has no UID/GID"));
    }
    if attrs.size.is_some_and(|size| size > u32::MAX as u64) {
        return Err(Status::failure("File exceeds FAT's 4 GiB limit"));
    }
    if let Some((atime, mtime)) = attrs.times {
        fat_time(atime, config)?;
        fat_time(mtime, config)?;
    }
    Ok(())
}

fn fat_time(unix: u32, config: &ClockConfig) -> Result<FatDateTime, Status> {
    let time = clock::date_time(clock::local_seconds(unix as u64, config));
    if !(1980..=2107).contains(&time.year) {
        return Err(Status(
            FX_OP_UNSUPPORTED,
            "Timestamp is outside FAT's date range",
        ));
    }
    Ok(FatDateTime::new(
        time.year as u16,
        time.month as u8,
        time.day as u8,
        time.hour as u8,
        time.minute as u8,
        time.second as u8,
    ))
}

fn apply_metadata(
    volume: &SdVolume,
    entry: &FileEntry,
    attrs: &RequestedAttrs,
    config: &ClockConfig,
) -> Reply {
    if let Some((atime, mtime)) = attrs.times {
        volume
            .set_times(
                entry,
                Some(fat_time(mtime, config)?),
                Some(fat_time(atime, config)?.date),
                None,
            )
            .map_err(|_| Status::failure("Cannot set FAT timestamps"))?;
    }
    if let Some(permissions) = attrs.permissions {
        let mut flags = entry.attributes();
        flags.set(DirEntryAttrFlags::READ_ONLY, permissions & 0o222 == 0);
        volume
            .set_attributes(entry, flags)
            .map_err(|_| Status::failure("Cannot set FAT attributes"))?;
    }
    Ok(())
}

/// SFTP v3 attributes as this server reports them.
struct Attrs {
    size: u64,
    permissions: u32,
    mtime: Option<u32>,
    atime: Option<u32>,
}

impl Attrs {
    fn root() -> Self {
        Self {
            size: 0,
            permissions: 0o040_755,
            mtime: None,
            atime: None,
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
            permissions: (if entry.is_directory() {
                0o040_755
            } else {
                0o100_644
            }) & if entry.attributes().contains(DirEntryAttrFlags::READ_ONLY) {
                !0o222
            } else {
                u32::MAX
            },
            mtime: Some(mtime),
            atime: Some({
                let date = entry.accessed_date();
                let local = clock::seconds_from_civil(
                    i64::from(((date >> 9) & 0x7f) + 1980),
                    u32::from((date >> 5) & 0xf).max(1),
                    u32::from(date & 0x1f).max(1),
                    0,
                    0,
                    0,
                );
                clock::unix_from_local(local, clock).clamp(0, u32::MAX as i64) as u32
            }),
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        let flags = ATTR_SIZE | ATTR_PERMISSIONS | self.mtime.map_or(0, |_| ATTR_ACMODTIME);
        put_u32(out, flags);
        out.extend_from_slice(&self.size.to_be_bytes());
        put_u32(out, self.permissions);
        if let Some(mtime) = self.mtime {
            put_u32(out, self.atime.unwrap_or(mtime)); // FAT access dates have day resolution
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

    /// Parse attributes without silently throwing away metadata.
    fn attrs(&mut self) -> Option<RequestedAttrs> {
        let flags = self.u32()?;
        if flags & !(ATTR_SIZE | ATTR_UIDGID | ATTR_PERMISSIONS | ATTR_ACMODTIME | ATTR_EXTENDED)
            != 0
        {
            return None;
        }
        let mut attrs = RequestedAttrs::default();
        if flags & ATTR_SIZE != 0 {
            attrs.size = Some(self.u64()?);
        }
        if flags & ATTR_UIDGID != 0 {
            self.take(8)?;
            attrs.owner = true;
        }
        if flags & ATTR_PERMISSIONS != 0 {
            attrs.permissions = Some(self.u32()?);
        }
        if flags & ATTR_ACMODTIME != 0 {
            attrs.times = Some((self.u32()?, self.u32()?));
        }
        if flags & ATTR_EXTENDED != 0 {
            let count = self.u32()?;
            for _ in 0..count {
                self.string()?;
                self.string()?;
            }
        }
        Some(attrs)
    }
}
