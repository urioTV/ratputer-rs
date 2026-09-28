//! On-device SD browser. FAT handles never outlive a single main-loop step;
//! USB MSC and FTP cannot run while this view is open.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use hadris_fat::sync::{
    read::{FileReader, ReadCursor},
    write::{AppendCursor, FileWriter},
    FatDir, FileEntry,
};
use slint::SharedString;

use crate::storage::{SdBlock, SdVolume};
use crate::MainWindow;

const PAGE_LEN: usize = 24;
/// Rows that fit the browser viewport: the six static `fm-row-N` slots in the
/// UI. Static rows cost nothing at runtime; a `for` repeater allocated ~2 KiB
/// per row and kept it after leaving the screen, starving FTP of heap.
const VISIBLE_ROWS: usize = 6;
const COPY_CHUNK: usize = 2048;
const MAX_COPY_DEPTH: usize = 12;
const MAX_NAME_BYTES: usize = 80;
const ACTIONS: [&str; 7] = [
    "COPY", "MOVE", "PASTE", "RENAME", "NEW FILE", "NEW DIR", "DELETE",
];

#[derive(Clone, Copy)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Back,
    Backspace,
    Delete,
    Tab,
    Char(char),
}

#[derive(Clone)]
struct Item {
    name: String,
    directory: bool,
    size: u64,
}

#[derive(Clone)]
struct Clipboard {
    parent: Vec<String>,
    name: String,
    directory: bool,
    moving: bool,
}

#[derive(Clone, Copy)]
enum NameAction {
    Rename,
    File,
    Directory,
}

struct DirectoryCopy {
    source: Vec<String>,
    target: Vec<String>,
    next: usize,
}

struct FileCopy {
    source: Vec<String>,
    target: Vec<String>,
    name: String,
    read_cursor: Option<ReadCursor>,
    write_cursor: Option<AppendCursor>,
    started: bool,
}

pub struct CopyJob {
    directories: Vec<DirectoryCopy>,
    file: Option<FileCopy>,
    copied: u64,
    files: usize,
}

pub struct FileManager {
    cwd: Vec<String>,
    page: usize,
    selected: usize,
    /// First page entry shown in the viewport.
    first: usize,
    entries: Vec<Item>,
    more: bool,
    clipboard: Option<Clipboard>,
    action: usize,
    name_action: NameAction,
    input: String,
    confirm_name: Option<String>,
    status: String,
    /// Progress label granularity guard (KB already displayed).
    shown_kb: u64,
    copy: Option<CopyJob>,
}

fn open_dir<'a>(volume: &'a SdVolume, path: &[String]) -> Result<FatDir<'a, SdBlock>, ()> {
    let mut dir = volume.root_dir();
    for component in path {
        dir = dir.open_dir(component).map_err(|_| ())?;
    }
    Ok(dir)
}

fn lookup(volume: &SdVolume, parent: &[String], name: &str) -> Result<FileEntry, ()> {
    open_dir(volume, parent)?
        .find(name)
        .map_err(|_| ())?
        .ok_or(())
}

/// Return one directory item without retaining a FAT iterator across UI frames.
fn item_at(volume: &SdVolume, path: &[String], index: usize) -> Result<Option<Item>, ()> {
    let dir = open_dir(volume, path)?;
    let mut items = dir.entries();
    let mut position = 0;
    while let Some(record) = items.next_entry() {
        let record = record.map_err(|_| ())?;
        let entry = record.as_entry().ok_or(())?;
        let name = entry.name().into_owned();
        if name == "." || name == ".." {
            continue;
        }
        if position == index {
            return Ok(Some(Item {
                name,
                directory: entry.is_directory(),
                size: entry.len(),
            }));
        }
        position += 1;
    }
    Ok(None)
}

/// One row fits 28 glyphs. Truncate the NAME, never the size: a clipped
/// "20833" tells nothing, while a clipped name is still recognizable ("~").
fn row_label(item: &Item) -> SharedString {
    const WIDTH: usize = 28;
    if item.directory {
        let mut label = format!("[{}]", item.name);
        if label.len() > WIDTH {
            label.truncate(WIDTH - 1);
            label.push('~');
        }
        return SharedString::from(label);
    }
    let size = format!("{}B", item.size);
    let budget = WIDTH.saturating_sub(size.len() + 1);
    let mut name: String = crate::truncate_ascii(&item.name, budget);
    if name.len() < item.name.len() && name.len() > 1 {
        name.truncate(name.len() - 1);
        name.push('~');
    }
    SharedString::from(format!("{name} {size}"))
}

fn inside(path: &[String], ancestor: &[String]) -> bool {
    path.len() >= ancestor.len()
        && path
            .iter()
            .zip(ancestor)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= MAX_NAME_BYTES
        && !name.ends_with([' ', '.'])
        && !name.bytes().any(|b| {
            b < 32
                || b == b'/'
                || b == b'\\'
                || b == b':'
                || b == b'*'
                || b == b'?'
                || b == b'"'
                || b == b'<'
                || b == b'>'
                || b == b'|'
        })
}

impl FileManager {
    /// Absolute selection within the current page, for the debug console.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn copying(&self) -> bool {
        self.copy.is_some()
    }

    pub fn new(volume: &SdVolume, ui: &MainWindow) -> Self {
        let mut manager = Self {
            cwd: Vec::new(),
            page: 0,
            selected: 0,
            first: 0,
            entries: Vec::new(),
            more: false,
            clipboard: None,
            action: 0,
            name_action: NameAction::File,
            input: String::new(),
            confirm_name: None,
            status: String::new(),
            shown_kb: 0,
            copy: None,
        };
        manager.reload(volume);
        manager.show(ui);
        manager
    }

    fn reload(&mut self, volume: &SdVolume) {
        self.entries.clear();
        let result = (|| {
            let dir = open_dir(volume, &self.cwd)?;
            let mut iter = dir.entries();
            let mut seen = 0;
            while let Some(record) = iter.next_entry() {
                let record = record.map_err(|_| ())?;
                let entry = record.as_entry().ok_or(())?;
                let name = entry.name().into_owned();
                if name == "." || name == ".." {
                    continue;
                }
                if seen >= self.page && self.entries.len() <= PAGE_LEN {
                    self.entries.push(Item {
                        name,
                        directory: entry.is_directory(),
                        size: entry.len(),
                    });
                }
                seen += 1;
                if self.entries.len() > PAGE_LEN {
                    break;
                }
            }
            Ok::<_, ()>(())
        })();
        if result.is_err() {
            self.entries.clear();
            self.status = "DIR UNREADABLE - BS BACK".into();
        }
        self.more = self.entries.len() > PAGE_LEN;
        self.entries.truncate(PAGE_LEN);
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
    }

    fn show(&mut self, ui: &MainWindow) {
        let path = if self.cwd.is_empty() {
            String::from("/")
        } else {
            format!("/{}", self.cwd.join("/"))
        };
        ui.set_fm_path(crate::truncate_ascii(&path, 28).into());
        ui.set_fm_status(crate::truncate_ascii(&self.status, 29).into());
        // Keep the selection inside a stable viewport (scroll only at edges).
        self.first = self.first.min(self.selected);
        if self.selected >= self.first + VISIBLE_ROWS {
            self.first = self.selected + 1 - VISIBLE_ROWS;
        }
        self.first = self
            .first
            .min(self.entries.len().saturating_sub(VISIBLE_ROWS));
        ui.set_fm_index((self.selected - self.first) as i32);
        ui.set_fm_action_index(self.action as i32);
        ui.set_fm_name(self.input.as_str().into());
        ui.set_fm_target(self.confirm_name.as_deref().unwrap_or("").into());
        let mut rows = self
            .entries
            .iter()
            .skip(self.first)
            .take(VISIBLE_ROWS)
            .map(row_label);
        let setters: [fn(&MainWindow, SharedString); VISIBLE_ROWS] = [
            MainWindow::set_fm_row_0,
            MainWindow::set_fm_row_1,
            MainWindow::set_fm_row_2,
            MainWindow::set_fm_row_3,
            MainWindow::set_fm_row_4,
            MainWindow::set_fm_row_5,
        ];
        let mut count = 0;
        for set in setters {
            let label = rows.next();
            count += usize::from(label.is_some());
            set(ui, label.unwrap_or_default());
        }
        ui.set_fm_count(count as i32);
        ui.set_fm_more(self.more);
        ui.set_fm_page((self.page / PAGE_LEN + 1) as i32);
        ui.set_fm_clipboard(if let Some(ref clip) = self.clipboard {
            format!(
                "{}: {}",
                if clip.moving { "MOVE" } else { "COPY" },
                crate::truncate_ascii(&clip.name, 20)
            )
            .into()
        } else {
            "".into()
        });
    }

    /// Label of the selected entry as shown on screen, for the debug console.
    pub fn selected_label(&self) -> Option<SharedString> {
        self.entries.get(self.selected).map(row_label)
    }

    fn selected_item(&self) -> Option<Item> {
        self.entries.get(self.selected).cloned()
    }

    fn actions(&mut self, ui: &MainWindow) {
        self.action = 0;
        ui.set_view_state(11);
        self.show(ui);
    }

    fn name_input(&mut self, action: NameAction, text: &str, ui: &MainWindow) {
        self.name_action = action;
        self.input = text.into();
        ui.set_fm_prompt(
            match action {
                NameAction::Rename => "RENAME TO",
                NameAction::File => "NEW FILE",
                NameAction::Directory => "NEW DIRECTORY",
            }
            .into(),
        );
        ui.set_view_state(12);
        self.show(ui);
    }

    fn delete_prompt(&mut self, ui: &MainWindow) {
        if let Some(item) = self.selected_item() {
            self.confirm_name = Some(item.name);
            ui.set_view_state(13);
            self.show(ui);
        }
    }

    fn paste(&mut self, volume: &SdVolume, ui: &MainWindow) {
        let Some(clip) = self.clipboard.clone() else {
            self.status = "CLIPBOARD EMPTY".into();
            return;
        };
        let mut source_path = clip.parent.clone();
        source_path.push(clip.name.clone());
        if clip.directory && inside(&self.cwd, &source_path) {
            self.status = "CANNOT PASTE INTO SELF".into();
            return;
        }
        if self.cwd == clip.parent {
            self.status = "SAME DIRECTORY".into();
            return;
        }
        let Ok(source) = lookup(volume, &clip.parent, &clip.name) else {
            self.status = "SOURCE NOT FOUND".into();
            return;
        };
        if source.is_directory() != clip.directory {
            self.status = "SOURCE CHANGED".into();
            return;
        }
        let Ok(target) = open_dir(volume, &self.cwd) else {
            self.status = "DESTINATION ERROR".into();
            return;
        };
        if !matches!(target.find(&clip.name), Ok(None)) {
            self.status = "NAME EXISTS OR SD ERROR".into();
            return;
        }
        if clip.moving {
            if volume.rename(&source, &target, &clip.name).is_ok() {
                self.clipboard = None;
                self.status = "MOVED".into();
                self.reload(volume);
            } else {
                self.status = "MOVE FAILED".into();
            }
        } else if clip.directory {
            match volume.create_dir(&target, &clip.name) {
                Ok(_) => {
                    let mut dst = self.cwd.clone();
                    dst.push(clip.name);
                    self.copy = Some(CopyJob {
                        directories: alloc::vec![DirectoryCopy {
                            source: source_path,
                            target: dst,
                            next: 0
                        }],
                        file: None,
                        copied: 0,
                        files: 0,
                    });
                    self.status = "COPYING DIRECTORY...".into();
                }
                Err(_) => self.status = "COPY FAILED".into(),
            }
        } else {
            self.copy = Some(CopyJob {
                directories: Vec::new(),
                file: Some(FileCopy {
                    source: clip.parent,
                    target: self.cwd.clone(),
                    name: clip.name,
                    read_cursor: None,
                    write_cursor: None,
                    started: false,
                }),
                copied: 0,
                files: 0,
            });
            self.status = "COPYING FILE...".into();
        }
        self.show(ui);
    }

    fn apply_action(&mut self, volume: &SdVolume, ui: &MainWindow) {
        ui.set_view_state(10);
        match self.action {
            0 | 1 => {
                if let Some(item) = self.selected_item() {
                    self.clipboard = Some(Clipboard {
                        parent: self.cwd.clone(),
                        name: item.name,
                        directory: item.directory,
                        moving: self.action == 1,
                    });
                    self.status = if self.action == 1 {
                        "MOVE: NAVIGATE, PASTE"
                    } else {
                        "COPY: NAVIGATE, PASTE"
                    }
                    .into();
                } else {
                    self.status = "SELECT A FILE".into();
                }
            }
            2 => self.paste(volume, ui),
            3 => {
                if let Some(item) = self.selected_item() {
                    self.name_input(NameAction::Rename, &item.name, ui);
                } else {
                    self.status = "SELECT A FILE".into();
                }
            }
            4 => self.name_input(NameAction::File, "", ui),
            5 => self.name_input(NameAction::Directory, "", ui),
            6 => self.delete_prompt(ui),
            _ => {}
        }
        self.show(ui);
    }

    fn submit_name(&mut self, volume: &SdVolume, ui: &MainWindow) {
        if !valid_name(&self.input) {
            self.status = "INVALID NAME (MAX 80)".into();
            self.show(ui);
            return;
        }
        let result = (|| {
            let dir = open_dir(volume, &self.cwd)?;
            if !matches!(dir.find(&self.input), Ok(None)) {
                self.status = "NAME EXISTS OR SD ERROR".into();
                return Err(());
            }
            match self.name_action {
                NameAction::Rename => {
                    let item = self.selected_item().ok_or(())?;
                    let old = dir.find(&item.name).map_err(|_| ())?.ok_or(())?;
                    volume.rename(&old, &dir, &self.input).map_err(|_| ())?;
                    if self
                        .clipboard
                        .as_ref()
                        .is_some_and(|clip| clip.parent == self.cwd && clip.name == item.name)
                    {
                        self.clipboard = None;
                    }
                }
                NameAction::File => {
                    volume.create_file(&dir, &self.input).map_err(|_| ())?;
                }
                NameAction::Directory => {
                    volume.create_dir(&dir, &self.input).map_err(|_| ())?;
                }
            }
            Ok::<_, ()>(())
        })();
        if result.is_ok() {
            self.status = "SAVED".into();
            self.reload(volume);
            ui.set_view_state(10);
        } else if self.status != "NAME EXISTS OR SD ERROR" {
            self.status = "SD WRITE FAILED".into();
        }
        self.show(ui);
    }

    fn confirm_delete(&mut self, volume: &SdVolume, ui: &MainWindow) {
        if let Some(name) = self.confirm_name.take() {
            let outcome = match lookup(volume, &self.cwd, &name) {
                Ok(entry) => match volume.delete(&entry) {
                    Ok(()) => Ok("DELETED"),
                    // Directories left behind by an interrupted create (old
                    // hadris-fat: parent entry written, cluster never zeroed)
                    // fail with arbitrary parse errors. The repair has its own
                    // strict guards and wipes only that directory's single
                    // cluster, never the chains its stale bytes point at.
                    Err(error) if entry.is_directory() => {
                        match volume.repair_uninitialized_dir(&entry) {
                            Ok(()) => {
                                log::warn!("Repaired uninitialized directory {name}");
                                lookup(volume, &self.cwd, &name)
                                    .and_then(|entry| volume.delete(&entry).map_err(|_| ()))
                                    .map(|()| "BROKEN DIR REPAIRED+DELETED")
                                    .map_err(|()| "REPAIRED, DELETE FAILED")
                            }
                            Err(_) if matches!(error, hadris_fat::Error::DirectoryNotEmpty) => {
                                Err("DIR NOT EMPTY")
                            }
                            Err(_) => Err("DELETE FAILED - SD ERROR"),
                        }
                    }
                    Err(_) => Err("DELETE FAILED - SD ERROR"),
                },
                Err(()) => Err("NOT FOUND"),
            };
            if outcome.is_ok()
                && self
                    .clipboard
                    .as_ref()
                    .is_some_and(|clip| clip.parent == self.cwd && clip.name == name)
            {
                self.clipboard = None;
            }
            self.status = match outcome {
                Ok(text) | Err(text) => text.into(),
            };
        }
        ui.set_view_state(10);
        self.reload(volume);
        self.show(ui);
    }

    pub fn key(&mut self, key: Key, volume: &SdVolume, ui: &MainWindow) {
        if self.copy.is_some() {
            return;
        }
        match ui.get_view_state() {
            10 => match key {
                Key::Up if self.selected > 0 => self.selected -= 1,
                Key::Down if self.selected + 1 < self.entries.len() => self.selected += 1,
                Key::Left if self.page > 0 => {
                    self.page -= PAGE_LEN;
                    self.selected = 0;
                    self.reload(volume);
                }
                Key::Right if self.more => {
                    self.page += PAGE_LEN;
                    self.selected = 0;
                    self.reload(volume);
                }
                Key::Enter => {
                    if let Some(item) = self.selected_item() {
                        if item.directory {
                            self.cwd.push(item.name);
                            self.page = 0;
                            self.selected = 0;
                            self.status.clear();
                            self.reload(volume);
                        } else {
                            self.status = format!("{} BYTES  TAB: ACTIONS", item.size);
                        }
                    }
                }
                Key::Back | Key::Backspace => {
                    if self.cwd.pop().is_some() {
                        self.page = 0;
                        self.selected = 0;
                        self.status.clear();
                        self.reload(volume);
                    } else {
                        ui.set_view_state(0);
                    }
                }
                Key::Tab => self.actions(ui),
                Key::Delete => self.delete_prompt(ui),
                _ => {}
            },
            11 => match key {
                Key::Up => self.action = (self.action + ACTIONS.len() - 1) % ACTIONS.len(),
                Key::Down | Key::Tab => self.action = (self.action + 1) % ACTIONS.len(),
                Key::Enter => self.apply_action(volume, ui),
                Key::Back | Key::Backspace => ui.set_view_state(10),
                _ => {}
            },
            12 => match key {
                Key::Char(c) if c.is_ascii_graphic() || c == ' ' => {
                    if self.input.len() < MAX_NAME_BYTES {
                        self.input.push(c);
                    }
                }
                Key::Backspace => {
                    self.input.pop();
                }
                Key::Enter => self.submit_name(volume, ui),
                Key::Back => ui.set_view_state(10),
                _ => {}
            },
            13 => match key {
                Key::Enter => self.confirm_delete(volume, ui),
                Key::Back | Key::Backspace => {
                    self.confirm_name = None;
                    ui.set_view_state(10);
                }
                _ => {}
            },
            _ => {}
        }
        self.show(ui);
    }

    pub fn append_text(&mut self, text: &str, ui: &MainWindow) -> Result<(), &'static str> {
        if ui.get_view_state() != 12 {
            return Err("text_entry_not_active");
        }
        if !text.bytes().all(|b| (32..=126).contains(&b))
            || self.input.len() + text.len() > MAX_NAME_BYTES
        {
            return Err("invalid_or_long_filename");
        }
        self.input.push_str(text);
        self.show(ui);
        Ok(())
    }

    pub fn clear_text(&mut self, ui: &MainWindow) {
        self.input.clear();
        self.show(ui);
    }

    /// Perform one bounded FAT operation per main-loop pass. No open handles
    /// survive it. The progress label only changes every >=16 KB, avoiding
    /// repeated scene updates for otherwise identical status text.
    pub fn poll(&mut self, volume: &SdVolume, ui: &MainWindow) {
        let Some(job) = self.copy.as_mut() else {
            return;
        };
        let copied_kb = job.copied / 1024;
        match job.step(volume) {
            Ok(true) => {
                self.status = format!("COPIED {} FILES {} KB", job.files, copied_kb);
                self.copy = None;
                self.reload(volume);
            }
            Ok(false) => {
                if copied_kb / 16 > self.shown_kb / 16 {
                    self.status = format!("COPYING {} FILES {} KB", job.files, copied_kb);
                }
                self.shown_kb = copied_kb;
            }
            Err(()) => {
                self.status = "COPY FAILED - PARTIAL DEST".into();
                self.copy = None;
                self.reload(volume);
            }
        }
        self.show(ui);
    }
}

impl CopyJob {
    /// true = complete; errors leave the original intact and warn that the
    /// destination may be partial. Never overwrite a destination entry.
    fn step(&mut self, volume: &SdVolume) -> Result<bool, ()> {
        if let Some(file) = self.file.as_mut() {
            let parent = open_dir(volume, &file.source)?;
            let source = parent.find(&file.name).map_err(|_| ())?.ok_or(())?;
            if source.is_directory() {
                return Err(());
            }
            let dest = open_dir(volume, &file.target)?;
            let target = if file.started {
                dest.find(&file.name).map_err(|_| ())?.ok_or(())?
            } else {
                if !matches!(dest.find(&file.name), Ok(None)) {
                    return Err(());
                }
                volume.create_file(&dest, &file.name).map_err(|_| ())?
            };
            let mut reader = if let Some(cursor) = file.read_cursor.take() {
                FileReader::new_from_cursor(volume, &source, cursor).map_err(|_| ())?
            } else {
                FileReader::new(volume, &source).map_err(|_| ())?
            };
            let mut buffer = [0u8; COPY_CHUNK];
            let count = reader.read(&mut buffer).map_err(|_| ())?;
            file.read_cursor = Some(reader.cursor());
            drop(reader);
            let mut writer = if file.started {
                FileWriter::new_append_from_cursor(
                    volume,
                    &target,
                    file.write_cursor.take().ok_or(())?,
                )
                .map_err(|_| ())?
            } else {
                FileWriter::new(volume, &target).map_err(|_| ())?
            };
            if count > 0
                && !matches!(writer.write(&buffer[..count]), Ok(written) if written == count)
            {
                // Even on an I/O error, try to commit the partial size so FAT
                // can reclaim any allocated clusters when the user deletes it.
                let _ = writer.finish();
                return Err(());
            }
            let cursor = writer.append_cursor();
            writer.finish().map_err(|_| ())?;
            file.started = true;
            file.write_cursor = Some(cursor);
            self.copied += count as u64;
            if count == 0 {
                self.files += 1;
                self.file = None;
            }
            return Ok(false);
        }
        let depth = self.directories.len();
        let Some(frame) = self.directories.last_mut() else {
            return Ok(true);
        };
        let Some(item) = item_at(volume, &frame.source, frame.next)? else {
            self.directories.pop();
            return Ok(false);
        };
        frame.next += 1;
        if item.directory {
            if depth >= MAX_COPY_DEPTH {
                return Err(());
            }
            let target = open_dir(volume, &frame.target)?;
            if !matches!(target.find(&item.name), Ok(None)) {
                return Err(());
            }
            volume.create_dir(&target, &item.name).map_err(|_| ())?;
            let mut source = frame.source.clone();
            source.push(item.name.clone());
            let mut dest = frame.target.clone();
            dest.push(item.name);
            self.directories.push(DirectoryCopy {
                source,
                target: dest,
                next: 0,
            });
        } else {
            self.file = Some(FileCopy {
                source: frame.source.clone(),
                target: frame.target.clone(),
                name: item.name,
                read_cursor: None,
                write_cursor: None,
                started: false,
            });
        }
        Ok(false)
    }
}
