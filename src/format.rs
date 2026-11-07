//! Append-only `.knit` file: header, framed records, flock, crash-tolerant reads.

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const MAGIC: &[u8; 4] = b"KNIT";
pub const VERSION: u32 = 1;
pub const HEADER_LEN: usize = 32;

pub const KIND_BLOB: u8 = 0x01;
pub const KIND_TREE: u8 = 0x02;
pub const KIND_KNOT: u8 = 0x03;
pub const KIND_REF: u8 = 0x04;
pub const KIND_HEAD: u8 = 0x05;
pub const KIND_INDEX: u8 = 0x06;
pub const KIND_MARK: u8 = 0x07;
pub const KIND_STASH: u8 = 0x08;
pub const KIND_TOMBSTONE: u8 = 0x09;
pub const KIND_KEY: u8 = 0x0A;
pub const KIND_JOIN: u8 = 0x0B;
pub const KIND_HUB: u8 = 0x0C;
pub const KIND_STASH_DROP: u8 = 0x0D;

pub const FRAME_HEADER_LEN: usize = 5; // u32 len + u8 kind
pub const FRAME_TRAILER_LEN: usize = 4; // u32 crc32

#[derive(Debug)]
pub struct Frame {
    pub offset: u64,
    pub kind: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    /// Byte offset of the payload inside the file.
    pub fn payload_offset(&self) -> u64 {
        self.offset + FRAME_HEADER_LEN as u64
    }
}

fn frame_bytes(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len() + FRAME_TRAILER_LEN);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    let crc = crc32fast::hash(&out[FRAME_HEADER_LEN - 1..]);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

pub fn encode_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    frame_bytes(kind, payload)
}

/// Write a fresh `.knit` file with just the header.
pub fn create(path: &Path) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("Failed to create {}", path.display()))?;
    let mut header = [0u8; HEADER_LEN];
    header[..4].copy_from_slice(MAGIC);
    header[4..8].copy_from_slice(&VERSION.to_le_bytes());
    f.write_all(&header)?;
    f.sync_data()?;
    Ok(())
}

fn check_header(file: &mut File) -> Result<()> {
    let mut header = [0u8; HEADER_LEN];
    file.read_exact(&mut header)?;
    if &header[..4] != MAGIC {
        bail!("Not a knit file (bad magic)");
    }
    let version = u32::from_le_bytes(header[4..8].try_into().unwrap());
    if version != VERSION {
        bail!("Unsupported .knit version {version}");
    }
    Ok(())
}

/// Read every complete, valid frame. A torn tail (crash mid-append) ends the
/// scan silently; corrupt frames in the middle are an error only if they have
/// a plausible length but bad CRC - in practice we stop at the first bad frame
/// either way, matching append-only semantics.
pub fn read_frames(path: &Path) -> Result<Vec<Frame>> {
    let mut file = File::open(path)?;
    check_header(&mut file)?;
    let mut frames = Vec::new();
    let mut pos = HEADER_LEN as u64;
    let file_len = file.metadata()?.len();
    loop {
        if pos + FRAME_HEADER_LEN as u64 + FRAME_TRAILER_LEN as u64 > file_len {
            break; // clean EOF or torn header
        }
        file.seek(SeekFrom::Start(pos))?;
        let mut hdr = [0u8; FRAME_HEADER_LEN];
        file.read_exact(&mut hdr)?;
        let len = u32::from_le_bytes(hdr[..4].try_into().unwrap()) as u64;
        let kind = hdr[4];
        if len > file_len || pos + FRAME_HEADER_LEN as u64 + len + FRAME_TRAILER_LEN as u64 > file_len {
            break; // torn frame
        }
        let mut payload = vec![0u8; len as usize];
        file.read_exact(&mut payload)?;
        let mut crc_buf = [0u8; FRAME_TRAILER_LEN];
        file.read_exact(&mut crc_buf)?;
        let expect = u32::from_le_bytes(crc_buf.try_into().unwrap());
        let mut check = Vec::with_capacity(1 + payload.len());
        check.push(kind);
        check.extend_from_slice(&payload);
        if crc32fast::hash(&check) != expect {
            break; // corrupt or torn tail
        }
        frames.push(Frame { offset: pos, kind, payload });
        pos += FRAME_HEADER_LEN as u64 + len + FRAME_TRAILER_LEN as u64;
    }
    Ok(frames)
}

/// Append records and fsync. Caller must hold the exclusive lock.
pub fn append_records(file: &mut File, records: &[(u8, Vec<u8>)]) -> Result<()> {
    file.seek(SeekFrom::End(0))?;
    for (kind, payload) in records {
        file.write_all(&frame_bytes(*kind, payload))?;
    }
    file.sync_data()?;
    Ok(())
}

/// Open the `.knit` file with a shared lock for reading.
pub fn open_read(path: &Path) -> Result<File> {
    let f = OpenOptions::new().read(true).open(path)?;
    fs2::FileExt::lock_shared(&f)?;
    Ok(f)
}

/// Open the `.knit` file with an exclusive lock for writing.
pub fn open_write(path: &Path) -> Result<File> {
    let f = OpenOptions::new().read(true).append(true).open(path)?;
    fs2::FileExt::lock_exclusive(&f)?;
    Ok(f)
}

pub fn yarnball_path(dir: &Path) -> PathBuf {
    dir.join(".knit")
}
