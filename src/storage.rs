use alloc::{string::String, vec::Vec};
use core::sync::atomic::{AtomicU32, Ordering};

use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::delay::Delay;
use esp_hal::gpio::Output;
use esp_hal::spi::master::Spi;
use esp_hal::Blocking;

use embedded_sdmmc::{BlockDevice, Mode, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use serde::{Deserialize, Serialize};

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

pub fn load<D, T, const DIRS: usize, const FILES: usize, const VOLUMES: usize>(
    manager: &VolumeManager<D, T, DIRS, FILES, VOLUMES>,
) -> Result<WifiConfig, StorageError>
where
    D: BlockDevice,
    T: TimeSource,
{
    let volume = manager
        .open_volume(VolumeIdx(0))
        .map_err(|_| StorageError::Sd)?;
    let root = volume.open_root_dir().map_err(|_| StorageError::Sd)?;
    let directory = root
        .open_dir(CONFIG_DIR)
        .map_err(|_| StorageError::Missing)?;
    let file = directory
        .open_file_in_dir(CONFIG_FILE, Mode::ReadOnly)
        .map_err(|_| StorageError::Missing)?;

    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 128];
    while !file.is_eof() {
        let count = file.read(&mut chunk).map_err(|_| StorageError::Sd)?;
        if bytes.len() + count > MAX_CONFIG_BYTES {
            return Err(StorageError::TooLarge);
        }
        bytes.extend_from_slice(&chunk[..count]);
    }

    let text = core::str::from_utf8(&bytes).map_err(|_| StorageError::InvalidToml)?;
    let config: WifiConfig = toml::from_str(text).map_err(|_| StorageError::InvalidToml)?;
    config.validate()
}

pub fn save<D, T, const DIRS: usize, const FILES: usize, const VOLUMES: usize>(
    manager: &VolumeManager<D, T, DIRS, FILES, VOLUMES>,
    config: &WifiConfig,
) -> Result<(), StorageError>
where
    D: BlockDevice,
    T: TimeSource,
{
    let text = toml::to_string(config).map_err(|_| StorageError::InvalidToml)?;
    if text.len() > MAX_CONFIG_BYTES {
        return Err(StorageError::TooLarge);
    }

    let volume = manager
        .open_volume(VolumeIdx(0))
        .map_err(|_| StorageError::Sd)?;
    let root = volume.open_root_dir().map_err(|_| StorageError::Sd)?;
    if root.open_dir(CONFIG_DIR).is_err() {
        root.make_dir_in_dir(CONFIG_DIR)
            .map_err(|_| StorageError::Sd)?;
    }
    let directory = root.open_dir(CONFIG_DIR).map_err(|_| StorageError::Sd)?;
    let file = directory
        .open_file_in_dir(CONFIG_FILE, Mode::ReadWriteCreateOrTruncate)
        .map_err(|_| StorageError::Sd)?;
    file.write(text.as_bytes()).map_err(|_| StorageError::Sd)?;
    file.flush().map_err(|_| StorageError::Sd)
}

/// The concrete SD volume manager type used by the whole firmware.
pub type SdVolumeManager = embedded_sdmmc::VolumeManager<
    embedded_sdmmc::SdCard<ExclusiveDevice<Spi<'static, Blocking>, Output<'static>, Delay>, Delay>,
    FatClock,
>;

/// Local wall-clock time for FAT timestamps, published by the main loop once
/// SNTP has synced (seconds since 1970-01-01 local time; 0 = not synced yet).
static LOCAL_TIME: AtomicU32 = AtomicU32::new(0);

pub fn set_local_time(local_seconds: i64) {
    LOCAL_TIME.store(
        local_seconds.clamp(0, u32::MAX as i64) as u32,
        Ordering::Relaxed,
    );
}

/// FAT time source: the synced local time, or 2026-01-01 before the first sync.
/// Last published local time (0 = not synced yet).
pub fn local_now_seconds() -> Option<i64> {
    let seconds = LOCAL_TIME.load(Ordering::Relaxed);
    (seconds != 0).then_some(i64::from(seconds))
}

#[derive(Clone, Copy)]
pub struct FatClock;

impl TimeSource for FatClock {
    fn get_timestamp(&self) -> Timestamp {
        let seconds = LOCAL_TIME.load(Ordering::Relaxed);
        if seconds == 0 {
            return Timestamp {
                year_since_1970: 56,
                zero_indexed_month: 0,
                zero_indexed_day: 0,
                hours: 0,
                minutes: 0,
                seconds: 0,
            };
        }
        let time = crate::clock::date_time(i64::from(seconds));
        Timestamp {
            // FAT dates cover 1980..=2107.
            year_since_1970: (time.year - 1970).clamp(10, 137) as u8,
            zero_indexed_month: (time.month - 1) as u8,
            zero_indexed_day: (time.day - 1) as u8,
            hours: time.hour as u8,
            minutes: time.minute as u8,
            seconds: time.second as u8,
        }
    }
}
