// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This crate provides a logic for creating an ext2 filesystem on memory.

#![cfg(any(target_os = "android", target_os = "linux"))]
#![deny(missing_docs)]

mod arena;
mod bitmap;
mod blockgroup;
mod builder;
mod fs;
mod inode;
mod mappings;
mod superblock;
mod xattr;

pub use arena::FileMappingInfo;
pub use blockgroup::BLOCK_SIZE;
pub use builder::read_paths_file;
pub use builder::Builder;
pub use mappings::filter_root_dir;
pub use mappings::load_mappings;
pub use mappings::read_mappings;
pub use mappings::write_mappings;
pub use mappings::MappingEntry;
pub use xattr::dump_xattrs;
pub use xattr::set_xattr;
