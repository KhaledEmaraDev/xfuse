mod attr;
mod attr_bptree;
mod attr_leaf;
mod attr_node;
mod attr_shortform;
mod block_device;
mod block_reader;
mod bmbt_rec;
mod btree;
mod da_btree;
mod definitions;
mod dinode;
mod dinode_core;
mod dir3;
mod dir3_block;
mod dir3_lf;
mod dir3_sf;
mod file;
mod file_btree;
mod file_extent_list;
mod sb;
mod symlink_extent;
mod utils;
pub mod volume;

pub use fuser::FileType;
pub use libc::{c_int, c_ulong};
use cfg_if::cfg_if;

cfg_if! {
    if #[cfg(target_os = "freebsd")] {
        use libc::ENOATTR;
    } else if #[cfg(target_os = "linux")] {
        const ENOATTR: i32 = libc::ENODATA;
    }
}

#[allow(clippy::unnecessary_cast)] // It isn't unnecessary on all platforms.
const S_IFMT: u16 = libc::S_IFMT as u16;
