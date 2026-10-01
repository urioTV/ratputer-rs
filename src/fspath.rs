//! Protocol-independent helpers for addressing the FAT volume by path, shared
//! by the SFTP server.
//!
//! Paths are component lists from the volume root. FAT handles returned here
//! borrow the volume, so callers use and drop them within one poll step.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use hadris_fat::sync::{FatDir, FileEntry};
use hadris_fat::time::FatDateTime;

use crate::storage::{now_ymdhms, SdBlock, SdVolume};

/// Apply a `/`-separated path to `base`, honouring `.`, `..` and a leading
/// `/`. Backslashes are not separators here; callers translate if needed.
/// `..` above the root stays at the root.
pub fn resolve(base: &[String], path: &str) -> Vec<String> {
    let mut result: Vec<String> = if path.starts_with('/') {
        Vec::new()
    } else {
        base.to_vec()
    };
    for component in path.split('/') {
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

/// `/a/b` form of a component list (`/` for the root).
pub fn display(components: &[String]) -> String {
    if components.is_empty() {
        String::from("/")
    } else {
        let mut text = String::new();
        for component in components {
            text.push('/');
            text.push_str(component);
        }
        text
    }
}

/// Open the directory at absolute components. Handle is borrowed for this poll.
pub fn open_dir_path<'a>(
    volume: &'a SdVolume,
    components: &[String],
) -> Option<FatDir<'a, SdBlock>> {
    let mut current = volume.root_dir();
    for component in components {
        current = current.open_dir(component).ok()?;
    }
    Some(current)
}

/// Find an entry by name inside a directory addressed by components.
pub fn find_in<'a>(
    volume: &'a SdVolume,
    parent: &[String],
    leaf: &str,
) -> Option<(FileEntry, FatDir<'a, SdBlock>)> {
    let dir = open_dir_path(volume, parent)?;
    let entry = dir.find(leaf).ok().flatten()?;
    Some((entry, dir))
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Unpack a FAT datetime into (year, month, day, hour, minute).
pub fn unpack_fat(dt: FatDateTime) -> (u16, u8, u8, u8, u8) {
    let (date, time, _) = dt.to_raw();
    (
        ((date >> 9) & 0x7F) + 1980,
        ((date >> 5) & 0x0F) as u8,
        (date & 0x1F) as u8,
        ((time >> 11) & 0x1F) as u8,
        ((time >> 5) & 0x3F) as u8,
    )
}

/// `ls -l`-style line, used by SFTP `READDIR` long names.
pub fn long_listing_line(entry: &FileEntry) -> String {
    let name = entry.name();
    let (year, month, day, hour, minute) = unpack_fat(entry.modified());
    let permissions = if entry.is_directory() {
        "drwxr-xr-x"
    } else {
        "-rw-r--r--"
    };
    let month_name = MONTHS[(month as usize).saturating_sub(1).min(11)];
    let current_year = now_ymdhms();
    let same_year = current_year.starts_with(&format!("{year:04}"));
    let when = if same_year {
        format!("{hour:02}:{minute:02}")
    } else {
        format!("{year:>5}")
    };
    format!(
        "{} 1 rat rat {:>13} {} {:>2} {} {}",
        permissions,
        entry.len(),
        month_name,
        day,
        when,
        name
    )
}
