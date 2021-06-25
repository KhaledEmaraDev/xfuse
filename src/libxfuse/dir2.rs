use std::io::{BufRead, Seek};

use super::{definitions::XfsIno, sb::Sb};

use fuse::{FileAttr, FileType};
use libc::c_int;

pub type XfsDir2DataOff = u16;
pub type XfsDir2Dataptr = u32;

pub const XFS_DIR3_FT_UNKNOWN: u8 = 0;
pub const XFS_DIR3_FT_REG_FILE: u8 = 1;
pub const XFS_DIR3_FT_DIR: u8 = 2;
pub const XFS_DIR3_FT_CHRDEV: u8 = 3;
pub const XFS_DIR3_FT_BLKDEV: u8 = 4;
pub const XFS_DIR3_FT_FIFO: u8 = 5;
pub const XFS_DIR3_FT_SOCK: u8 = 6;
pub const XFS_DIR3_FT_SYMLINK: u8 = 7;
pub const XFS_DIR3_FT_WHT: u8 = 8;

pub trait Dir2 {
    fn lookup<T: BufRead + Seek>(
        &self,
        buf_reader: &mut T,
        super_block: &Sb,
        name: &str,
    ) -> Result<(FileAttr, u64), c_int>;

    fn iterate<T: BufRead + Seek>(
        &self,
        buf_reader: &mut T,
        offset: i64,
    ) -> Result<(XfsIno, i64, FileType, String), c_int>;
}
