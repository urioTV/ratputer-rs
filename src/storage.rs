//! SD-card Wi-Fi/FTP configuration (`RATPUTER/WIFI.CFG`, TOML) and the FAT
//! volume mounted with `hadris-fat`.
//!
//! Ownership: one mounted `FatVolume` owns the raw SD card. Opening the USB
//! DISK screen moves the card out via `into_inner()`; leaving it mounts a
//! fresh volume (the USB host may have rewritten the MBR/FAT). Writes are
//! `hadris-fat` write-through by default — the optional FAT-sector cache is
//! write-back and deliberately not enabled.

use alloc::{string::String, vec::Vec};
use core::sync::atomic::{AtomicU32, Ordering};

use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::delay::Delay;
use esp_hal::gpio::Output;
use esp_hal::spi::master::Spi;
use esp_hal::Blocking;
use hadris_fat::time::{FatDateTime, TimeProvider};

use embedded_sdmmc::SdCard;
use serde::{Deserialize, Serialize};

use crate::sdblock::SdBlockDevice;

const CONFIG_DIR: &str = "RATPUTER";
const CONFIG_FILE: &str = "WIFI.CFG";
const CONFIG_VERSION: u8 = 1;
const MAX_CONFIG_BYTES: usize = 4096;
const MAX_SAVED_NETWORKS: usize = 12;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    Open,
    Wep,
    Wpa,
    #[default]
    Wpa2,
    WpaWpa2,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SavedNetwork {
    pub ssid: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub auth: AuthKind,
}

/// Daylight-saving rule applied on top of the fixed UTC offset.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DstRule {
    None,
    /// EU rule: +1 h from the last Sunday of March to the last Sunday of October (01:00 UTC).
    #[default]
    Eu,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClockConfig {
    #[serde(default = "default_utc_offset_minutes")]
    pub utc_offset_minutes: i16,
    #[serde(default)]
    pub dst: DstRule,
    #[serde(default = "default_ntp_server")]
    pub ntp_server: String,
}

// Defaults match Central European Time (CET/CEST).
fn default_utc_offset_minutes() -> i16 {
    60
}

fn default_ntp_server() -> String {
    String::from("pool.ntp.org")
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            utc_offset_minutes: default_utc_offset_minutes(),
            dst: DstRule::default(),
            ntp_server: default_ntp_server(),
        }
    }
}

/// FTP server login. Shown on the FTP screen; the password can be changed there.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FtpConfig {
    #[serde(default = "default_ftp_user")]
    pub user: String,
    #[serde(default = "default_ftp_password")]
    pub password: String,
}

fn default_ftp_user() -> String {
    String::from("rat")
}

fn default_ftp_password() -> String {
    String::from("cheese")
}

impl Default for FtpConfig {
    fn default() -> Self {
        Self {
            user: default_ftp_user(),
            password: default_ftp_password(),
        }
    }
}

/// FTP credentials: 1-32 printable ASCII characters without spaces, so they can
/// be typed on the Cardputer and shown in the pixel font.
pub fn valid_ftp_credential(value: &str) -> bool {
    (1..=32).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_graphic())
}

#[derive(Debug, Deserialize, Serialize)]
pub struct WifiConfig {
    pub version: u8,
    // Kept before `networks`: TOML needs plain tables ahead of arrays of tables.
    #[serde(default)]
    pub clock: ClockConfig,
    #[serde(default)]
    pub ftp: FtpConfig,
    #[serde(default)]
    pub networks: Vec<SavedNetwork>,
}

impl Default for WifiConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            clock: ClockConfig::default(),
            ftp: FtpConfig::default(),
            networks: Vec::new(),
        }
    }
}

impl WifiConfig {
    pub fn upsert(&mut self, network: SavedNetwork) {
        if let Some(existing) = self
            .networks
            .iter_mut()
            .find(|entry| entry.ssid == network.ssid)
        {
            *existing = network;
            return;
        }

        if self.networks.len() == MAX_SAVED_NETWORKS {
            self.networks.remove(0);
        }
        self.networks.push(network);
    }

    pub fn remove(&mut self, index: usize) -> bool {
        if index >= self.networks.len() {
            return false;
        }
        self.networks.remove(index);
        true
    }

    fn validate(mut self) -> Result<Self, StorageError> {
        if self.version != CONFIG_VERSION {
            return Err(StorageError::UnsupportedVersion);
        }
        self.networks.retain(|entry| {
            !entry.ssid.is_empty() && entry.ssid.len() <= 32 && entry.password.len() <= 64
        });
        self.networks.truncate(MAX_SAVED_NETWORKS);
        // Real-world offsets span UTC-12:00..UTC+14:00.
        if !(-720..=840).contains(&self.clock.utc_offset_minutes) {
            self.clock.utc_offset_minutes = default_utc_offset_minutes();
        }
        if self.clock.ntp_server.is_empty() || self.clock.ntp_server.len() > 64 {
            self.clock.ntp_server = default_ntp_server();
        }
        if !valid_ftp_credential(&self.ftp.user) {
            self.ftp.user = default_ftp_user();
        }
        if !valid_ftp_credential(&self.ftp.password) {
            self.ftp.password = default_ftp_password();
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageError {
    Sd,
    Missing,
    TooLarge,
    InvalidToml,
    UnsupportedVersion,
}

/// The raw SD card on the Cardputer ADV's dedicated SPI3 bus.
pub type SdCardDevice =
    SdCard<ExclusiveDevice<Spi<'static, Blocking>, Output<'static>, Delay>, Delay>;
/// Seekable first-partition view of that card.
pub type SdBlock = SdBlockDevice<SdCardDevice>;
/// Mounted FAT volume; owns the card while mounted.
pub type SdVolume = hadris_fat::sync::FatVolume<SdBlock>;

/// Mount the card's first partition with the firmware clock provider.
pub fn mount(card: SdCardDevice) -> Option<SdVolume> {
    let block = SdBlockDevice::mount(card).ok()?;
    hadris_fat::sync::FatVolume::builder(block)
        .time_provider(&FatClock)
        .open()
        .ok()
}

/// Return the raw card so USB Mass Storage can export it sector-by-sector.
pub fn free(volume: SdVolume) -> SdCardDevice {
    volume.into_inner().into_inner()
}

pub fn load(volume: &SdVolume) -> Result<WifiConfig, StorageError> {
    let root = volume.root_dir();
    let directory = root
        .open_dir(CONFIG_DIR)
        .map_err(|_| StorageError::Missing)?;
    let mut reader = directory
        .open_file(CONFIG_FILE)
        .map_err(|_| StorageError::Missing)?;

    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 256];
    loop {
        let count = reader.read(&mut chunk).map_err(|_| StorageError::Sd)?;
        if count == 0 {
            break;
        }
        if bytes.len() + count > MAX_CONFIG_BYTES {
            return Err(StorageError::TooLarge);
        }
        bytes.extend_from_slice(&chunk[..count]);
    }

    let text = core::str::from_utf8(&bytes).map_err(|_| StorageError::InvalidToml)?;
    let config: WifiConfig = toml::from_str(text).map_err(|_| StorageError::InvalidToml)?;
    config.validate()
}

pub fn save(volume: &SdVolume, config: &WifiConfig) -> Result<(), StorageError> {
    let text = toml::to_string(config).map_err(|_| StorageError::InvalidToml)?;
    if text.len() > MAX_CONFIG_BYTES {
        return Err(StorageError::TooLarge);
    }

    let root = volume.root_dir();
    let directory = match root.open_dir(CONFIG_DIR) {
        Ok(directory) => directory,
        Err(_) => volume
            .create_dir(&root, CONFIG_DIR)
            .map_err(|_| StorageError::Sd)?,
    };
    let entry = match directory.find(CONFIG_FILE) {
        Ok(Some(entry)) if !entry.is_directory() => entry,
        Ok(Some(_)) | Ok(None) => volume
            .create_file(&directory, CONFIG_FILE)
            .map_err(|_| StorageError::Sd)?,
        Err(_) => return Err(StorageError::Sd),
    };
    let mut writer =
        hadris_fat::sync::write::FileWriter::new(volume, &entry).map_err(|_| StorageError::Sd)?;
    writer
        .write(text.as_bytes())
        .map_err(|_| StorageError::Sd)?;
    writer.finish().map_err(|_| StorageError::Sd)
}

/// Local wall-clock time for FAT timestamps, published by the main loop once
/// SNTP has synced (seconds since 1970-01-01 local time; 0 = not synced yet).
static LOCAL_TIME: AtomicU32 = AtomicU32::new(0);

pub fn set_local_time(local_seconds: i64) {
    LOCAL_TIME.store(
        local_seconds.clamp(0, u32::MAX as i64) as u32,
        Ordering::Relaxed,
    );
}

/// Last published local time (None = no SNTP sync yet).
pub fn local_now_seconds() -> Option<i64> {
    let seconds = LOCAL_TIME.load(Ordering::Relaxed);
    (seconds != 0).then_some(i64::from(seconds))
}

/// FAT timestamp provider: current local time after SNTP sync, otherwise a
/// fixed 2026-01-01 fallback date so uploads still get a sane, monotonic date.
#[derive(Debug)]
pub struct FatClock;

impl TimeProvider for FatClock {
    fn now(&self) -> FatDateTime {
        let seconds = LOCAL_TIME.load(Ordering::Relaxed);
        if seconds == 0 {
            return FatDateTime::new(2026, 1, 1, 0, 0, 0);
        }
        let time = crate::clock::date_time(i64::from(seconds));
        FatDateTime::new(
            time.year as u16,
            time.month as u8,
            time.day as u8,
            time.hour as u8,
            time.minute as u8,
            time.second as u8,
        )
    }
}

/// Format a FAT timestamp as `YYYYMMDDHHMMSS` (used by FTP MDTM/MLST) with the
/// pre-sync fallback date.
pub fn fmt_fat_mtime(date: FatDateTime) -> String {
    let (raw_date, raw_time, _) = date.to_raw();
    let year = ((raw_date >> 9) & 0x7F) + 1980;
    alloc::format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        year,
        (raw_date >> 5) & 0x0F,
        raw_date & 0x1F,
        (raw_time >> 11) & 0x1F,
        (raw_time >> 5) & 0x3F,
        (raw_time & 0x1F) * 2
    )
}

/// Current local time for FTP LIST/MLSD fallbacks, 1980-01-01 before NTP sync.
pub fn now_ymdhms() -> String {
    match local_now_seconds() {
        Some(now) => {
            let time = crate::clock::date_time(now);
            alloc::format!(
                "{:04}{:02}{:02}{:02}{:02}{:02}",
                time.year,
                time.month,
                time.day,
                time.hour,
                time.minute,
                time.second
            )
        }
        None => String::from("19800101000000"),
    }
}
