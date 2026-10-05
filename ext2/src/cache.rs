// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Saves and restores a built ext2 image with its file mappings.
//!
//! A cache file is a zstd stream of a header with the builder options and the sorted `paths`
//! allowlist, the file mappings
//! (`mem_offset length file_offset file_size path_len path`), and the non-zero 4K blocks of the
//! image as `offset len data` records ended by a record with `len == 0`. All integers are
//! little-endian.

use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use base::MappedRegion;
use base::MemoryMapping;

use crate::arena::FileMappingInfo;
use crate::Builder;
use crate::BLOCK_SIZE;

const MAGIC: &[u8; 8] = b"PMEXT2C\x02";

/// A file mapping as stored in a cache file.
#[derive(Debug, PartialEq, Eq)]
pub struct CachedMapping {
    /// Offset in the memory that the file is mapped to.
    pub mem_offset: u64,
    /// The length of the mapping.
    pub length: u64,
    /// Offset in the file to start the mapping.
    pub file_offset: u64,
    /// Size of the file when the image was built.
    pub file_size: u64,
    /// Path of the file relative to the root directory of the file system.
    pub path: PathBuf,
}

impl From<&FileMappingInfo> for CachedMapping {
    fn from(m: &FileMappingInfo) -> Self {
        Self {
            mem_offset: m.mem_offset as u64,
            length: m.length as u64,
            file_offset: m.file_offset as u64,
            file_size: m.file_size,
            path: m.path.clone(),
        }
    }
}

fn header(cfg: &Builder) -> Vec<u8> {
    let mut h = MAGIC.to_vec();
    h.extend(cfg.size.to_le_bytes());
    h.extend(cfg.blocks_per_group.to_le_bytes());
    h.extend(cfg.inodes_per_group.to_le_bytes());
    let mut paths: Vec<&[u8]> = cfg.paths.iter().flatten().map(|p| p.as_bytes()).collect();
    paths.sort();
    h.extend((paths.len() as u64).to_le_bytes());
    for p in paths {
        h.extend((p.len() as u32).to_le_bytes());
        h.extend(p);
    }
    h
}

/// Writes the image in `mem` and its `mappings` to `w`. Only `data` ranges of `mem` are read.
pub fn write_cache(
    w: impl Write,
    cfg: &Builder,
    mem: &MemoryMapping,
    data: &[Range<usize>],
    mappings: &[CachedMapping],
) -> Result<()> {
    let mut z = zstd::Encoder::new(w, 3)?;
    z.include_checksum(true)?;
    z.write_all(&header(cfg))?;
    z.write_all(&(mappings.len() as u64).to_le_bytes())?;
    for m in mappings {
        let path = m.path.as_os_str().as_bytes();
        for v in [m.mem_offset, m.length, m.file_offset, m.file_size] {
            z.write_all(&v.to_le_bytes())?;
        }
        z.write_all(&(path.len() as u32).to_le_bytes())?;
        z.write_all(path)?;
    }
    let mut buf = vec![0u8; BLOCK_SIZE];
    for r in data {
        let mut run: Option<usize> = None;
        let mut blocks = Vec::new();
        for off in (r.start / BLOCK_SIZE * BLOCK_SIZE..r.end).step_by(BLOCK_SIZE) {
            let n = mem.read_slice(&mut buf, off)?;
            if buf[..n].iter().all(|b| *b == 0) {
                if let Some(start) = run.take() {
                    write_region(&mut z, start, &blocks)?;
                    blocks.clear();
                }
                continue;
            }
            run.get_or_insert(off);
            blocks.extend_from_slice(&buf[..n]);
        }
        if let Some(start) = run {
            write_region(&mut z, start, &blocks)?;
        }
    }
    z.write_all(&[0u8; 16])?;
    z.finish()?.flush()?;
    Ok(())
}

fn write_region(w: &mut impl Write, offset: usize, data: &[u8]) -> Result<()> {
    w.write_all(&(offset as u64).to_le_bytes())?;
    w.write_all(&(data.len() as u64).to_le_bytes())?;
    w.write_all(data)?;
    Ok(())
}

fn read_u64(r: &mut impl Read) -> Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// Returns the image size stored in the cache file at `path` if it was made with `cfg`'s options.
pub fn cached_size(path: &Path, cfg: &Builder) -> Result<u64> {
    let mut h = header(cfg);
    zstd::Decoder::new(File::open(path)?)?.read_exact(&mut h)?;
    let size = u64::from_le_bytes(h[8..16].try_into()?);
    if h != header(&Builder {
        size,
        ..cfg.clone()
    }) {
        bail!("the cache was made for other builder options");
    }
    Ok(size)
}

/// Restores an image written by `write_cache` for `cfg` into the zeroed `mem` and returns its
/// mappings.
pub fn read_cache(r: impl Read, cfg: &Builder, mem: &MemoryMapping) -> Result<Vec<CachedMapping>> {
    let mut z = zstd::Decoder::new(r)?;
    let expected = header(cfg);
    let mut h = vec![0u8; expected.len()];
    z.read_exact(&mut h).context("failed to read the header")?;
    if h != expected {
        bail!("the cache was made for other builder options");
    }
    let size = mem.size() as u64;
    let mut mappings = Vec::new();
    for _ in 0..read_u64(&mut z)? {
        let mem_offset = read_u64(&mut z)?;
        let length = read_u64(&mut z)?;
        let file_offset = read_u64(&mut z)?;
        let file_size = read_u64(&mut z)?;
        if mem_offset.checked_add(length).is_none_or(|end| end > size) {
            bail!("a mapping is out of the image");
        }
        let mut len = [0u8; 4];
        z.read_exact(&mut len)?;
        let len = u32::from_le_bytes(len) as usize;
        if len == 0 || len > libc::PATH_MAX as usize {
            bail!("invalid path length {}", len);
        }
        let mut path = vec![0u8; len];
        z.read_exact(&mut path)?;
        mappings.push(CachedMapping {
            mem_offset,
            length,
            file_offset,
            file_size,
            path: PathBuf::from(OsStr::from_bytes(&path)),
        });
    }
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let offset = read_u64(&mut z)?;
        let len = read_u64(&mut z)?;
        if len == 0 {
            break;
        }
        if offset.checked_add(len).is_none_or(|end| end > size) {
            bail!("a data region is out of the image");
        }
        let mut done = 0;
        while done < len {
            let n = (len - done).min(buf.len() as u64) as usize;
            z.read_exact(&mut buf[..n])?;
            mem.write_slice(&buf[..n], (offset + done) as usize)?;
            done += n as u64;
        }
    }
    // Reading to the end makes the decoder verify the checksum.
    if z.read(&mut [0u8; 1])? != 0 {
        bail!("trailing data after the last region");
    }
    Ok(mappings)
}
