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
