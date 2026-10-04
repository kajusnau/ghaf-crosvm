// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Provides a function to lanunches a process of creating ext2 filesystem on memory region
//! asynchronously for pmem-ext2 device.
//!
//! The ext2 file system is created in the memory area for pmem by the following three processes:
//! (a). The main process
//! (b). ext2 process launched by the `launch()` below.
//! (c). The virtio-pmem process
//!
//! By executing mkfs in the multiple processes, mkfs won't block other initalization steps. Also,
//! we can use different seccopm poliy for (b) and (c).
//!
//! The overall workflow is like the followings:
//! 1. At (a): `launch()` is called from (a)
//! 2. At (a): (b) is foked from (a) in `launch()`
//! 3. At (b): The given directory is traversed and metadata is constructed.
//! 4. At (b): File descriptors are sent to (a) with `VmMemoryRequest::MmapAndRegisterMemory`.
//! 5. At (a): mmap() for the file descriptors are called. The reply is sent to (b).
//! 6. At (b): memory slot number is sent to (c).
//! 7. At (c): device activation finished.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use base::error;
use base::linux::SharedMemoryLinux;
use base::warn;
use base::AsRawDescriptor;
use base::MappedRegion;
use base::MemoryMappingBuilder;
use base::Pid;
use base::Protection;
use base::SharedMemory;
use base::Tube;
use jail::create_base_minijail;
use jail::create_sandbox_minijail;
use jail::fork_process;
use jail::JailConfig;
use jail::RunAsUser;
use jail::SandboxConfig;
use vm_control::api::VmMemoryClient;
use vm_control::VmMemoryFileMapping;
use vm_memory::GuestAddress;

use crate::crosvm::sys::config::PmemExt2Option;

/// Builds an image of `src` at `image` and writes its file mappings to `mappings`.
pub fn make_image(src: &PmemExt2Option, image: &Path, mappings: &Path) -> Result<()> {
    let mut builder = ext2::Builder {
        inodes_per_group: src.inodes_per_group,
        blocks_per_group: src.blocks_per_group,
        root_dir: Some(src.path.clone()),
        ..Default::default()
    };
    let paths = src.paths.as_deref().context("`paths` is required")?;
    builder.paths = Some(ext2::read_paths_file(paths)?);
    builder.set_auto_size(&src.path)?;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(image)
        .with_context(|| format!("failed to create {}", image.display()))?;
    file.set_len(builder.size)
        .context("failed to size the image")?;
    let shm = SharedMemory::from_file(file).context("failed to create shared memory from file")?;
    let mapping_info = builder
        .build_on_shm(&shm)
        .context("failed to build memory region")?
        .build_mmap_info()
        .context("failed to build ext2")?
        .mapping_info;
    ext2::write_mappings(mappings, &mapping_info)
}

/// Starts a process to create an ext2 filesystem on a given shared memory region.
pub fn launch(
    mapping_address: GuestAddress,
    vm_memory_client: VmMemoryClient,
    device_tube: Tube, // Connects to a virtio device to send a memory slot number.
    path: &Path,
    ugid: &(Option<u32>, Option<u32>),
    ugid_map: (&str, &str),
    mut builder: ext2::Builder,
    backing_dir: Option<&Path>,
    prebuilt: Option<(&Path, &Path)>, // (image, mappings) from `crosvm make_pmem_ext2`
    jail_config: Option<&JailConfig>,
) -> Result<Pid> {
    let max_open_files = base::linux::max_open_files()
        .context("failed to get max number of open files")?
        .rlim_max;

    let jail = if let Some(jail_config) = jail_config {
        let mut config = SandboxConfig::new(jail_config, "virtual_ext2");
        config.limit_caps = false;
        config.ugid_map = Some(ugid_map);
        // We want bind mounts from the parent namespaces to propagate into the mkfs's
        // namespace.
        config.remount_mode = Some(libc::MS_SLAVE);
        config.run_as = match *ugid {
            (None, None) => RunAsUser::Unspecified,
            (uid_opt, gid_opt) => RunAsUser::Specified(uid_opt.unwrap_or(0), gid_opt.unwrap_or(0)),
        };
        create_sandbox_minijail(path, max_open_files, &config)?
    } else {
        create_base_minijail(path, max_open_files)?
    };

    // Use "/" in the new mount namespace as the root for mkfs.
    builder.root_dir = Some(std::path::PathBuf::from("/"));

    let mut entries = None;
    let shm = match (prebuilt, backing_dir) {
        (Some((image, m)), _) => {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC)
                .open(image)
                .with_context(|| format!("failed to open {}", image.display()))?;
            entries = Some(ext2::read_mappings(m)?);
            SharedMemory::from_file(file).context("failed to create shared memory from file")?
        }
        (None, Some(dir)) => {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_TMPFILE | libc::O_CLOEXEC)
                .open(dir)
                .with_context(|| format!("failed to create backing file in {}", dir.display()))?;
            file.set_len(builder.size as u64)
                .context("failed to size backing file")?;
            SharedMemory::from_file(file).context("failed to create shared memory from file")?
        }
        (None, None) => SharedMemory::new("pmem_ext2_shm", builder.size as u64)
            .context("failed to create shared memory")?,
    };
    let mut keep_rds = vec![
        shm.as_raw_descriptor(),
        vm_memory_client.as_raw_descriptor(),
        device_tube.as_raw_descriptor(),
    ];
    base::syslog::push_descriptors(&mut keep_rds);

    let child_process = fork_process(jail, keep_rds, Some(String::from("mkfs process")), || {
        if let Err(e) = mkfs_callback(
            vm_memory_client,
            mapping_address,
            device_tube,
            builder,
            shm,
            entries,
        ) {
            error!("failed to create file system: {:#}", e);
            // SAFETY: exit() is trivially safe.
            unsafe { libc::exit(1) };
        }
    })
    .context("failed to fork a process for mkfs")?;
    Ok(child_process.pid)
}

/// A callback to create a ext2 file system on `shm`.
/// This is supposed to be run in a jailed child process so operations are sandboxed and limited.
fn mkfs_callback(
    mem_client: VmMemoryClient,
    mapping_address: GuestAddress,
    device_tube: Tube, // Connects to a virtio device to send a memory slot number.
    builder: ext2::Builder,
    shm: SharedMemory,
    entries: Option<Vec<ext2::MappingEntry>>,
) -> Result<()> {
    let file_mappings = match entries {
        Some(entries) => {
            if let Err(e) = page_out_image(&shm) {
                warn!("{:#}", e);
            }
            ext2::load_mappings(
                entries,
                builder.root_dir.as_deref().context("no root directory")?,
            )
            .context("failed to load file mappings")?
        }
        None => {
            let region = builder
                .build_on_shm(&shm)
                .context("failed to build memory region")?
                .build_mmap_info()
                .context("failed to build ext2")?;
            // Only this process maps most of the image, so only it can page it out.
            if let Err(e) = region.page_out() {
                warn!("{:#}", e);
            }
            region.mapping_info
        }
    };

    let file_mapping_info: Vec<_> = file_mappings
        .into_iter()
        .map(|info| VmMemoryFileMapping {
            file: info.file,
            length: info.length,
            mem_offset: info.mem_offset,
            file_offset: info.file_offset as u64,
        })
        .collect();

    let slot = mem_client
        .mmap_and_register_memory(mapping_address, shm, file_mapping_info)
        .context("failed to request mmaping and registering memory")?;
    device_tube
        .send(&slot)
        .context("failed to send VmMemoryRequest::RegisterMemory")?;
    Ok(())
}

/// Pages out the allocated pages of a prebuilt image. MADV_PAGEOUT only reaches mapped pages, and
/// faulting in a hole would allocate it, so only SEEK_DATA ranges are touched (mincore() reports
/// every page resident for a file the caller cannot write).
fn page_out_image(shm: &SharedMemory) -> Result<()> {
    let mem = MemoryMappingBuilder::new(shm.size() as usize)
        .from_shared_memory(shm)
        .protection(Protection::read())
        .build()
        .context("failed to map the image")?;
    let fd = shm.as_raw_descriptor();
    let page = base::pagesize() as i64;
    let mut off = 0;
    while off < mem.size() as i64 {
        // SAFETY: lseek on a valid descriptor; ENXIO past the last data range ends the loop.
        let start = unsafe { libc::lseek64(fd, off, libc::SEEK_DATA) };
        if start < 0 {
            break;
        }
        // SAFETY: as above.
        let end = unsafe { libc::lseek64(fd, start, libc::SEEK_HOLE) };
        if end < 0 {
            bail!("lseek failed: {}", std::io::Error::last_os_error());
        }
        for p in (start / page * page..end.min(mem.size() as i64)).step_by(page as usize) {
            // SAFETY: the offset is within the mapping.
            unsafe { std::ptr::read_volatile(mem.as_ptr().add(p as usize)) };
        }
        off = end;
    }
    <dyn MappedRegion>::madvise(&mem, 0, mem.size(), libc::MADV_PAGEOUT)
        .context("failed to madvise(MADV_PAGEOUT)")
}
