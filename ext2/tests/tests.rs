// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::fs::create_dir;
use std::fs::read_link;
use std::fs::symlink_metadata;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufWriter;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use base::test_utils::call_test_with_sudo;
use base::MappedRegion;
use ext2::Builder;
use tempfile::tempdir;
use tempfile::tempdir_in;
use tempfile::TempDir;
use walkdir::WalkDir;

const FSCK_PATH: &str = "/usr/sbin/e2fsck";
const DEBUGFS_PATH: &str = "/usr/sbin/debugfs";

const BLOCK_SIZE: u32 = 4096;

fn run_fsck(path: &PathBuf) {
    // Run fsck and scheck its exit code is 0.
    // Passing 'y' to stop attempting interactive repair.
    let output = Command::new(FSCK_PATH)
        .arg("-fvy")
        .arg(path)
        .output()
        .unwrap();
    println!("status: {}", output.status);
    println!("stdout: {}", String::from_utf8_lossy(&output.stdout));
    println!("stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success());
}

fn run_debugfs_cmd(args: &[&str], disk: &PathBuf) -> String {
    let output = Command::new(DEBUGFS_PATH)
        .arg("-R")
        .args(args)
        .arg(disk)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("status: {}", output.status);
    println!("stdout: {stdout}");
    println!("stderr: {stderr}");
    assert!(output.status.success());

    stdout.trim_start().trim_end().to_string()
}

fn mkfs(td: &TempDir, builder: Builder) -> PathBuf {
    let path = td.path().join("empty.ext2");
    let mem = builder
        .allocate_memory()
        .unwrap()
        .build_mmap_info()
        .unwrap()
        .do_mmap()
        .unwrap();
    // SAFETY: `mem` has a valid pointer and its size.
    let buf = unsafe { std::slice::from_raw_parts(mem.as_ptr(), mem.size()) };
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    file.write_all(buf).unwrap();

    run_fsck(&path);

    path
}

#[test]
fn test_mkfs_empty() {
    let td = tempdir().unwrap();
    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group: 1024,
            inodes_per_group: 1024,
            ..Default::default()
        },
    );

    // Ensure the content of the generated disk image with `debugfs`.
    // It contains the following entries:
    // - `.`: the rootdir whose inode is 2 and rec_len is 12.
    // - `..`: this is also the rootdir with same inode and the same rec_len.
    // - `lost+found`: inode is 11 and rec_len is 4072 (= block_size - 2*12).
    assert_eq!(
        run_debugfs_cmd(&["ls"], &disk),
        "2  (12) .    2  (12) ..    11  (4072) lost+found"
    );
}

#[test]
fn test_mkfs_empty_multi_block_groups() {
    let td = tempdir().unwrap();
    let blocks_per_group = 2048;
    let num_groups = 2;
    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group,
            inodes_per_group: 4096,
            size: (4096 * blocks_per_group * num_groups).into(),
            ..Default::default()
        },
    );
    assert_eq!(
        run_debugfs_cmd(&["ls"], &disk),
        "2  (12) .    2  (12) ..    11  (4072) lost+found"
    );
}

fn collect_paths(dir: &Path, skip_lost_found: bool) -> BTreeSet<(String, PathBuf)> {
    WalkDir::new(dir)
        .into_iter()
        .filter_map(|entry| {
            entry.ok().and_then(|e| {
                let name = e
                    .path()
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let path = e.path().to_path_buf();
                if name.is_empty() {
                    return None;
                }
                if skip_lost_found && name == "lost+found" {
                    return None;
                }

                Some((name, path))
            })
        })
        .collect()
}

fn assert_eq_dirs(
    td: &TempDir,
    dir: &Path,
    disk: &PathBuf,
    // Check the correct xattr is set and any unexpected one isn't set.
    // Pass None to skip this check for test cases where many files are created.
    xattr_map: Option<BTreeMap<String, Vec<(&str, &str)>>>,
) {
    // dump the disk contents to `dump_dir`.
    let dump_dir = td.path().join("dump");
    std::fs::create_dir(&dump_dir).unwrap();
    run_debugfs_cmd(
        &[&format!(
            "rdump / {}",
            dump_dir.as_os_str().to_str().unwrap()
        )],
        disk,
    );

    let paths1 = collect_paths(dir, true);
    let paths2 = collect_paths(&dump_dir, true);
    if paths1.len() != paths2.len() {
        panic!(
            "number of entries mismatch: {:?}={:?}, {:?}={:?}",
            dir,
            paths1.len(),
            dump_dir,
            paths2.len()
        );
    }

    for ((name1, path1), (name2, path2)) in paths1.iter().zip(paths2.iter()) {
        assert_eq!(name1, name2);
        let m1 = symlink_metadata(path1).unwrap();
        let m2 = symlink_metadata(path2).unwrap();
        assert_eq!(
            m1.file_type(),
            m2.file_type(),
            "file type mismatch ({name1})"
        );

        if m1.file_type().is_symlink() {
            let dst1 = read_link(path1).unwrap();
            let dst2 = read_link(path2).unwrap();
            assert_eq!(
                dst1, dst2,
                "symlink mismatch ({name1}): {path1:?}->{dst1:?} vs {path2:?}->{dst2:?}"
            );
        } else {
            assert_eq!(m1.len(), m2.len(), "length mismatch ({name1})");
        }

        assert_eq!(
            m1.permissions(),
            m2.permissions(),
            "permissions mismatch ({name1})"
        );

        if m1.file_type().is_file() {
            // Check contents
            let c1 = std::fs::read_to_string(path1).unwrap();
            let c2 = std::fs::read_to_string(path2).unwrap();
            assert_eq!(c1, c2, "content mismatch: ({name1})");
        }

        // Check xattr
        if let Some(mp) = &xattr_map {
            match mp.get(name1) {
                Some(expected_xattrs) if !expected_xattrs.is_empty() => {
                    for (key, value) in expected_xattrs {
                        let s = run_debugfs_cmd(&[&format!("ea_get -V {name1} {key}",)], disk);
                        assert_eq!(&s, value);
                    }
                }
                // If no xattr is specified, any value must not be set.
                _ => {
                    let s = run_debugfs_cmd(&[&format!("ea_list {name1}",)], disk);
                    assert_eq!(s, "");
                }
            }
        }
    }
}

#[test]
fn test_simple_dir() {
    // testdata
    // ├── a.txt
    // ├── b.txt
    // └── dir
    //     └── c.txt
    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");
    create_dir(&dir).unwrap();
    File::create(dir.join("a.txt")).unwrap();
    File::create(dir.join("b.txt")).unwrap();
    create_dir(dir.join("dir")).unwrap();
    File::create(dir.join("dir/c.txt")).unwrap();
    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group: 2048,
            inodes_per_group: 4096,
            root_dir: Some(dir.clone()),
            ..Default::default()
        },
    );

    assert_eq_dirs(&td, &dir, &disk, Some(Default::default()));

    td.close().unwrap(); // make sure that tempdir is properly deleted.
}

#[test]
fn test_nested_dirs() {
    // testdata
    // └── dir1
    //     ├── a.txt
    //     └── dir2
    //         ├── b.txt
    //         └── dir3
    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");
    create_dir(&dir).unwrap();
    let dir1 = &dir.join("dir1");
    create_dir(dir1).unwrap();
    File::create(dir1.join("a.txt")).unwrap();
    let dir2 = dir1.join("dir2");
    create_dir(&dir2).unwrap();
    File::create(dir2.join("b.txt")).unwrap();
    let dir3 = dir2.join("dir3");
    create_dir(dir3).unwrap();
    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group: 2048,
            inodes_per_group: 4096,
            root_dir: Some(dir.clone()),
            ..Default::default()
        },
    );

    assert_eq_dirs(&td, &dir, &disk, Some(Default::default()));
}

#[test]
fn test_file_contents() {
    // testdata
    // ├── hello.txt (content: "Hello!\n")
    // └── big.txt (content: 10KB of data, which doesn't fit in one block)
    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");
    create_dir(&dir).unwrap();
    let mut hello = File::create(dir.join("hello.txt")).unwrap();
    hello.write_all(b"Hello!\n").unwrap();
    let mut big = BufWriter::new(File::create(dir.join("big.txt")).unwrap());
    let data = b"123456789\n";
    for _ in 0..1024 {
        big.write_all(data).unwrap();
    }

    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group: 2048,
            inodes_per_group: 4096,
            root_dir: Some(dir.clone()),
            ..Default::default()
        },
    );

    assert_eq_dirs(&td, &dir, &disk, Some(Default::default()));
}

#[test]
fn test_max_file_name() {
    // testdata
    // └── aa..aa (whose file name length is 255, which is the ext2/3/4's maximum file name length)
    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");
    create_dir(&dir).unwrap();
    let long_name = "a".repeat(255);
    File::create(dir.join(long_name)).unwrap();

    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group: 2048,
            inodes_per_group: 4096,
            root_dir: Some(dir.clone()),
            ..Default::default()
        },
    );

    assert_eq_dirs(&td, &dir, &disk, Some(Default::default()));
}

#[test]
fn test_mkfs_indirect_block() {
    // testdata
    // ├── big.txt (80KiB), which requires indirect blocks
    // └── huge.txt (8MiB), which requires doubly indirect blocks
    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");
    std::fs::create_dir(&dir).unwrap();
    let mut big = std::fs::File::create(dir.join("big.txt")).unwrap();
    big.seek(SeekFrom::Start(80 * 1024)).unwrap();
    big.write_all(&[0]).unwrap();

    let mut huge = std::fs::File::create(dir.join("huge.txt")).unwrap();
    huge.seek(SeekFrom::Start(8 * 1024 * 1024)).unwrap();
    huge.write_all(&[0]).unwrap();

    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group: 4096,
            inodes_per_group: 4096,
            root_dir: Some(dir.clone()),
            ..Default::default()
        },
    );

    assert_eq_dirs(&td, &dir, &disk, Some(Default::default()));
}

#[test]
fn test_mkfs_symlink() {
    // testdata
    // ├── a.txt
    // ├── self -> ./self
    // ├── symlink0 -> ./a.txt
    // ├── symlink1 -> ./symlink0
    // └── dir
    //     └── upper-a -> ../a.txt
    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");
    create_dir(&dir).unwrap();

    let mut f = File::create(dir.join("a.txt")).unwrap();
    f.write_all("Hello".as_bytes()).unwrap();

    symlink("./self", dir.join("self")).unwrap();

    symlink("./a.txt", dir.join("symlink0")).unwrap();
    symlink("./symlink0", dir.join("symlink1")).unwrap();

    create_dir(dir.join("dir")).unwrap();
    symlink("../a.txt", dir.join("dir/upper-a")).unwrap();

    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group: 2048,
            inodes_per_group: 4096,
            root_dir: Some(dir.clone()),
            ..Default::default()
        },
    );

    assert_eq_dirs(&td, &dir, &disk, Some(Default::default()));
}

#[test]
fn test_mkfs_abs_symlink() {
    // testdata
    // ├── a.txt
    // ├── a -> /testdata/a
    // ├── self -> /testdata/self
    // ├── tmp -> /tmp
    // └── abc -> /a/b/c
    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");

    std::fs::create_dir(&dir).unwrap();
    File::create(dir.join("a.txt")).unwrap();
    symlink(dir.join("a.txt"), dir.join("a")).unwrap();
    symlink(dir.join("self"), dir.join("self")).unwrap();
    symlink("/tmp/", dir.join("tmp")).unwrap();
    symlink("/a/b/c", dir.join("abc")).unwrap();

    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group: 2048,
            inodes_per_group: 4096,
            root_dir: Some(dir.clone()),
            ..Default::default()
        },
    );

    assert_eq_dirs(&td, &dir, &disk, Some(Default::default()));
}

#[test]
fn test_mkfs_symlink_to_deleted() {
    // testdata
    // ├── (deleted)
    // └── symlink_to_deleted -> (deleted)
    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");

    std::fs::create_dir(&dir).unwrap();
    File::create(dir.join("deleted")).unwrap();
    symlink("./deleted", dir.join("symlink_to_deleted")).unwrap();
    fs::remove_file(dir.join("deleted")).unwrap();

    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group: 2048,
            inodes_per_group: 4096,
            root_dir: Some(dir.clone()),
            ..Default::default()
        },
    );

    assert_eq_dirs(&td, &dir, &disk, Some(Default::default()));
}

#[test]
fn test_mkfs_long_symlink() {
    // testdata
    // ├── /(long name directory)/a.txt
    // └── symlink -> /(long name directory)/a.txt
    // ├── (60-byte filename)
    // └── symlink60 -> (60-byte filename)

    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");

    create_dir(&dir).unwrap();

    const LONG_DIR_NAME: &str =
        "this_is_a_very_long_directory_name_so_that_name_cannoot_fit_in_60_characters_in_inode";
    assert!(LONG_DIR_NAME.len() > 60);

    let long_dir = dir.join(LONG_DIR_NAME);
    create_dir(&long_dir).unwrap();
    File::create(long_dir.join("a.txt")).unwrap();
    symlink(long_dir.join("a.txt"), dir.join("symlink")).unwrap();

    const SIXTY_CHAR_DIR_NAME: &str =
        "./this_is_just_60_byte_long_so_it_can_work_as_a_corner_case.";
    assert_eq!(SIXTY_CHAR_DIR_NAME.len(), 60);
    File::create(dir.join(SIXTY_CHAR_DIR_NAME)).unwrap();
    symlink(SIXTY_CHAR_DIR_NAME, dir.join("symlink60")).unwrap();

    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group: 2048,
            inodes_per_group: 4096,
            root_dir: Some(dir.clone()),
            ..Default::default()
        },
    );

    assert_eq_dirs(&td, &dir, &disk, Some(Default::default()));
}

#[test]
fn test_ignore_lost_found() {
    // Ignore /lost+found/ directory in source to avoid conflict.
    //
    // testdata
    // ├── lost+found (ignored and recreated as an empty dir)
    // │   └── should_be_ignored.txt
    // └── sub
    //     └── lost+found (not ignored)
    //         └── a.txt

    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");

    create_dir(&dir).unwrap();
    create_dir(dir.join("lost+found")).unwrap();
    File::create(dir.join("lost+found").join("should_be_ignored.txt")).unwrap();
    create_dir(dir.join("sub")).unwrap();
    create_dir(dir.join("sub").join("lost+found")).unwrap();
    File::create(dir.join("sub").join("lost+found").join("a.txt")).unwrap();

    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group: 2048,
            inodes_per_group: 4096,
            root_dir: Some(dir.clone()),
            ..Default::default()
        },
    );

    // dump the disk contents to `dump_dir`.
    let dump_dir = td.path().join("dump");
    std::fs::create_dir(&dump_dir).unwrap();
    run_debugfs_cmd(
        &[&format!(
            "rdump / {}",
            dump_dir.as_os_str().to_str().unwrap()
        )],
        &disk,
    );

    let paths = collect_paths(&dump_dir, false /* skip_lost_found */)
        .into_iter()
        .map(|(path, _)| path)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        paths,
        BTreeSet::from([
            "lost+found".to_string(),
            // 'lost+found/should_be_ignored.txt' must not in the result.
            "sub".to_string(),
            "sub/lost+found".to_string(),
            "sub/lost+found/a.txt".to_string()
        ])
    );
}

#[test]
fn test_multiple_block_directory_entry() {
    // Creates a many files in a directory.
    // So the sum of the sizes of directory entries exceeds 4KB and they need to be stored in
    // multiple blocks.
    //
    // testdata
    // ├─  0.txt
    // ├─  1.txt
    // ...
    // └── 999.txt
    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");

    std::fs::create_dir(&dir).unwrap();

    for i in 0..1000 {
        let path = dir.join(format!("{i}.txt"));
        File::create(&path).unwrap();
    }

    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group: 2048,
            inodes_per_group: 4096,
            root_dir: Some(dir.clone()),
            ..Default::default()
        },
    );

    assert_eq_dirs(&td, &dir, &disk, None); // skip xattr check
}

#[test]
fn test_indirect_block_directory_entry() {
    // Directory entries need more than 12 blocks, so the indirect block is used.
    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");

    std::fs::create_dir(&dir).unwrap();

    for i in 0..2000 {
        let path = dir.join(format!("{i:040}"));
        File::create(&path).unwrap();
    }

    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group: 2048,
            inodes_per_group: 4096,
            root_dir: Some(dir.clone()),
            ..Default::default()
        },
    );

    assert_eq_dirs(&td, &dir, &disk, None);
}

// Test a case where the inode tables spans multiple block groups.
#[test]
fn test_multiple_bg_multi_inode_bitmap() {
    // testdata
    // ├─  0.txt
    // ├─  1.txt
    // ...
    // └── 999.txt
    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");

    std::fs::create_dir(&dir).unwrap();

    for i in 0..1000 {
        let fname = format!("{i}.txt");
        let path = dir.join(&fname);
        let mut f = File::create(&path).unwrap();
        // Write a file name to the file.
        f.write_all(fname.as_bytes()).unwrap();
    }

    let blocks_per_group = 1024;
    // Set `inodes_per_group` to a smaller value than the number of files.
    // So, the inode table in the 2nd block group will be used.
    let inodes_per_group = 512;
    let num_groups = 2;
    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group,
            inodes_per_group,
            size: (BLOCK_SIZE * blocks_per_group * num_groups).into(),
            root_dir: Some(dir.clone()),
            paths: None,
        },
    );

    assert_eq_dirs(&td, &dir, &disk, None);
}

/// Test a case where the block tables spans multiple block groups.
#[test]
fn test_multiple_bg_multi_block_bitmap() {
    // testdata
    // ├─  0.txt
    // ├─  1.txt
    // ...
    // └── 999.txt
    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");

    std::fs::create_dir(&dir).unwrap();

    for i in 0..1000 {
        let fname = format!("{i}.txt");
        let path = dir.join(&fname);
        let mut f = File::create(&path).unwrap();
        // Write a file name to the file.
        f.write_all(fname.as_bytes()).unwrap();
    }

    // Set `blocks_per_group` to a smaller value than the number of files.
    // So, the block table in the 2nd block group will be used.
    let blocks_per_group = 512;
    let inodes_per_group = 2048;
    let num_groups = 4;
    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group,
            inodes_per_group,
            size: (BLOCK_SIZE * blocks_per_group * num_groups).into(),
            root_dir: Some(dir.clone()),
            paths: None,
        },
    );

    assert_eq_dirs(&td, &dir, &disk, None);
}

// Test a case where a file spans multiple block groups.
#[test]
fn test_multiple_bg_big_files() {
    // testdata
    // ├─  0.txt (200 * 5000 bytes)
    // ├─  1.txt (200 * 5000 bytes)
    // ...
    // └── 9.txt (200 * 5000 bytes)
    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");

    std::fs::create_dir(&dir).unwrap();

    // Prepare a large data.
    let data = vec!["0123456789"; 5000 * 20].concat();
    for i in 0..10 {
        let path = dir.join(format!("{i}.txt"));
        let mut f = File::create(&path).unwrap();
        f.write_all(data.as_bytes()).unwrap();
    }

    // Set `blocks_per_group` to a value smaller than |size of a file| / 4K.
    // So, each file spans multiple block groups.
    let blocks_per_group = 128;
    let num_groups = 50;
    let disk = mkfs(
        &td,
        Builder {
            blocks_per_group,
            inodes_per_group: 1024,
            size: (BLOCK_SIZE * blocks_per_group * num_groups).into(),
            root_dir: Some(dir.clone()),
            paths: None,
        },
    );

    assert_eq_dirs(&td, &dir, &disk, Some(Default::default()));
}

#[test]
#[ignore = "Called by test_mkfs_xattr"]
fn test_mkfs_xattr_impl() {
    // Since tmpfs doesn't support xattr, use the current directory.
    let td = tempdir_in(".").unwrap();
    let dir = td.path().join("testdata");
    // testdata
    // ├── a.txt ("user.foo"="a", "user.bar"="0123456789")
    // ├── b.txt ("security.selinux"="unconfined_u:object_r:user_home_t:s0")
    // ├── c.txt (no xattr)
    // └── dir/ ("user.foo"="directory")
    //     └─ d.txt ("user.foo"="in_directory")
    std::fs::create_dir(&dir).unwrap();

    let dir_xattrs = vec![("dir".to_string(), vec![("user.foo", "directory")])];
    let file_xattrs = vec![
        (
            "a.txt".to_string(),
            vec![("user.foo", "a"), ("user.number", "0123456789")],
        ),
        (
            "b.txt".to_string(),
            vec![("security.selinux", "unconfined_u:object_r:user_home_t:s0")],
        ),
        ("c.txt".to_string(), vec![]),
        ("dir/d.txt".to_string(), vec![("user.foo", "in_directory")]),
    ];

    // Create dirs
    for (fname, xattrs) in &dir_xattrs {
        let f_path = dir.join(fname);
        std::fs::create_dir(&f_path).unwrap();
        for (key, value) in xattrs {
            ext2::set_xattr(&f_path, key, value).unwrap();
        }
    }
    // Create files
    for (fname, xattrs) in &file_xattrs {
        let f_path = dir.join(fname);
        File::create(&f_path).unwrap();
        for (key, value) in xattrs {
            ext2::set_xattr(&f_path, key, value).unwrap();
        }
    }

    let xattr_map: BTreeMap<String, Vec<(&str, &str)>> =
        file_xattrs.into_iter().chain(dir_xattrs).collect();

    let builder = Builder {
        root_dir: Some(dir.clone()),
        ..Default::default()
    };
    let disk = mkfs(&td, builder);

    assert_eq_dirs(&td, &dir, &disk, Some(xattr_map));
}

#[test]
fn test_mkfs_xattr() {
    call_test_with_sudo("test_mkfs_xattr_impl")
}

#[test]
fn test_mkfs_paths_allowlist() {
    // testdata
    // ├── a.txt
    // ├── b
    // │   └── c.txt
    // └── d.txt  (not listed)
    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");
    create_dir(&dir).unwrap();
    File::create(dir.join("a.txt")).unwrap();
    create_dir(dir.join("b")).unwrap();
    fs::write(dir.join("b/c.txt"), vec![1u8; 100 * BLOCK_SIZE as usize]).unwrap();
    File::create(dir.join("d.txt")).unwrap();
    let list = td.path().join("paths");
    fs::write(&list, "/nix/store/a.txt\n\n/nix/store/b\n").unwrap();

    let mut builder = Builder {
        blocks_per_group: 1024,
        inodes_per_group: 1024,
        root_dir: Some(dir.clone()),
        paths: Some(ext2::read_paths_file(&list).unwrap()),
        ..Default::default()
    };
    builder.set_auto_size(&dir).unwrap();
    assert_eq!(builder.size, 4096 * 1024);
    let disk = mkfs(&td, builder);

    fs::remove_file(dir.join("d.txt")).unwrap();
    assert_eq_dirs(&td, &dir, &disk, None);
}

#[test]
fn test_mkfs_paths_missing() {
    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");
    create_dir(&dir).unwrap();
    File::create(dir.join("a.txt")).unwrap();

    let mut builder = Builder {
        root_dir: Some(dir.clone()),
        paths: Some(vec!["a.txt".into(), "missing".into()]),
        ..Default::default()
    };
    assert!(builder.set_auto_size(&dir).is_err());
}

#[test]
fn test_mkfs_on_file_backed_shm() {
    use base::linux::SharedMemoryLinux;
    use base::SharedMemory;

    let td = tempdir().unwrap();
    let path = td.path().join("backed.ext2");
    let builder = Builder {
        blocks_per_group: 1024,
        inodes_per_group: 1024,
        ..Default::default()
    };
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    file.set_len(builder.size as u64).unwrap();
    let shm = SharedMemory::from_file(file).unwrap();
    let mem = builder
        .build_on_shm(&shm)
        .unwrap()
        .build_mmap_info()
        .unwrap()
        .do_mmap()
        .unwrap();
    drop(mem);

    run_fsck(&path);
    assert_eq!(
        run_debugfs_cmd(&["ls"], &path),
        "2  (12) .    2  (12) ..    11  (4072) lost+found"
    );
}

#[test]
fn test_shared_image_mappings() {
    use base::linux::SharedMemoryLinux;
    use base::SharedMemory;

    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");
    create_dir(&dir).unwrap();
    create_dir(dir.join("a")).unwrap();
    create_dir(dir.join("b")).unwrap();
    fs::write(dir.join("a/f"), vec![0xaau8; 3 * BLOCK_SIZE as usize]).unwrap();
    fs::write(dir.join("b/f"), vec![0xbbu8; BLOCK_SIZE as usize]).unwrap();

    let mut builder = Builder {
        blocks_per_group: 1024,
        inodes_per_group: 1024,
        root_dir: Some(dir.clone()),
        paths: Some(vec!["a".into(), "b".into()]),
        ..Default::default()
    };
    builder.set_auto_size(&dir).unwrap();
    let image = td.path().join("store.img");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&image)
        .unwrap();
    file.set_len(builder.size).unwrap();
    let shm = SharedMemory::from_file(file).unwrap();
    let mappings = builder
        .build_on_shm(&shm)
        .unwrap()
        .build_mmap_info()
        .unwrap()
        .mapping_info;
    let map = td.path().join("store.map");
    ext2::write_mappings(&map, &mappings).unwrap();

    let loaded =
        ext2::load_mappings(ext2::read_mappings(&map).unwrap(), &dir, &["a".into()]).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].path, PathBuf::from("a/f"));
    assert_eq!(loaded[0].length, 3 * BLOCK_SIZE as usize);
    assert!(
        ext2::load_mappings(ext2::read_mappings(&map).unwrap(), &dir, &["c".into()])
            .unwrap()
            .is_empty()
    );
    symlink(dir.join("b/f"), dir.join("a/l")).unwrap();
    let load_a = |line: String| {
        fs::write(&map, line).unwrap();
        ext2::load_mappings(ext2::read_mappings(&map).unwrap(), &dir, &["a".into()])
    };
    assert!(load_a("0 0 4096 a/l\0".to_string()).is_err());
    assert!(load_a(format!("{} 0 4096 a/f\0", u64::MAX)).is_err());
}

#[test]
fn test_filter_root_dir() {
    use base::linux::SharedMemoryLinux;
    use std::os::unix::fs::FileExt;

    let td = tempdir().unwrap();
    let dir = td.path().join("testdata");
    create_dir(&dir).unwrap();
    for name in ["a", "b", "c"] {
        create_dir(dir.join(name)).unwrap();
        fs::write(dir.join(name).join("f"), name.repeat(5000)).unwrap();
    }
    // Long filtered-out names push the root directory past its 12 direct blocks.
    let mut paths: Vec<std::ffi::OsString> = vec!["a".into(), "b".into(), "c".into()];
    for i in 0..300 {
        let name = format!("{i:0>200}");
        File::create(dir.join(&name)).unwrap();
        paths.push(name.into());
    }
    let mut builder = Builder {
        blocks_per_group: 1024,
        inodes_per_group: 1024,
        root_dir: Some(dir.clone()),
        paths: Some(paths),
        ..Default::default()
    };
    builder.set_auto_size(&dir).unwrap();
    let image = td.path().join("store.img");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&image)
        .unwrap();
    file.set_len(builder.size).unwrap();
    let shm = base::SharedMemory::from_file(file.try_clone().unwrap()).unwrap();
    let mappings = builder
        .build_on_shm(&shm)
        .unwrap()
        .build_mmap_info()
        .unwrap()
        .mapping_info;
    let map = td.path().join("store.map");
    ext2::write_mappings(&map, &mappings).unwrap();

    // With 240 long names the 13 compact blocks still need the indirect table; without, 1 block.
    for (long, k) in [(240, 13), (0, 1)] {
        let mut allowed: Vec<std::ffi::OsString> = vec!["a".into(), "c".into()];
        allowed.extend((0..long).map(|i| format!("{i:0>200}").into()));
        let root = ext2::filter_root_dir(&file, &allowed).unwrap();
        assert_eq!(root.len(), k + 1 + (k > 12) as usize);
        let data = ext2::load_mappings(ext2::read_mappings(&map).unwrap(), &dir, &allowed).unwrap();
        let copy = td.path().join("vm.img");
        fs::copy(&image, &copy).unwrap();
        let out = OpenOptions::new().write(true).open(&copy).unwrap();
        for m in root.iter().chain(&data) {
            let mut buf = vec![0u8; m.length];
            m.file
                .read_exact_at(&mut buf, m.file_offset as u64)
                .unwrap();
            out.write_all_at(&buf, m.mem_offset as u64).unwrap();
        }

        let ls = run_debugfs_cmd(&["ls -p /"], &copy);
        let names: BTreeSet<&str> = ls.lines().map(|l| l.split('/').nth(5).unwrap()).collect();
        let mut expected: BTreeSet<&str> = allowed.iter().map(|n| n.to_str().unwrap()).collect();
        expected.extend([".", "..", "lost+found"]);
        assert_eq!(names, expected);
        let stat = run_debugfs_cmd(&["stat /"], &copy);
        assert!(
            stat.contains(&format!("Size: {}", k as u32 * BLOCK_SIZE)),
            "{stat}"
        );
        let fsck = Command::new(FSCK_PATH)
            .arg("-fn")
            .arg(&copy)
            .output()
            .unwrap();
        let fsck = String::from_utf8_lossy(&fsck.stdout);
        println!("{fsck}");
        // Only the hidden entries' inodes, link counts and the root's freed blocks may differ.
        let allowed_errors = [
            "Pass ",
            "Unattached inode ",
            "Unconnected directory inode ",
            "Connect to /lost+found? no",
            "'..' in ",
            "Fix? no",
            "Block bitmap differences: ",
            "/",
        ];
        for line in fsck.lines().filter(|l| !l.is_empty()) {
            let ok = allowed_errors.iter().any(|a| line.starts_with(a))
                || (line.starts_with("Inode ") && line.contains(" ref count is "));
            assert!(ok, "{line}");
        }
        assert_eq!(run_debugfs_cmd(&["cat /a/f"], &copy), "a".repeat(5000));
        assert_eq!(run_debugfs_cmd(&["cat /c/f"], &copy), "c".repeat(5000));
    }
}

#[test]
fn test_mappings_round_trip() {
    let td = tempdir().unwrap();
    let src = td.path().join("f");
    fs::write(&src, b"x").unwrap();
    let info = |path: &str, file_offset, mem_offset, length| ext2::FileMappingInfo {
        mem_offset,
        file: File::open(&src).unwrap(),
        length,
        file_offset,
        path: PathBuf::from(path),
    };
    let map = td.path().join("map");
    ext2::write_mappings(
        &map,
        &[
            info("a/f", 0, 4096, 10),
            info("b/g  h\nx", 1 << 20, 1 << 33, 8192),
        ],
    )
    .unwrap();
    let entry = |path: &str, file_offset, mem_offset, length| ext2::MappingEntry {
        path: PathBuf::from(path),
        file_offset,
        mem_offset,
        length,
    };
    assert_eq!(
        ext2::read_mappings(&map).unwrap(),
        vec![
            entry("a/f", 0, 4096, 10),
            entry("b/g  h\nx", 1 << 20, 1 << 33, 8192)
        ]
    );

    for bad in ["0 4096 a/f\0", "0 x 1 a/f\0", "0 4096 1 \0", "0 4096 1 a/f"] {
        fs::write(&map, bad).unwrap();
        assert!(ext2::read_mappings(&map).is_err(), "{bad:?}");
    }
}
