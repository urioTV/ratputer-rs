use alloc::{string::String, vec::Vec};

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

#[derive(Debug, Deserialize, Serialize)]
pub struct WifiConfig {
    pub version: u8,
    #[serde(default)]
    pub networks: Vec<SavedNetwork>,
}

impl Default for WifiConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
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

#[derive(Clone, Copy)]
pub struct BuildTime;

impl TimeSource for BuildTime {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 56,
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}
