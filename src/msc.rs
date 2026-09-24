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

type CountFn = unsafe fn(*const ()) -> u32;
type ReadFn = unsafe fn(*const (), u32, &mut [u8; Block::LEN]) -> bool;
type WriteFn = unsafe fn(*const (), u32, &[u8; Block::LEN]) -> bool;

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
unsafe fn no_read(_: *const (), _: u32, _: &mut [u8; Block::LEN]) -> bool {
    false
}
unsafe fn no_write(_: *const (), _: u32, _: &[u8; Block::LEN]) -> bool {
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

pub struct SharedState {
    backend: Backend,
    attached: bool,
    configured: bool,
    ejected: bool,
    sense: Sense,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            backend: Backend::default(),
            attached: false,
            configured: false,
            ejected: false,
            sense: Sense::default(),
        }
    }
}

impl SharedState {
    pub fn attach<D: BlockDevice>(&mut self, device: &D) {
        self.backend = Backend {
            context: (device as *const D).cast(),
            count: block_count::<D>,
            read: read_block::<D>,
            write: write_block::<D>,
        };
        self.attached = true;
        self.ejected = false;
        self.sense = Sense::default();
    }

    pub fn detach(&mut self) {
        self.backend = Backend::default();
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

    fn ready(&self) -> bool {
        self.attached && !self.ejected && unsafe { (self.backend.count)(self.backend.context) != 0 }
    }

    fn block_count(&self) -> u32 {
        if self.attached && !self.ejected {
            unsafe { (self.backend.count)(self.backend.context) }
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

unsafe fn read_block<D: BlockDevice>(
    context: *const (),
    lba: u32,
    output: &mut [u8; Block::LEN],
) -> bool {
    let mut block = [Block::new()];
    if (&*context.cast::<D>())
        .read(&mut block, BlockIdx(lba))
        .is_err()
    {
        return false;
    }
    output.copy_from_slice(&block[0].contents);
    true
}

unsafe fn write_block<D: BlockDevice>(
    context: *const (),
    lba: u32,
    input: &[u8; Block::LEN],
) -> bool {
    let mut block = Block::new();
    block.contents.copy_from_slice(input);
    (&*context.cast::<D>())
        .write(core::slice::from_ref(&block), BlockIdx(lba))
        .is_ok()
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

        Self {
            endpoint_out,
            endpoint_in,
            shared,
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

        let mut residue = cbw.transfer_len;
        let mut block = [0_u8; Block::LEN];
        for index in 0..blocks {
            let backend = self.shared.borrow().backend;
            if !unsafe { (backend.read)(backend.context, lba + index, &mut block) } {
                self.shared
                    .borrow_mut()
                    .set_sense(SENSE_MEDIUM_ERROR, ASC_UNRECOVERED_READ, 0);
                return (residue, CommandStatus::Failed);
            }
            if self.write_all(&block).await.is_err() {
                return (residue, CommandStatus::PhaseError);
            }
            residue -= Block::LEN as u32;
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

        let mut residue = cbw.transfer_len;
        let mut block = [0_u8; Block::LEN];
        for index in 0..blocks {
            if self.read_exact(&mut block).await.is_err() {
                return (residue, CommandStatus::PhaseError);
            }
            let backend = self.shared.borrow().backend;
            if !unsafe { (backend.write)(backend.context, lba + index, &block) } {
                self.shared
                    .borrow_mut()
                    .set_sense(SENSE_MEDIUM_ERROR, ASC_WRITE_ERROR, 0);
                let remaining = residue.saturating_sub(Block::LEN as u32);
                if self.discard_out(remaining).await.is_err() {
                    return (residue, CommandStatus::PhaseError);
                }
                return (residue, CommandStatus::Failed);
            }
            residue -= Block::LEN as u32;
        }
        (residue, CommandStatus::Passed)
    }

    async fn send_data(&mut self, cbw: Cbw, data: &[u8]) -> (u32, CommandStatus) {
        if cbw.direction != Direction::In && cbw.transfer_len != 0 {
            return (cbw.transfer_len, CommandStatus::PhaseError);
        }
        let count = data.len().min(cbw.transfer_len as usize);
        if self.write_all(&data[..count]).await.is_err() {
            return (cbw.transfer_len, CommandStatus::PhaseError);
        }
        if count < cbw.transfer_len as usize && count % BULK_PACKET_SIZE as usize == 0 {
            if self.endpoint_in.write(&[]).await.is_err() {
                return (cbw.transfer_len - count as u32, CommandStatus::PhaseError);
            }
        }
        (cbw.transfer_len - count as u32, CommandStatus::Passed)
    }

    async fn write_all(&mut self, data: &[u8]) -> Result<(), EndpointError> {
        for chunk in data.chunks(BULK_PACKET_SIZE as usize) {
            self.endpoint_in.write(chunk).await?;
        }
        Ok(())
    }

    async fn read_exact(&mut self, data: &mut [u8]) -> Result<(), EndpointError> {
        for chunk in data.chunks_mut(BULK_PACKET_SIZE as usize) {
            let count = self.endpoint_out.read(chunk).await?;
            if count != chunk.len() {
                return Err(EndpointError::BufferOverflow);
            }
        }
        Ok(())
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
