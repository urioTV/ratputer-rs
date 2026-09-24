//! Seekable block adapter that exposes the first FAT partition of the raw SD
//! card to `hadris-fat` through the `embedded_io` 0.7 traits.
//!
//! The adapter performs three jobs:
//!
//! 1. **Partition translation** — at mount time it reads the MBR at LBA 0,
//!    finds the first non-empty partition entry, and shifts every I/O by the
//!    partition's starting byte offset. `hadris-fat` therefore always sees
//!    byte 0 at the FAT boot sector, never the partition table.
//! 2. **Sub-sector read-modify-write** — FAT write paths place 32-byte
//!    directory entries and FAT updates at arbitrary sector offsets. The
//!    adapter reads affected blocks, patches the byte range, and writes the
//!    blocks back. Sector-aligned multi-block runs go straight through.
//! 3. **Ownership round-trip for USB MSC** — `into_inner()` returns the raw
//!    card so the USB screen can export it, and `mount()` parses a fresh MBR
//!    when the firmware regains the card.

use embedded_io::{ErrorType, Read, Seek, SeekFrom, Write};
use embedded_sdmmc::{Block, BlockDevice, BlockIdx};

const BLOCK_LEN: usize = 512;
const MBR_PARTITION_TABLE: usize = 0x1BE;
const MBR_SIGNATURE: usize = 0x1FE;
/// Blocks transferred per card command in the aligned fast path.
const MAX_IO_BLOCKS: usize = 16;

/// Error type shared by every adapter operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdBlockError {
    /// Underlying SD block transfer failed.
    Device,
    /// The MBR signature is missing, or no usable partition was found.
    NoPartition,
    /// I/O would cross the end of the partition.
    OutOfRange,
    /// A seek landed outside the partition.
    InvalidPosition,
}

impl embedded_io::Error for SdBlockError {
    fn kind(&self) -> embedded_io::ErrorKind {
        match self {
            SdBlockError::OutOfRange | SdBlockError::InvalidPosition => {
                embedded_io::ErrorKind::InvalidInput
            }
            SdBlockError::NoPartition | SdBlockError::Device => embedded_io::ErrorKind::Other,
        }
    }
}

impl core::fmt::Display for SdBlockError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SdBlockError::Device => f.write_str("SD block transfer failed"),
            SdBlockError::NoPartition => f.write_str("no usable MBR partition"),
            SdBlockError::OutOfRange => f.write_str("I/O past partition end"),
            SdBlockError::InvalidPosition => f.write_str("invalid seek position"),
        }
    }
}

impl core::error::Error for SdBlockError {}

/// Byte-seekable view of one FAT partition on a block device.
pub struct SdBlockDevice<D: BlockDevice> {
    card: D,
    /// Absolute byte offset of the partition's first sector on the card.
    base: u64,
    /// Partition size in bytes.
    size: u64,
    /// Current position relative to the partition start.
    pos: u64,
}

impl<D: BlockDevice> SdBlockDevice<D> {
    /// Mount: read the MBR, adopt the first usable partition, seek to 0.
    pub fn mount(card: D) -> Result<Self, SdBlockError> {
        let mut mbr = [Block::new(); 1];
        card.read(&mut mbr, BlockIdx(0))
            .map_err(|_| SdBlockError::Device)?;
        let bytes = &mbr[0].contents;
        if bytes[MBR_SIGNATURE] != 0x55 || bytes[MBR_SIGNATURE + 1] != 0xAA {
            return Err(SdBlockError::NoPartition);
        }
        let mut found = None;
        for index in 0..4 {
            let entry =
                &bytes[MBR_PARTITION_TABLE + index * 16..MBR_PARTITION_TABLE + index * 16 + 16];
            if entry[4] == 0 {
                continue;
            }
            let start = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]);
            let length = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]);
            if length == 0 {
                continue;
            }
            found = Some((
                u64::from(start) * BLOCK_LEN as u64,
                u64::from(length) * BLOCK_LEN as u64,
            ));
            break;
        }
        let (base, size) = found.ok_or(SdBlockError::NoPartition)?;
        let card_blocks = card.num_blocks().map_err(|_| SdBlockError::Device)?.0;
        if base + size > u64::from(card_blocks) * BLOCK_LEN as u64 {
            return Err(SdBlockError::OutOfRange);
        }
        Ok(Self {
            card,
            base,
            size,
            pos: 0,
        })
    }

    /// Hand the raw block device back (e.g., for USB Mass Storage export).
    pub fn into_inner(self) -> D {
        self.card
    }

    fn check_range(&self, byte_len: usize) -> Result<(), SdBlockError> {
        let end = self
            .pos
            .checked_add(byte_len as u64)
            .ok_or(SdBlockError::OutOfRange)?;
        if end > self.size {
            return Err(SdBlockError::OutOfRange);
        }
        Ok(())
    }

    /// Read one whole block from the card.
    fn read_card_block(&self, card_block: u32) -> Result<Block, SdBlockError> {
        let mut block = Block::new();
        self.card
            .read(core::slice::from_mut(&mut block), BlockIdx(card_block))
            .map_err(|_| SdBlockError::Device)?;
        Ok(block)
    }

    /// Write one whole block to the card.
    fn write_card_block(&self, card_block: u32, block: &Block) -> Result<(), SdBlockError> {
        self.card
            .write(core::slice::from_ref(block), BlockIdx(card_block))
            .map_err(|_| SdBlockError::Device)
    }
}

impl<D: BlockDevice> ErrorType for SdBlockDevice<D> {
    type Error = SdBlockError;
}

impl<D: BlockDevice> Read for SdBlockDevice<D> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.check_range(buf.len())?;
        let start = self.base + self.pos;
        let mut done = 0_usize;
        while done < buf.len() {
            let absolute = start + done as u64;
            let card_block = (absolute / BLOCK_LEN as u64) as u32;
            let block_offset = (absolute % BLOCK_LEN as u64) as usize;
            let remaining = buf.len() - done;

            if block_offset == 0 && remaining >= BLOCK_LEN {
                // Aligned run: single card command for up to MAX_IO_BLOCKS.
                let blocks = (remaining / BLOCK_LEN).min(MAX_IO_BLOCKS);
                let mut staging = core::array::from_fn::<_, MAX_IO_BLOCKS, _>(|_| Block::new());
                let scratch = &mut staging[..blocks];
                self.card
                    .read(scratch, BlockIdx(card_block))
                    .map_err(|_| SdBlockError::Device)?;
                for (index, block) in scratch.iter().enumerate() {
                    buf[done + index * BLOCK_LEN..done + (index + 1) * BLOCK_LEN]
                        .copy_from_slice(&block.contents);
                }
                done += blocks * BLOCK_LEN;
            } else {
                // Partial block: read the whole block, copy the slice.
                let block = self.read_card_block(card_block)?;
                let take = remaining.min(BLOCK_LEN - block_offset);
                buf[done..done + take]
                    .copy_from_slice(&block.contents[block_offset..block_offset + take]);
                done += take;
            }
        }
        self.pos += done as u64;
        Ok(done)
    }
}

impl<D: BlockDevice> Write for SdBlockDevice<D> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.check_range(buf.len())?;
        let start = self.base + self.pos;
        let mut done = 0_usize;
        while done < buf.len() {
            let absolute = start + done as u64;
            let card_block = (absolute / BLOCK_LEN as u64) as u32;
            let block_offset = (absolute % BLOCK_LEN as u64) as usize;
            let remaining = buf.len() - done;

            if block_offset == 0 && remaining >= BLOCK_LEN {
                // Aligned run: single card command for up to MAX_IO_BLOCKS.
                let blocks = (remaining / BLOCK_LEN).min(MAX_IO_BLOCKS);
                let mut staging = core::array::from_fn::<_, MAX_IO_BLOCKS, _>(|_| Block::new());
                let scratch = &mut staging[..blocks];
                for (index, block) in scratch.iter_mut().enumerate() {
                    block.contents.copy_from_slice(
                        &buf[done + index * BLOCK_LEN..done + (index + 1) * BLOCK_LEN],
                    );
                }
                self.card
                    .write(scratch, BlockIdx(card_block))
                    .map_err(|_| SdBlockError::Device)?;
                done += blocks * BLOCK_LEN;
            } else {
                // Sub-sector write: read-modify-write the one affected block.
                let mut block = self.read_card_block(card_block)?;
                let take = remaining.min(BLOCK_LEN - block_offset);
                block.contents[block_offset..block_offset + take]
                    .copy_from_slice(&buf[done..done + take]);
                self.write_card_block(card_block, &block)?;
                done += take;
            }
        }
        self.pos += done as u64;
        Ok(done)
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        // The SD block layer is write-through at block granularity; there is
        // no deferred queue to drain here.
        Ok(())
    }
}

impl<D: BlockDevice> Seek for SdBlockDevice<D> {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64, Self::Error> {
        let target = match pos {
            SeekFrom::Start(value) => value as i128,
            SeekFrom::End(value) => self.size as i128 + value as i128,
            SeekFrom::Current(value) => self.pos as i128 + value as i128,
        };
        if !(0..=self.size as i128).contains(&target) {
            return Err(SdBlockError::InvalidPosition);
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}
