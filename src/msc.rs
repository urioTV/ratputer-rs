//! Pure-Rust USB Mass Storage (Bulk-Only Transport + SCSI transparent command set).
//!
//! `embassy-usb` supplies enumeration, endpoint allocation and the ESP32-S3 DWC2
//! driver. This module implements the missing device-side MSC class and bridges
//! it to an exclusively owned `embedded_sdmmc::BlockDevice`.

use alloc::{boxed::Box, rc::Rc};
use core::{cell::RefCell, ptr};

use embassy_usb::Builder;
use embassy_usb::control::{InResponse, OutResponse, Recipient, Request, RequestType};
use embassy_usb::driver::{Direction, Driver, Endpoint, EndpointError, EndpointIn, EndpointOut};
use embassy_usb::types::InterfaceNumber;
use embassy_usb::Handler;
use embedded_sdmmc::{Block, BlockDevice, BlockIdx};

const USB_CLASS_MASS_STORAGE: u8 = 0x08;
const MSC_SUBCLASS_SCSI: u8 = 0x06;
const MSC_PROTOCOL_BULK_ONLY: u8 = 0x50;
const BULK_PACKET_SIZE: u16 = 64;

const CBW_SIGNATURE: u32 = 0x4342_5355;
const CSW_SIGNATURE: u32 = 0x5342_5355;
const CBW_LEN: usize = 31;
const CSW_LEN: usize = 13;

const REQ_GET_MAX_LUN: u8 = 0xfe;
const REQ_BULK_ONLY_RESET: u8 = 0xff;

const SCSI_TEST_UNIT_READY: u8 = 0x00;
const SCSI_REQUEST_SENSE: u8 = 0x03;
const SCSI_INQUIRY: u8 = 0x12;
const SCSI_MODE_SENSE_6: u8 = 0x1a;
const SCSI_START_STOP_UNIT: u8 = 0x1b;
const SCSI_PREVENT_ALLOW_REMOVAL: u8 = 0x1e;
const SCSI_READ_FORMAT_CAPACITIES: u8 = 0x23;
const SCSI_READ_CAPACITY_10: u8 = 0x25;
const SCSI_READ_10: u8 = 0x28;
const SCSI_WRITE_10: u8 = 0x2a;
const SCSI_VERIFY_10: u8 = 0x2f;
const SCSI_SYNCHRONIZE_CACHE_10: u8 = 0x35;
const SCSI_MODE_SENSE_10: u8 = 0x5a;
const SCSI_SERVICE_ACTION_IN_16: u8 = 0x9e;
const SCSI_REPORT_LUNS: u8 = 0xa0;

const SENSE_NOT_READY: u8 = 0x02;
const SENSE_MEDIUM_ERROR: u8 = 0x03;
const SENSE_ILLEGAL_REQUEST: u8 = 0x05;

const ASC_MEDIUM_NOT_PRESENT: u8 = 0x3a;
const ASC_UNRECOVERED_READ: u8 = 0x11;
const ASC_WRITE_ERROR: u8 = 0x0c;
const ASC_INVALID_COMMAND: u8 = 0x20;
const ASC_INVALID_FIELD: u8 = 0x24;

/// Blocks moved by one SD multi-block command and held by one cache line.
const LINE_BLOCKS: usize = 8;
/// LRU lines for small, repeated reads (FAT, directories, boot sector).
const CACHE_LINES: usize = 3;
/// Index of the extra line used for large sequential transfers, so they do not
/// evict the metadata cached in the LRU lines.
const STREAM_LINE: usize = CACHE_LINES;

type CountFn = unsafe fn(*const ()) -> u32;
type ReadFn = unsafe fn(*const (), u32, &mut [Block]) -> bool;
type WriteFn = unsafe fn(*const (), u32, &[Block]) -> bool;

#[derive(Clone, Copy)]
struct Backend {
    context: *const (),
    count: CountFn,
    read: ReadFn,
    write: WriteFn,
}

unsafe fn no_count(_: *const ()) -> u32 {
    0
}
unsafe fn no_read(_: *const (), _: u32, _: &mut [Block]) -> bool {
    false
}
unsafe fn no_write(_: *const (), _: u32, _: &[Block]) -> bool {
    false
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            context: ptr::null(),
            count: no_count,
            read: no_read,
            write: no_write,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Sense {
    key: u8,
    asc: u8,
    ascq: u8,
}

/// Transfer counters shown on the USB DISK screen, in 512-byte blocks.
#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub read_blocks: u32,
    pub write_blocks: u32,
    pub cache_hit_blocks: u32,
}

pub struct SharedState {
    backend: Backend,
    /// Card capacity, read once on attach. `num_blocks()` re-reads the CSD
    /// register over SPI, far too slow for every SCSI command.
    blocks: u32,
    /// Bumped on every attach so the class drops blocks cached in an earlier
    /// session; the firmware may have rewritten the card in between.
    generation: u32,
    attached: bool,
    configured: bool,
    ejected: bool,
    sense: Sense,
    stats: Stats,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            backend: Backend::default(),
            blocks: 0,
            generation: 0,
            attached: false,
            configured: false,
            ejected: false,
            sense: Sense::default(),
            stats: Stats::default(),
        }
    }
}

impl SharedState {
    pub fn attach<D: BlockDevice>(&mut self, device: &D) {
        self.backend = Backend {
            context: (device as *const D).cast(),
            count: block_count::<D>,
            read: read_blocks::<D>,
            write: write_blocks::<D>,
        };
        self.blocks = unsafe { (self.backend.count)(self.backend.context) };
        self.generation = self.generation.wrapping_add(1);
        self.attached = true;
        self.ejected = false;
        self.sense = Sense::default();
        self.stats = Stats::default();
    }

    pub fn detach(&mut self) {
        self.backend = Backend::default();
        self.blocks = 0;
        self.attached = false;
        self.configured = false;
        self.ejected = false;
    }

    pub fn attached(&self) -> bool {
        self.attached
    }

    pub fn configured(&self) -> bool {
        self.configured
    }

    pub fn ejected(&self) -> bool {
        self.ejected
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    fn ready(&self) -> bool {
        self.block_count() != 0
    }

    fn block_count(&self) -> u32 {
        if self.attached && !self.ejected {
            self.blocks
        } else {
            0
        }
    }

    fn set_sense(&mut self, key: u8, asc: u8, ascq: u8) {
        self.sense = Sense { key, asc, ascq };
    }
}

unsafe fn block_count<D: BlockDevice>(context: *const ()) -> u32 {
    (&*context.cast::<D>())
        .num_blocks()
        .map(|count| count.0)
        .unwrap_or(0)
}

// embedded-sdmmc issues CMD18/CMD25 for multi-block slices, so one call moves a
// whole cache line with a single command/response exchange.
unsafe fn read_blocks<D: BlockDevice>(context: *const (), lba: u32, blocks: &mut [Block]) -> bool {
    (&*context.cast::<D>()).read(blocks, BlockIdx(lba)).is_ok()
}

unsafe fn write_blocks<D: BlockDevice>(context: *const (), lba: u32, blocks: &[Block]) -> bool {
    (&*context.cast::<D>()).write(blocks, BlockIdx(lba)).is_ok()
}

struct Line {
    valid: bool,
    /// First LBA held by the line (aligned to `LINE_BLOCKS` for cached reads).
    start: u32,
    len: u32,
    last_used: u32,
    data: [Block; LINE_BLOCKS],
}

impl Line {
    fn contains(&self, lba: u32) -> bool {
        self.valid && lba >= self.start && lba - self.start < self.len
    }
}

/// Read cache in front of the SD card. Writes go through to the card before the
/// CSW is sent (no write-back), and update any cached copy of the same blocks.
struct BlockCache {
    lines: [Line; CACHE_LINES + 1],
    clock: u32,
}

impl BlockCache {
    fn invalidate(&mut self) {
        for line in &mut self.lines {
            line.valid = false;
        }
    }

    fn find(&mut self, lba: u32) -> Option<usize> {
        let index = self.lines.iter().position(|line| line.contains(lba))?;
        self.clock = self.clock.wrapping_add(1);
        self.lines[index].last_used = self.clock;
        Some(index)
    }

    fn victim(&mut self, streaming: bool) -> usize {
        if streaming {
            return STREAM_LINE;
        }
        let mut index = 0;
        for (candidate, line) in self.lines[..CACHE_LINES].iter().enumerate() {
            if !line.valid {
                return candidate;
            }
            if line.last_used < self.lines[index].last_used {
                index = candidate;
            }
        }
        index
    }

    /// Refresh cached copies of `count` blocks just written from the stream line.
    fn update_from_stream(&mut self, lba: u32, count: usize) {
        let (lines, stream) = self.lines.split_at_mut(STREAM_LINE);
        let blocks = &stream[0].data[..count];
        for line in lines {
            for (offset, block) in blocks.iter().enumerate() {
                let target = lba + offset as u32;
                if line.contains(target) {
                    line.data[(target - line.start) as usize]
                        .contents
                        .copy_from_slice(&block.contents);
                }
            }
        }
    }
}

/// 16 KiB of internal SRAM, kept out of the 150 KiB heap shared with Wi-Fi and
/// Slint. `MscClass` is created once, so the cache has exactly one owner.
static mut CACHE: core::mem::MaybeUninit<BlockCache> = core::mem::MaybeUninit::zeroed();

async fn write_packets<E: EndpointIn>(endpoint: &mut E, data: &[u8]) -> Result<(), EndpointError> {
    for chunk in data.chunks(BULK_PACKET_SIZE as usize) {
        endpoint.write(chunk).await?;
    }
    Ok(())
}

async fn read_packets<E: EndpointOut>(endpoint: &mut E, data: &mut [u8]) -> Result<(), EndpointError> {
    for chunk in data.chunks_mut(BULK_PACKET_SIZE as usize) {
        let count = endpoint.read(chunk).await?;
        if count != chunk.len() {
            return Err(EndpointError::BufferOverflow);
        }
    }
    Ok(())
}

struct ControlHandler {
    interface: InterfaceNumber,
    shared: Rc<RefCell<SharedState>>,
}

impl Handler for ControlHandler {
    fn reset(&mut self) {
        let mut shared = self.shared.borrow_mut();
        shared.configured = false;
    }

    fn enabled(&mut self, enabled: bool) {
        if !enabled {
            self.shared.borrow_mut().configured = false;
        }
    }

    fn configured(&mut self, configured: bool) {
        self.shared.borrow_mut().configured = configured;
    }

    fn control_in<'a>(&'a mut self, request: Request, buffer: &'a mut [u8]) -> Option<InResponse<'a>> {
        if request.request_type != RequestType::Class
            || request.recipient != Recipient::Interface
            || request.index != self.interface.0 as u16
        {
            return None;
        }

        match request.request {
            REQ_GET_MAX_LUN if request.value == 0 && request.length == 1 => {
                buffer[0] = 0;
                Some(InResponse::Accepted(&buffer[..1]))
            }
            _ => Some(InResponse::Rejected),
        }
    }

    fn control_out(&mut self, request: Request, data: &[u8]) -> Option<OutResponse> {
        if request.request_type != RequestType::Class
            || request.recipient != Recipient::Interface
            || request.index != self.interface.0 as u16
        {
            return None;
        }

        match request.request {
            REQ_BULK_ONLY_RESET if request.value == 0 && request.length == 0 && data.is_empty() => {
                let mut shared = self.shared.borrow_mut();
                shared.sense = Sense::default();
                Some(OutResponse::Accepted)
            }
            _ => Some(OutResponse::Rejected),
        }
    }
}

#[derive(Clone, Copy)]
struct Cbw {
    tag: u32,
    transfer_len: u32,
    direction: Direction,
    lun: u8,
    command_len: u8,
    command: [u8; 16],
}

impl Cbw {
    fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != CBW_LEN || u32::from_le_bytes(bytes[0..4].try_into().ok()?) != CBW_SIGNATURE {
            return None;
        }
        let command_len = bytes[14] & 0x1f;
        if command_len == 0 || command_len > 16 || bytes[13] > 15 || bytes[12] & 0x7f != 0 {
            return None;
        }
        let mut command = [0_u8; 16];
        command.copy_from_slice(&bytes[15..31]);
        Some(Self {
            tag: u32::from_le_bytes(bytes[4..8].try_into().ok()?),
            transfer_len: u32::from_le_bytes(bytes[8..12].try_into().ok()?),
            direction: if bytes[12] & 0x80 == 0 {
                Direction::Out
            } else {
                Direction::In
            },
            lun: bytes[13],
            command_len,
            command,
        })
    }
}

#[derive(Clone, Copy)]
enum CommandStatus {
    Passed = 0,
    Failed = 1,
    PhaseError = 2,
}

pub struct MscClass<'d, D: Driver<'d>> {
    endpoint_out: D::EndpointOut,
    endpoint_in: D::EndpointIn,
    shared: Rc<RefCell<SharedState>>,
    cache: &'static mut BlockCache,
    cache_generation: u32,
}

impl<'d, D: Driver<'d>> MscClass<'d, D> {
    pub fn new(builder: &mut Builder<'d, D>, shared: Rc<RefCell<SharedState>>) -> Self {
        let mut function = builder.function(
            USB_CLASS_MASS_STORAGE,
            MSC_SUBCLASS_SCSI,
            MSC_PROTOCOL_BULK_ONLY,
        );
        let mut interface = function.interface();
        let interface_number = interface.interface_number();
        let mut alternate = interface.alt_setting(
            USB_CLASS_MASS_STORAGE,
            MSC_SUBCLASS_SCSI,
            MSC_PROTOCOL_BULK_ONLY,
            None,
        );
        let endpoint_out = alternate.endpoint_bulk_out(None, BULK_PACKET_SIZE);
        let endpoint_in = alternate.endpoint_bulk_in(None, BULK_PACKET_SIZE);
        drop(alternate);
        drop(interface);
        drop(function);

        builder.handler(Box::leak(Box::new(ControlHandler {
            interface: interface_number,
            shared: shared.clone(),
        })));

        // SAFETY: `UsbDisk` builds the class once, so this is the only reference;
        // an all-zero `BlockCache` is valid (invalid lines, zeroed blocks).
        let cache = unsafe { (*ptr::addr_of_mut!(CACHE)).assume_init_mut() };
        cache.invalidate();

        Self {
            endpoint_out,
            endpoint_in,
            shared,
            cache,
            cache_generation: 0,
        }
    }

    pub async fn run(mut self) -> ! {
        let mut cbw_buffer = [0_u8; BULK_PACKET_SIZE as usize];
        loop {
            let received = match self.endpoint_out.read(&mut cbw_buffer).await {
                Ok(received) => received,
                Err(EndpointError::Disabled) => {
                    self.endpoint_out.wait_enabled().await;
                    continue;
                }
                Err(EndpointError::BufferOverflow) => continue,
            };

            let Some(cbw) = Cbw::parse(&cbw_buffer[..received]) else {
                log::warn!("USB MSC received an invalid CBW");
                continue;
            };
            if cbw.lun != 0 {
                self.send_csw(cbw.tag, cbw.transfer_len, CommandStatus::Failed)
                    .await;
                continue;
            }

            let (residue, status) = self.execute(cbw).await;
            self.send_csw(cbw.tag, residue, status).await;
        }
    }

    async fn send_csw(&mut self, tag: u32, residue: u32, status: CommandStatus) {
        let mut bytes = [0_u8; CSW_LEN];
        bytes[0..4].copy_from_slice(&CSW_SIGNATURE.to_le_bytes());
        bytes[4..8].copy_from_slice(&tag.to_le_bytes());
        bytes[8..12].copy_from_slice(&residue.to_le_bytes());
        bytes[12] = status as u8;
        if self.endpoint_in.write(&bytes).await.is_err() {
            self.endpoint_in.wait_enabled().await;
        }
    }

    async fn execute(&mut self, cbw: Cbw) -> (u32, CommandStatus) {
        let _command_len = cbw.command_len;
        let generation = self.shared.borrow().generation;
        if generation != self.cache_generation {
            self.cache.invalidate();
            self.cache_generation = generation;
        }
        match cbw.command[0] {
            SCSI_TEST_UNIT_READY => self.test_unit_ready(cbw),
            SCSI_REQUEST_SENSE => self.request_sense(cbw).await,
            SCSI_INQUIRY => self.inquiry(cbw).await,
            SCSI_MODE_SENSE_6 => self.mode_sense_6(cbw).await,
            SCSI_MODE_SENSE_10 => self.mode_sense_10(cbw).await,
            SCSI_START_STOP_UNIT => self.start_stop(cbw),
            SCSI_PREVENT_ALLOW_REMOVAL | SCSI_SYNCHRONIZE_CACHE_10 => self.no_data(cbw),
            SCSI_READ_FORMAT_CAPACITIES => self.read_format_capacities(cbw).await,
            SCSI_READ_CAPACITY_10 => self.read_capacity_10(cbw).await,
            SCSI_SERVICE_ACTION_IN_16 if cbw.command[1] & 0x1f == 0x10 => {
                self.read_capacity_16(cbw).await
            }
            SCSI_REPORT_LUNS => self.report_luns(cbw).await,
            SCSI_READ_10 => self.read_10(cbw).await,
            SCSI_WRITE_10 => self.write_10(cbw).await,
            SCSI_VERIFY_10 if cbw.command[1] & 0x02 == 0 => self.no_data(cbw),
            _ => {
                self.shared
                    .borrow_mut()
                    .set_sense(SENSE_ILLEGAL_REQUEST, ASC_INVALID_COMMAND, 0);
                (cbw.transfer_len, CommandStatus::Failed)
            }
        }
    }

    fn no_data(&mut self, cbw: Cbw) -> (u32, CommandStatus) {
        if cbw.transfer_len == 0 {
            (0, CommandStatus::Passed)
        } else {
            self.shared
                .borrow_mut()
                .set_sense(SENSE_ILLEGAL_REQUEST, ASC_INVALID_FIELD, 0);
            (cbw.transfer_len, CommandStatus::PhaseError)
        }
    }

    fn test_unit_ready(&mut self, cbw: Cbw) -> (u32, CommandStatus) {
        if cbw.transfer_len != 0 {
            return (cbw.transfer_len, CommandStatus::PhaseError);
        }
        if self.shared.borrow().ready() {
            (0, CommandStatus::Passed)
        } else {
            self.shared
                .borrow_mut()
                .set_sense(SENSE_NOT_READY, ASC_MEDIUM_NOT_PRESENT, 0);
            (0, CommandStatus::Failed)
        }
    }

    fn start_stop(&mut self, cbw: Cbw) -> (u32, CommandStatus) {
        if cbw.transfer_len != 0 {
            return (cbw.transfer_len, CommandStatus::PhaseError);
        }
        let load_eject = cbw.command[4] & 0x02 != 0;
        let start = cbw.command[4] & 0x01 != 0;
        if load_eject {
            self.shared.borrow_mut().ejected = !start;
        }
        (0, CommandStatus::Passed)
    }

    async fn request_sense(&mut self, cbw: Cbw) -> (u32, CommandStatus) {
        let sense = self.shared.borrow().sense;
        let mut response = [0_u8; 18];
        response[0] = 0x70;
        response[2] = sense.key;
        response[7] = 10;
        response[12] = sense.asc;
        response[13] = sense.ascq;
        let result = self.send_data(cbw, &response).await;
        if matches!(result.1, CommandStatus::Passed) {
            self.shared.borrow_mut().sense = Sense::default();
        }
        result
    }

    async fn inquiry(&mut self, cbw: Cbw) -> (u32, CommandStatus) {
        let evpd = cbw.command[1] & 0x01 != 0;
        let page = cbw.command[2];
        if !evpd {
            if page != 0 {
                self.shared
                    .borrow_mut()
                    .set_sense(SENSE_ILLEGAL_REQUEST, ASC_INVALID_FIELD, 0);
                return (cbw.transfer_len, CommandStatus::Failed);
            }
            let mut response = [0_u8; 36];
            response[0] = 0x00;
            response[1] = 0x80;
            response[2] = 0x05;
            response[3] = 0x02;
            response[4] = 31;
            response[8..16].copy_from_slice(b"RATPUTER");
            response[16..32].copy_from_slice(b"SD CARD         ");
            response[32..36].copy_from_slice(b"1.0 ");
            return self.send_data(cbw, &response).await;
        }

        match page {
            0x00 => self.send_data(cbw, &[0x00, 0x00, 0x00, 0x03, 0x00, 0x80, 0x83]).await,
            0x80 => {
                let mut response = [0_u8; 16];
                response[..4].copy_from_slice(&[0x00, 0x80, 0x00, 12]);
                response[4..].copy_from_slice(b"RATPUTER-ADV");
                self.send_data(cbw, &response).await
            }
            0x83 => self.send_data(cbw, &[0x00, 0x83, 0x00, 0x00]).await,
            _ => {
                self.shared
                    .borrow_mut()
                    .set_sense(SENSE_ILLEGAL_REQUEST, ASC_INVALID_FIELD, 0);
                (cbw.transfer_len, CommandStatus::Failed)
            }
        }
    }

    async fn mode_sense_6(&mut self, cbw: Cbw) -> (u32, CommandStatus) {
        // Four-byte header, no block descriptor and no mode pages.
        self.send_data(cbw, &[3, 0, 0, 0]).await
    }

    async fn mode_sense_10(&mut self, cbw: Cbw) -> (u32, CommandStatus) {
        self.send_data(cbw, &[0, 6, 0, 0, 0, 0, 0, 0]).await
    }

    async fn read_capacity_10(&mut self, cbw: Cbw) -> (u32, CommandStatus) {
        let count = self.shared.borrow().block_count();
        if count == 0 {
            self.shared
                .borrow_mut()
                .set_sense(SENSE_NOT_READY, ASC_MEDIUM_NOT_PRESENT, 0);
            return (cbw.transfer_len, CommandStatus::Failed);
        }
        let mut response = [0_u8; 8];
        response[..4].copy_from_slice(&(count - 1).to_be_bytes());
        response[4..].copy_from_slice(&(Block::LEN as u32).to_be_bytes());
        self.send_data(cbw, &response).await
    }

    async fn read_capacity_16(&mut self, cbw: Cbw) -> (u32, CommandStatus) {
        let count = self.shared.borrow().block_count();
        if count == 0 {
            return (cbw.transfer_len, CommandStatus::Failed);
        }
        let mut response = [0_u8; 32];
        response[..8].copy_from_slice(&u64::from(count - 1).to_be_bytes());
        response[8..12].copy_from_slice(&(Block::LEN as u32).to_be_bytes());
        self.send_data(cbw, &response).await
    }

    async fn read_format_capacities(&mut self, cbw: Cbw) -> (u32, CommandStatus) {
        let count = self.shared.borrow().block_count();
        let mut response = [0_u8; 12];
        response[3] = 8;
        response[4..8].copy_from_slice(&count.to_be_bytes());
        response[8] = 0x02;
        response[9] = ((Block::LEN >> 16) & 0xff) as u8;
        response[10] = ((Block::LEN >> 8) & 0xff) as u8;
        response[11] = (Block::LEN & 0xff) as u8;
        self.send_data(cbw, &response).await
    }

    async fn report_luns(&mut self, cbw: Cbw) -> (u32, CommandStatus) {
        let mut response = [0_u8; 16];
        response[..4].copy_from_slice(&8_u32.to_be_bytes());
        self.send_data(cbw, &response).await
    }

    async fn read_10(&mut self, cbw: Cbw) -> (u32, CommandStatus) {
        let lba = u32::from_be_bytes(cbw.command[2..6].try_into().unwrap());
        let blocks = u16::from_be_bytes(cbw.command[7..9].try_into().unwrap()) as u32;
        let expected = blocks * Block::LEN as u32;
        if cbw.direction != Direction::In || cbw.transfer_len != expected {
            return (cbw.transfer_len, CommandStatus::PhaseError);
        }
        if blocks == 0 {
            return (0, CommandStatus::Passed);
        }

        let count = self.shared.borrow().block_count();
        if lba.checked_add(blocks).is_none_or(|end| end > count) {
            self.shared
                .borrow_mut()
                .set_sense(SENSE_ILLEGAL_REQUEST, ASC_INVALID_FIELD, 0);
            return (cbw.transfer_len, CommandStatus::Failed);
        }

        // Small reads (FAT, directories) go through the LRU lines; large ones use
        // the stream line so file data does not evict filesystem metadata.
        let streaming = blocks as usize > LINE_BLOCKS;
        let end = lba + blocks;
        let mut current = lba;
        let mut residue = cbw.transfer_len;
        while current < end {
            let (index, hit) = match self.cache.find(current) {
                Some(index) => (index, true),
                None => {
                    let index = self.cache.victim(streaming);
                    let start = current & !(LINE_BLOCKS as u32 - 1);
                    let len = (LINE_BLOCKS as u32).min(count - start);
                    let backend = self.shared.borrow().backend;
                    let line = &mut self.cache.lines[index];
                    line.valid = false;
                    if !unsafe { (backend.read)(backend.context, start, &mut line.data[..len as usize]) } {
                        self.shared
                            .borrow_mut()
                            .set_sense(SENSE_MEDIUM_ERROR, ASC_UNRECOVERED_READ, 0);
                        return (residue, CommandStatus::Failed);
                    }
                    line.valid = true;
                    line.start = start;
                    line.len = len;
                    self.cache.clock = self.cache.clock.wrapping_add(1);
                    self.cache.lines[index].last_used = self.cache.clock;
                    (index, false)
                }
            };

            let line = &self.cache.lines[index];
            let first = (current - line.start) as usize;
            let last = ((end - line.start) as usize).min(line.len as usize);
            let next = line.start + last as u32;
            for block in &line.data[first..last] {
                if write_packets(&mut self.endpoint_in, &block.contents).await.is_err() {
                    return (residue, CommandStatus::PhaseError);
                }
                residue -= Block::LEN as u32;
            }

            let moved = (last - first) as u32;
            let mut shared = self.shared.borrow_mut();
            shared.stats.read_blocks = shared.stats.read_blocks.wrapping_add(moved);
            if hit {
                shared.stats.cache_hit_blocks = shared.stats.cache_hit_blocks.wrapping_add(moved);
            }
            drop(shared);
            current = next;
        }
        (residue, CommandStatus::Passed)
    }

    async fn write_10(&mut self, cbw: Cbw) -> (u32, CommandStatus) {
        let lba = u32::from_be_bytes(cbw.command[2..6].try_into().unwrap());
        let blocks = u16::from_be_bytes(cbw.command[7..9].try_into().unwrap()) as u32;
        let expected = blocks * Block::LEN as u32;
        if cbw.direction != Direction::Out || cbw.transfer_len != expected {
            return (cbw.transfer_len, CommandStatus::PhaseError);
        }
        if blocks == 0 {
            return (0, CommandStatus::Passed);
        }

        let count = self.shared.borrow().block_count();
        if lba.checked_add(blocks).is_none_or(|end| end > count) {
            self.shared
                .borrow_mut()
                .set_sense(SENSE_ILLEGAL_REQUEST, ASC_INVALID_FIELD, 0);
            if self.discard_out(cbw.transfer_len).await.is_err() {
                return (cbw.transfer_len, CommandStatus::PhaseError);
            }
            return (cbw.transfer_len, CommandStatus::Failed);
        }

        // Collect up to one line from USB, then write it with a single CMD25.
        // The write completes on the card before the CSW reports success.
        let end = lba + blocks;
        let mut current = lba;
        let mut residue = cbw.transfer_len;
        while current < end {
            let count = ((end - current) as usize).min(LINE_BLOCKS);
            let line = &mut self.cache.lines[STREAM_LINE];
            line.valid = false;
            for block in &mut line.data[..count] {
                if read_packets(&mut self.endpoint_out, &mut block.contents).await.is_err() {
                    return (residue, CommandStatus::PhaseError);
                }
            }

            let backend = self.shared.borrow().backend;
            let written = unsafe {
                (backend.write)(backend.context, current, &self.cache.lines[STREAM_LINE].data[..count])
            };
            if !written {
                self.shared
                    .borrow_mut()
                    .set_sense(SENSE_MEDIUM_ERROR, ASC_WRITE_ERROR, 0);
                let remaining = residue - (count * Block::LEN) as u32;
                if self.discard_out(remaining).await.is_err() {
                    return (residue, CommandStatus::PhaseError);
                }
                return (residue, CommandStatus::Failed);
            }

            self.cache.update_from_stream(current, count);
            residue -= (count * Block::LEN) as u32;
            let mut shared = self.shared.borrow_mut();
            shared.stats.write_blocks = shared.stats.write_blocks.wrapping_add(count as u32);
            drop(shared);
            current += count as u32;
        }
        (residue, CommandStatus::Passed)
    }

    async fn send_data(&mut self, cbw: Cbw, data: &[u8]) -> (u32, CommandStatus) {
        if cbw.direction != Direction::In && cbw.transfer_len != 0 {
            return (cbw.transfer_len, CommandStatus::PhaseError);
        }
        let count = data.len().min(cbw.transfer_len as usize);
        if write_packets(&mut self.endpoint_in, &data[..count]).await.is_err() {
            return (cbw.transfer_len, CommandStatus::PhaseError);
        }
        if count < cbw.transfer_len as usize && count % BULK_PACKET_SIZE as usize == 0 {
            if self.endpoint_in.write(&[]).await.is_err() {
                return (cbw.transfer_len - count as u32, CommandStatus::PhaseError);
            }
        }
        (cbw.transfer_len - count as u32, CommandStatus::Passed)
    }

    async fn discard_out(&mut self, mut length: u32) -> Result<(), EndpointError> {
        let mut packet = [0_u8; BULK_PACKET_SIZE as usize];
        while length != 0 {
            let wanted = (length as usize).min(packet.len());
            let count = self.endpoint_out.read(&mut packet[..wanted]).await?;
            if count != wanted {
                return Err(EndpointError::BufferOverflow);
            }
            length -= count as u32;
        }
        Ok(())
    }
}
