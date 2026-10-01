//! Exercises the actual firmware SFTP parser against disposable FAT images.
//! Only the hardware timer and storage type are replaced by host equivalents.
#![allow(dead_code)] // The real firmware modules expose extra APIs unused here.
extern crate alloc;
extern crate self as esp_hal;

pub mod time {
    #[derive(Clone, Copy)]
    pub struct Instant(std::time::Instant);
    pub struct Duration(std::time::Duration);
    impl Duration {
        pub fn as_micros(&self) -> u64 {
            self.0.as_micros() as u64
        }
        pub fn as_secs(&self) -> u64 {
            self.0.as_secs()
        }
    }
    impl Instant {
        pub fn now() -> Self {
            Self(std::time::Instant::now())
        }
        pub fn elapsed(&self) -> Duration {
            Duration(self.0.elapsed())
        }
    }
    impl core::ops::Sub for Instant {
        type Output = Duration;
        fn sub(self, rhs: Self) -> Self::Output {
            Duration(self.0.duration_since(rhs.0))
        }
    }
}
mod storage {
    pub type SdBlock = std::fs::File;
    pub type SdVolume = hadris_fat::sync::FatVolume<SdBlock>;
    #[derive(Clone, Copy, PartialEq)]
    pub enum DstRule {
        None,
        Eu,
    }
    pub struct ClockConfig {
        pub utc_offset_minutes: i16,
        pub dst: DstRule,
    }
    pub fn now_ymdhms() -> String {
        "20261001000000".into()
    }
}
#[path = "../../../src/clock.rs"]
mod clock;
#[path = "../../../src/fspath.rs"]
mod fspath;
#[path = "../../../src/sftp.rs"]
mod sftp;

use hadris_fat::sync::{read::FileReader, write::FileWriter, FatVolume};
use storage::ClockConfig;

fn u32v(v: &mut Vec<u8>, n: u32) {
    v.extend(n.to_be_bytes());
}
fn u64v(v: &mut Vec<u8>, n: u64) {
    v.extend(n.to_be_bytes());
}
fn string(v: &mut Vec<u8>, s: &[u8]) {
    u32v(v, s.len() as u32);
    v.extend(s);
}
fn attrs(v: &mut Vec<u8>, size: Option<u64>, mode: Option<u32>, times: Option<(u32, u32)>) {
    u32v(
        v,
        u32::from(size.is_some())
            | (u32::from(mode.is_some()) << 2)
            | (u32::from(times.is_some()) << 3),
    );
    if let Some(n) = size {
        u64v(v, n);
    }
    if let Some(n) = mode {
        u32v(v, n);
    }
    if let Some((a, m)) = times {
        u32v(v, a);
        u32v(v, m);
    }
}
fn request(
    s: &mut sftp::Sftp,
    fs: &storage::SdVolume,
    cfg: &ClockConfig,
    kind: u8,
    body: Vec<u8>,
) -> Vec<u8> {
    let mut packet = vec![];
    u32v(&mut packet, body.len() as u32 + 5);
    packet.push(kind);
    u32v(&mut packet, 123);
    packet.extend(body);
    // Fragment the wire stream to cover streamed header/payload handling.
    let mut sent = 0;
    let mut reply = vec![];
    for _ in 0..100000 {
        let n = s.input_space().min(packet.len() - sent).min(997);
        s.feed(&packet[sent..sent + n]);
        sent += n;
        s.begin_poll();
        s.step(Some(fs), cfg);
        let n = s.output().len();
        reply.extend_from_slice(s.output());
        s.consume_output(n);
        assert!(!s.is_fatal(), "SFTP fatal for request {kind}");
        if reply.len() >= 4 {
            let len = u32::from_be_bytes(reply[..4].try_into().unwrap()) as usize + 4;
            if reply.len() == len {
                assert_eq!(sent, packet.len());
                return reply;
            }
        }
    }
    panic!("request {kind} never completed");
}
fn code(r: &[u8]) -> u32 {
    assert_eq!(r[4], 101, "not STATUS: {r:?}");
    u32::from_be_bytes(r[9..13].try_into().unwrap())
}
fn ok(r: Vec<u8>) {
    assert_eq!(code(&r), 0, "{r:?}");
}
fn open(
    s: &mut sftp::Sftp,
    fs: &storage::SdVolume,
    cfg: &ClockConfig,
    path: &str,
    flags: u32,
) -> Vec<u8> {
    let mut b = vec![];
    string(&mut b, path.as_bytes());
    u32v(&mut b, flags);
    attrs(&mut b, None, None, None);
    let r = request(s, fs, cfg, 3, b);
    assert_eq!(r[4], 102, "{r:?}");
    r[13..].to_vec()
}
fn write(
    s: &mut sftp::Sftp,
    fs: &storage::SdVolume,
    cfg: &ClockConfig,
    h: &[u8],
    offset: u64,
    bytes: &[u8],
) -> Vec<u8> {
    let mut b = vec![];
    string(&mut b, h);
    u64v(&mut b, offset);
    string(&mut b, bytes);
    request(s, fs, cfg, 6, b)
}
fn set(
    s: &mut sftp::Sftp,
    fs: &storage::SdVolume,
    cfg: &ClockConfig,
    h: &[u8],
    size: Option<u64>,
    mode: Option<u32>,
    times: Option<(u32, u32)>,
) -> Vec<u8> {
    let mut b = vec![];
    string(&mut b, h);
    attrs(&mut b, size, mode, times);
    request(s, fs, cfg, 10, b)
}
fn read_all(fs: &storage::SdVolume) -> Vec<u8> {
    let e = fs.root_dir().find("data.bin").unwrap().unwrap();
    let mut r = FileReader::new(fs, &e).unwrap();
    let mut out = vec![0; e.len() as usize];
    let mut at = 0;
    while at < out.len() {
        let n = r.read(&mut out[at..]).unwrap();
        assert_ne!(n, 0);
        at += n;
    }
    out
}
fn main() {
    let image = std::env::args().nth(1).expect("FAT image path");
    let disk = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&image)
        .unwrap();
    let fs = FatVolume::open(disk).unwrap();
    // Independent low-level test: no-op and middle writers preserve the tail.
    let root = fs.root_dir();
    let e = fs.create_file(&root, "data.bin").unwrap();
    let expected: Vec<u8> = (0..100_003).map(|i| (i * 37) as u8).collect();
    let mut w = FileWriter::new(&fs, &e).unwrap();
    w.write(&expected).unwrap();
    w.finish().unwrap();
    let e = root.find("data.bin").unwrap().unwrap();
    let w = FileWriter::new_at(&fs, &e, 0).unwrap();
    w.finish().unwrap();
    assert_eq!(read_all(&fs), expected);
    let mut s = sftp::Sftp::new();
    let cfg = ClockConfig {
        utc_offset_minutes: 60,
        dst: storage::DstRule::Eu,
    };
    let h = open(&mut s, &fs, &cfg, "/data.bin", 3);
    let mut expected = expected;
    for (offset, n) in [
        (0, 1),
        (511, 17),
        (4090, 32768),
        (32768, 1),
        (65535, 8192),
        (99990, 50),
    ] {
        let patch: Vec<u8> = (0..n).map(|i| (i * 11 + 3) as u8).collect();
        ok(write(&mut s, &fs, &cfg, &h, offset as u64, &patch));
        expected.resize(expected.len().max(offset + n), 0);
        expected[offset..offset + n].copy_from_slice(&patch);
        assert_eq!(read_all(&fs), expected, "random write at {offset}");
    }
    ok(set(&mut s, &fs, &cfg, &h, Some(200123), None, None));
    expected.resize(200123, 0);
    assert_eq!(read_all(&fs), expected);
    ok(write(&mut s, &fs, &cfg, &h, 210000, b"gap tail"));
    expected.resize(210008, 0);
    expected[210000..].copy_from_slice(b"gap tail");
    assert_eq!(read_all(&fs), expected);
    for len in [65536, 32768, 511, 0] {
        ok(set(&mut s, &fs, &cfg, &h, Some(len), None, None));
        expected.truncate(len as usize);
        assert_eq!(read_all(&fs), expected);
    }
    ok(set(&mut s, &fs, &cfg, &h, Some(40000), None, None));
    assert_eq!(read_all(&fs), vec![0; 40000]);
    ok(write(&mut s, &fs, &cfg, &h, 12000, b"middle"));
    // FAT modification times round down to even seconds; access time is a date.
    let t = 1_700_000_000;
    ok(set(&mut s, &fs, &cfg, &h, None, Some(0o444), Some((t, t))));
    let e = root.find("data.bin").unwrap().unwrap();
    assert!(e
        .attributes()
        .contains(hadris_fat::raw::DirEntryAttrFlags::READ_ONLY));
    let dt = clock::date_time(clock::local_seconds(t as u64, &cfg));
    assert_eq!(
        e.modified(),
        hadris_fat::time::FatDateTime::new(
            dt.year as u16,
            dt.month as u8,
            dt.day as u8,
            dt.hour as u8,
            dt.minute as u8,
            dt.second as u8
        )
    );
    assert_eq!(code(&write(&mut s, &fs, &cfg, &h, 0, b"no")), 3);
    ok(set(&mut s, &fs, &cfg, &h, None, Some(0o644), None));
    assert_eq!(
        code(&set(
            &mut s,
            &fs,
            &cfg,
            &h,
            Some(u32::MAX as u64 + 1),
            None,
            None
        )),
        4
    );
    assert_eq!(
        code(&set(&mut s, &fs, &cfg, &h, None, None, Some((1, 1)))),
        8
    );
    let mut b = vec![];
    string(&mut b, b"fsync@openssh.com");
    string(&mut b, &h);
    ok(request(&mut s, &fs, &cfg, 200, b));
    let mut b = vec![];
    string(&mut b, b"limits@openssh.com");
    let r = request(&mut s, &fs, &cfg, 200, b);
    assert_eq!(r[4], 201);
    assert_eq!(r.len(), 41);
    // Slot reuse must not revive stale handles, nor invalidate another live one.
    let h2 = open(&mut s, &fs, &cfg, "/data.bin", 1);
    let mut b = vec![];
    string(&mut b, &h);
    ok(request(&mut s, &fs, &cfg, 4, b));
    let h3 = open(&mut s, &fs, &cfg, "/data.bin", 3);
    assert_ne!(h, h3);
    assert_ne!(code(&write(&mut s, &fs, &cfg, &h, 0, b"bad")), 0);
    let mut b = vec![];
    string(&mut b, &h2);
    let r = request(&mut s, &fs, &cfg, 8, b);
    assert_eq!(r[4], 105);
    for token in [h2, h3] {
        let mut b = vec![];
        string(&mut b, &token);
        ok(request(&mut s, &fs, &cfg, 4, b));
    }
    // APPEND ignores client offsets; random OPEN/TRUNC preserves the same entry.
    let h = open(&mut s, &fs, &cfg, "/data.bin", 6);
    ok(write(&mut s, &fs, &cfg, &h, 0, b"append"));
    assert_eq!(&read_all(&fs)[40000..], b"append");
    let mut b = vec![];
    string(&mut b, &h);
    ok(request(&mut s, &fs, &cfg, 4, b));
    let h = open(&mut s, &fs, &cfg, "/data.bin", 0x12);
    assert!(read_all(&fs).is_empty());
    ok(write(&mut s, &fs, &cfg, &h, 32768, b"boundary"));
    let data = read_all(&fs);
    assert_eq!(&data[..32768], vec![0; 32768]);
    assert_eq!(&data[32768..], b"boundary");
    let mut b = vec![];
    string(&mut b, b"statvfs@openssh.com");
    string(&mut b, b"/");
    let r = request(&mut s, &fs, &cfg, 200, b);
    if fs.free_cluster_count().is_some() {
        assert_eq!(r[4], 201);
        assert_eq!(r.len(), 97);
    } else {
        assert_eq!(code(&r), 8);
    }
    let mut b = vec![];
    string(&mut b, &h);
    ok(request(&mut s, &fs, &cfg, 4, b));
    assert!(!s.has_open_handles());
    assert!(!s.transferring());
    fs.sync().unwrap();
    println!("SFTP/FAT image checks passed: {image}");
}
