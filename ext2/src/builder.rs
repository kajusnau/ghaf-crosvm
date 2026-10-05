// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Provides structs and logic to build ext2 file system with configurations.

use std::collections::HashSet;
use std::ffi::CString;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::ops::Range;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use base::MappedRegion;
use base::MemoryMapping;
use base::MemoryMappingArena;
use base::MemoryMappingBuilder;
use base::Protection;
use base::SharedMemory;

use crate::arena::Arena;
use crate::arena::FileMappingInfo;
use crate::cache;
use crate::cache::CachedMapping;
use crate::fs::Ext2;
use crate::BLOCK_SIZE;

/// A struct to represent the configuration of an ext2 filesystem.
#[derive(Clone)]
pub struct Builder {
    /// The number of blocks per group.
    pub blocks_per_group: u32,
    /// The number of inodes per group.
    pub inodes_per_group: u32,
    /// The size of the memory region.
    pub size: u64,
    /// The roof directory to be copied to the file system.
    pub root_dir: Option<PathBuf>,
    /// If set, only these top-level entries of `root_dir` are copied.
    pub paths: Option<Vec<OsString>>,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            blocks_per_group: 4096,
            inodes_per_group: 4096,
            size: 4096 * 4096,
            root_dir: None,
            paths: None,
        }
    }
}

impl Builder {
    /// Validates field values and adjusts them if needed.
    fn validate(&mut self) -> Result<()> {
        let block_group_size = BLOCK_SIZE as u64 * self.blocks_per_group as u64;
        if self.size < block_group_size {
            bail!(
            "memory size {} is too small to have a block group: block_size={},  block_per_group={}",
            self.size,
            BLOCK_SIZE,
            block_group_size
        );
        }
        if self.size % block_group_size != 0 {
            // Round down to the largest multiple of block_group_size that is smaller than self.size
            self.size = self.size.next_multiple_of(block_group_size) - block_group_size
        };
        Ok(())
    }

    /// Sets `size` to the smallest size that fits `src_dir` filtered by `paths`.
    pub fn set_auto_size(&mut self, src_dir: &Path) -> Result<()> {
        self.size = crate::fs::auto_size(
            src_dir,
            self.paths.as_deref(),
            self.blocks_per_group,
            self.inodes_per_group,
        )
        .context("failed to compute the ext2 size")?;
        Ok(())
    }

    /// Allocates memory region with the given configuration.
    pub fn allocate_memory(mut self) -> Result<MemRegion> {
        self.validate()
            .context("failed to validate the ext2 config")?;
        let mem = MemoryMappingBuilder::new(self.size as usize)
            .build()
            .context("failed to allocate memory for ext2")?;
        Ok(MemRegion { cfg: self, mem })
    }

    /// Builds memory region on the given shared memory.
    pub fn build_on_shm(self, shm: &SharedMemory) -> Result<MemRegion> {
        let mem = MemoryMappingBuilder::new(shm.size() as usize)
            .from_shared_memory(shm)
            .build()
            .expect("failed to build MemoryMapping from shared memory");
        Ok(MemRegion { cfg: self, mem })
    }
}

/// Reads a file of absolute paths, one per line, and returns their file names.
pub fn read_paths_file(path: &Path) -> Result<Vec<OsString>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read paths file {:?}", path))?;
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            Path::new(l.trim())
                .file_name()
                .map(OsString::from)
                .with_context(|| format!("invalid path {:?} in {:?}", l, path))
        })
        .collect()
}

/// Memory region for ext2 with its config.
pub struct MemRegion {
    cfg: Builder,
    mem: MemoryMapping,
}

impl MemRegion {
    /// Constructs an ext2 metadata by traversing `src_dir`.
    pub fn build_mmap_info(mut self) -> Result<MemRegionWithMappingInfo> {
        let arena = Arena::new(BLOCK_SIZE, &mut self.mem).context("failed to allocate arena")?;
        let mut ext2 = Ext2::new(&self.cfg, &arena).context("failed to create Ext2 struct")?;
        if let Some(dir) = &self.cfg.root_dir {
            ext2.copy_dirtree(&arena, dir, self.cfg.paths.as_deref())
                .context("failed to copy directory tree")?;
        }
        ext2.copy_backup_metadata(&arena)
            .context("failed to copy metadata for backup")?;
        let mut mapping_info = arena.into_mapping_info();
        if let Some(dir) = &self.cfg.root_dir {
            for info in &mut mapping_info {
                info.path = info.path.strip_prefix(dir)?.to_path_buf();
            }
        }

        self.mem
            .msync()
            .context("failed to msyn of ext2's memory region")?;
        Ok(MemRegionWithMappingInfo {
            mem: self.mem,
            mapping_info,
        })
    }

    /// Restores the image from a cache written by `MemRegionWithMappingInfo::write_cache` and
    /// opens its files under `root_dir`. On failure the region is zeroed again.
    pub fn restore(self, r: impl Read) -> Result<MemRegionWithMappingInfo> {
        let res = self.restore_mappings(r);
        if res.is_err() {
            let _ = <dyn MappedRegion>::madvise(&self.mem, 0, self.mem.size(), libc::MADV_REMOVE);
        }
        Ok(MemRegionWithMappingInfo {
            mem: self.mem,
            mapping_info: res?,
        })
    }

    fn restore_mappings(&self, r: impl Read) -> Result<Vec<FileMappingInfo>> {
        let root = self.cfg.root_dir.as_deref().context("no root directory")?;
        let root = File::open(root).with_context(|| format!("failed to open {:?}", root))?;
        let paths: Option<HashSet<&OsStr>> = self
            .cfg
            .paths
            .as_ref()
            .map(|p| p.iter().map(|p| p.as_os_str()).collect());
        let mut out = Vec::new();
        let mut last: Option<(PathBuf, File)> = None;
        for m in cache::read_cache(r, &self.cfg, &self.mem)? {
            let mut c = m.path.components();
            match c.next() {
                Some(Component::Normal(top)) if paths.as_ref().is_none_or(|p| p.contains(top)) => {}
                _ => bail!("{:?} is not in the paths", m.path),
            }
            if !c.all(|c| matches!(c, Component::Normal(_))) {
                bail!("invalid path {:?}", m.path);
            }
            if m.file_offset
                .checked_add(m.length)
                .is_none_or(|end| end > m.file_size)
            {
                bail!("mapping of {:?} is past its end", m.path);
            }
            let file = match &last {
                Some((p, f)) if *p == m.path => f.try_clone()?,
                _ => {
                    let f = open_beneath(&root, &m.path)
                        .with_context(|| format!("failed to open {:?}", m.path))?;
                    if f.metadata()?.len() != m.file_size {
                        bail!("{:?} changed size", m.path);
                    }
                    last = Some((m.path.clone(), f.try_clone()?));
                    f
                }
            };
            out.push(FileMappingInfo {
                mem_offset: m.mem_offset as usize,
                length: m.length as usize,
                file,
                file_offset: m.file_offset as usize,
                path: m.path,
                file_size: m.file_size,
            });
        }
        Ok(out)
    }
}

/// Opens `path` under `dir` one component at a time without following any symlink, so a cached
/// path cannot leave `dir`. openat2(RESOLVE_NO_SYMLINKS) would do this in one call, but systemd's
/// RestrictSUIDSGID= makes openat2 fail with ENOSYS.
fn open_beneath(dir: &File, path: &Path) -> Result<File> {
    let mut cur = dir.try_clone()?;
    let mut components = path.components().peekable();
    while let Some(c) = components.next() {
        let Component::Normal(name) = c else {
            bail!("invalid path {:?}", path);
        };
        let name = CString::new(name.as_bytes())?;
        let mut flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        if components.peek().is_some() {
            flags |= libc::O_DIRECTORY;
        }
        // SAFETY: `cur` is an open descriptor and `name` a valid C string.
        let fd = unsafe { libc::openat(cur.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: openat returned a new descriptor that nothing else owns.
        cur = unsafe { File::from_raw_fd(fd) };
    }
    Ok(cur)
}

/// Memory regions where ext2 metadata were written with information of mmap operations to be done.
pub struct MemRegionWithMappingInfo {
    mem: MemoryMapping,
    pub mapping_info: Vec<FileMappingInfo>,
}

impl MemRegionWithMappingInfo {
    /// Asks the kernel to reclaim the written metadata pages, e.g. into zswap.
    pub fn page_out(&self) -> Result<()> {
        <dyn MappedRegion>::madvise(&self.mem, 0, self.mem.size(), libc::MADV_PAGEOUT)
            .context("failed to madvise(MADV_PAGEOUT)")
    }

    /// Writes the image and `mappings` to `w` for `MemRegion::restore`. Only the `data` ranges of
    /// the image are read.
    pub fn write_cache(
        &self,
        w: impl Write,
        cfg: &Builder,
        data: &[Range<usize>],
        mappings: &[CachedMapping],
    ) -> Result<()> {
        cache::write_cache(w, cfg, &self.mem, data, mappings)
    }

    /// Do mmap and returns the memory region where ext2 was created.
    pub fn do_mmap(self) -> Result<MemoryMappingArena> {
        let mut mmap_arena = MemoryMappingArena::from(self.mem);
        for FileMappingInfo {
            mem_offset,
            file,
            length,
            file_offset,
            ..
        } in self.mapping_info
        {
            mmap_arena
                .add_fd_mapping(
                    mem_offset,
                    length,
                    &file,
                    file_offset as u64, /* fd_offset */
                    Protection::read(),
                )
                .context("failed mmaping an fd for ext2")?;
        }

        Ok(mmap_arena)
    }
}
