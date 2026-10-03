// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Reads and writes the file mappings of a prebuilt ext2 image.
//!
//! Each record is `file_offset mem_offset length relpath` followed by a NUL, so the relpath may
//! contain any byte except NUL.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufWriter;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use base::SafeDescriptor;
use base::SharedMemory;

use crate::arena::FileMappingInfo;
use crate::blockgroup::BLOCK_SIZE;

/// A file mapping as stored in a mappings file.
#[derive(Debug, PartialEq, Eq)]
pub struct MappingEntry {
    /// Path of the file relative to the root directory of the file system.
    pub path: PathBuf,
    /// Offset in the file to start the mapping.
    pub file_offset: u64,
    /// Offset in the memory that the file is mapped to.
    pub mem_offset: usize,
    /// The length of the mapping.
    pub length: usize,
}

/// Writes `mappings` to the file at `path`.
pub fn write_mappings(path: &Path, mappings: &[FileMappingInfo]) -> Result<()> {
    let mut w = BufWriter::new(
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?,
    );
    for m in mappings {
        if m.path.as_os_str().is_empty() {
            bail!("empty path cannot be stored in a mappings file");
        }
        write!(w, "{} {} {} ", m.file_offset, m.mem_offset, m.length)?;
        w.write_all(m.path.as_os_str().as_bytes())?;
        w.write_all(b"\0")?;
    }
    w.flush()?;
    Ok(())
}

/// Reads a mappings file written by `write_mappings`.
pub fn read_mappings(path: &Path) -> Result<Vec<MappingEntry>> {
    let content = std::fs::read(path)
        .with_context(|| format!("failed to read mappings file {}", path.display()))?;
    let Some(records) = content.strip_suffix(b"\0") else {
        if content.is_empty() {
            return Ok(Vec::new());
        }
        bail!("{}: last mapping is not NUL-terminated", path.display());
    };
    records
        .split(|b| *b == 0)
        .enumerate()
        .map(|(i, record)| {
            let parse = || -> Result<MappingEntry> {
                let mut f = record.splitn(4, |b| *b == b' ');
                let mut num = || -> Result<&str> {
                    Ok(std::str::from_utf8(f.next().context("missing field")?)?)
                };
                let file_offset = num()?.parse()?;
                let mem_offset = num()?.parse()?;
                let length = num()?.parse()?;
                let p = f.next().context("missing path")?;
                if p.is_empty() {
                    bail!("empty path");
                }
                Ok(MappingEntry {
                    path: PathBuf::from(OsStr::from_bytes(p)),
                    file_offset,
                    mem_offset,
                    length,
                })
            };
            parse().with_context(|| format!("{}: malformed mapping #{}", path.display(), i + 1))
        })
        .collect()
}

/// Opens the files under `root` for the `entries` whose first path component is in `allowed`.
pub fn load_mappings(
    entries: Vec<MappingEntry>,
    root: &Path,
    allowed: &[OsString],
) -> Result<Vec<FileMappingInfo>> {
    let allowed: BTreeSet<&OsStr> = allowed.iter().map(|s| s.as_os_str()).collect();
    let mut out = Vec::new();
    for e in entries {
        let mut components = e.path.components();
        match components.next() {
            Some(Component::Normal(first)) if allowed.contains(first) => {}
            _ => continue,
        }
        if components.any(|c| !matches!(c, Component::Normal(_))) {
            bail!("invalid path {:?} in mappings", e.path);
        }
        let full = root.join(&e.path);
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&full)
            .with_context(|| format!("failed to open {:?}", full))?;
        let size = file.metadata()?.len();
        let end = e
            .file_offset
            .checked_add(e.length as u64)
            .with_context(|| format!("mapping of {:?} overflows", full))?;
        if size < end {
            bail!("{:?} is smaller than its mapping", full);
        }
        out.push(FileMappingInfo {
            mem_offset: e.mem_offset,
            file,
            length: e.length,
            file_offset: e.file_offset as usize,
            path: e.path,
        });
    }
    Ok(out)
}

/// Returns read-only mappings that overlay the root directory of `image` with a compact copy
/// that lists only ".", "..", "lost+found" and the names in `allowed`.
pub fn filter_root_dir(image: &File, allowed: &[OsString]) -> Result<Vec<FileMappingInfo>> {
    let bs = BLOCK_SIZE as u64;
    let read_u32 = |off: u64| -> Result<u32> {
        let mut b = [0u8; 4];
        image.read_exact_at(&mut b, off)?;
        Ok(u32::from_le_bytes(b))
    };
    // The root is inode 2, the second record of group 0's inode table.
    let inode_size = read_u32(1024 + 88)? as u16 as usize;
    let table = read_u32(bs + 8)? as u64 * bs;
    let inode = table + inode_size as u64;
    let num_blocks = read_u32(inode + 4)? as u64 / bs;
    let allowed: BTreeSet<&[u8]> = allowed
        .iter()
        .map(|s| s.as_bytes())
        .chain([&b"."[..], b"..", b"lost+found"])
        .collect();
    let mut blocks = Vec::new();
    let mut entries = Vec::new();
    for i in 0..num_blocks {
        let block = if i < 12 {
            read_u32(inode + 40 + 4 * i)?
        } else {
            read_u32(read_u32(inode + 40 + 48)? as u64 * bs + 4 * (i - 12))?
        };
        blocks.push(block);
        let mut buf = vec![0u8; BLOCK_SIZE];
        image.read_exact_at(&mut buf, block as u64 * bs)?;
        let mut off = 0;
        while off + 8 <= BLOCK_SIZE {
            let rec_len = u16::from_le_bytes([buf[off + 4], buf[off + 5]]) as usize;
            let name_len = 8 + buf[off + 6] as usize;
            if buf[off..off + 4] != [0; 4] && allowed.contains(&buf[off + 8..off + name_len]) {
                entries.push(buf[off..off + name_len].to_vec());
            }
            if rec_len < 8 {
                break;
            }
            off += rec_len;
        }
    }
    let mut data = pack_dir_entries(&entries);
    let k = data.len() / BLOCK_SIZE;
    let mut inode_block = vec![0u8; BLOCK_SIZE];
    image.read_exact_at(&mut inode_block, table)?;
    let ino = &mut inode_block[inode_size..];
    ino[4..8].copy_from_slice(&((k * BLOCK_SIZE) as u32).to_le_bytes());
    // debugfs and e2fsck ignore i_size, so also drop the block pointers past the compact blocks.
    let indirect = k > 12;
    ino[40 + 4 * k.min(12)..40 + 4 * (12 + !indirect as usize)].fill(0);
    ino[28..32].copy_from_slice(&(((k + indirect as usize) * 8) as u32).to_le_bytes());
    data.extend_from_slice(&inode_block);
    let mut offsets: Vec<u64> = blocks[..k].iter().map(|b| *b as u64 * bs).collect();
    offsets.push(table);
    if indirect {
        let table = read_u32(inode + 40 + 48)? as u64 * bs;
        let mut buf = vec![0u8; BLOCK_SIZE];
        image.read_exact_at(&mut buf[..(k - 12) * 4], table)?;
        data.extend_from_slice(&buf);
        offsets.push(table);
    }

    let memfd = File::from(SafeDescriptor::from(SharedMemory::new(
        "pmem_ext2_root",
        data.len() as u64,
    )?));
    memfd.write_all_at(&data, 0)?;
    offsets
        .into_iter()
        .enumerate()
        .map(|(i, mem_offset)| {
            Ok(FileMappingInfo {
                mem_offset: mem_offset as usize,
                file: memfd.try_clone()?,
                length: BLOCK_SIZE,
                file_offset: i * BLOCK_SIZE,
                path: PathBuf::new(),
            })
        })
        .collect()
}

/// Packs directory entries (header and name, without padding) densely into directory blocks.
fn pack_dir_entries(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut last = 0;
    let finish_block = |out: &mut Vec<u8>, last: usize| {
        let end = out.len().next_multiple_of(BLOCK_SIZE);
        out[last + 4..last + 6].copy_from_slice(&((end - last) as u16).to_le_bytes());
        out.resize(end, 0);
    };
    for e in entries {
        let rec_len = e.len().next_multiple_of(4);
        if !out.is_empty() && out.len() % BLOCK_SIZE + rec_len > BLOCK_SIZE {
            finish_block(&mut out, last);
        }
        last = out.len();
        out.extend_from_slice(e);
        out.resize(last + rec_len, 0);
        out[last + 4..last + 6].copy_from_slice(&(rec_len as u16).to_le_bytes());
    }
    finish_block(&mut out, last);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name_len: usize) -> Vec<u8> {
        let mut e = vec![1, 0, 0, 0, 0, 0, name_len as u8, 2];
        e.resize(8 + name_len, b'n');
        e
    }

    #[test]
    fn pack_dir_entries_fills_blocks() {
        // 255-byte names take 264 bytes each: 15 fit in a block (3960), the 16th starts the next.
        let packed = pack_dir_entries(&vec![entry(255); 20]);
        assert_eq!(packed.len(), 2 * BLOCK_SIZE);
        let mut offsets = Vec::new();
        let mut off = 0;
        while off < packed.len() {
            let rec_len = u16::from_le_bytes([packed[off + 4], packed[off + 5]]) as usize;
            assert_eq!(off / BLOCK_SIZE, (off + rec_len - 1) / BLOCK_SIZE);
            offsets.push((off, rec_len));
            off += rec_len;
        }
        assert_eq!(offsets.len(), 20);
        assert_eq!(offsets[0], (0, 264));
        assert_eq!(offsets[14], (14 * 264, BLOCK_SIZE - 14 * 264));
        assert_eq!(offsets[15], (BLOCK_SIZE, 264));
        assert_eq!(offsets[19], (BLOCK_SIZE + 4 * 264, BLOCK_SIZE - 4 * 264));
        assert_eq!(pack_dir_entries(&[entry(1)]).len(), BLOCK_SIZE);
    }
}
