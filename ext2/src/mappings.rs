// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Reads and writes the file mappings of a prebuilt ext2 image.
//!
//! Each record is `file_offset mem_offset length relpath` followed by a NUL, so the relpath may
//! contain any byte except NUL.

use std::ffi::OsStr;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufWriter;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;

use crate::arena::FileMappingInfo;

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

/// Opens the files under `root` for the `entries`.
pub fn load_mappings(entries: Vec<MappingEntry>, root: &Path) -> Result<Vec<FileMappingInfo>> {
    let mut out = Vec::new();
    for e in entries {
        if e.path.components().any(|c| !matches!(c, Component::Normal(_))) {
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
