/*
 * BSD 2-Clause License
 *
 * Copyright (c) 2021, Khaled Emara
 * All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions are met:
 *
 * 1. Redistributions of source code must retain the above copyright notice, this
 *    list of conditions and the following disclaimer.
 *
 * 2. Redistributions in binary form must reproduce the above copyright notice,
 *    this list of conditions and the following disclaimer in the documentation
 *    and/or other materials provided with the distribution.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
 * AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
 * DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
 * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 * SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
 * CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
 * OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
 * OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
 */
// The allocation machinery is a facility: the allocator that reads these
// structures is written next, and until then they are here to be tested against
// real images rather than used.
#[allow(dead_code)]
mod alloc;
mod attr;
mod attr_bptree;
mod attr_leaf;
mod attr_node;
mod attr_shortform;
// The modules below describe a *facility* -- a cache, a device, a vocabulary of
// errors, the fields of an inode -- rather than one operation, so their
// interfaces are complete even where the first caller has not arrived yet.  The
// write path grows into them over the coming phases; until it does, the unused
// parts are the price of having them written down and tested in one place
// instead of spread across the operations that will use them.
#[allow(dead_code)]
mod block_cache;
#[allow(dead_code)]
mod block_device;
#[allow(dead_code)]
mod block_reader;
#[allow(dead_code)]
mod bmbt_rec;
mod btree;
#[allow(dead_code)]
mod capabilities;
mod da_btree;
mod definitions;
mod dinode;
mod dinode_core;
mod dir3;
mod dir3_block;
mod dir3_lf;
mod dir3_sf;
#[allow(dead_code)]
mod error;
#[allow(dead_code)]
mod extent;
#[allow(dead_code)]
mod inode;
mod sb;
mod symlink_extent;
#[allow(dead_code)]
mod transaction;
mod utils;
pub mod volume;

use cfg_if::cfg_if;

cfg_if! {
    if #[cfg(target_os = "freebsd")] {
        use libc::ENOATTR;
    } else if #[cfg(target_os = "linux")] {
        const ENOATTR: i32 = libc::ENODATA;
    }
}

/// The errno to report for a file system that is damaged.
///
/// Linux and macOS have a dedicated code for "structure needs cleaning", which
/// is exactly what a corrupt on-disk structure is, and which tells the caller
/// that the answer is not going to improve on a retry.  The BSDs have no such
/// code, and an I/O error is the honest thing to report there: the damage was
/// found in the image, and reading the image is what failed.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const EUCLEAN: i32 = libc::EUCLEAN;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const EUCLEAN: i32 = libc::EIO;

#[allow(clippy::unnecessary_cast)] // It isn't unnecessary on all platforms.
const S_IFMT: u16 = libc::S_IFMT as u16;
