use std::io::{BufRead, Seek, SeekFrom};

use super::definitions::*;
use super::dinode_core::{DinodeCore, XfsDinodeFmt};
use super::dir2_sf::Dir2Sf;
use super::sb::Sb;

use libc::{S_IFDIR, S_IFMT};

pub const LITERAL_AREA_OFFSET: u8 = 0x4C;

#[derive(Debug)]
pub enum DiU {
    DiDir2Sf(Dir2Sf),
}

#[derive(Debug)]
pub struct Dinode {
    pub di_core: DinodeCore,
    pub di_u: DiU,
}

impl Dinode {
    pub fn from<T: BufRead + Seek>(
        buf_reader: &mut T,
        superblock: &Sb,
        inode_number: XfsIno,
    ) -> Dinode {
        let ag_no: XfsAgnumber =
            (inode_number >> (superblock.sb_agblklog + superblock.sb_inopblog)) as u32;
        if ag_no >= superblock.sb_agcount {
            panic!("Wrong AG number!");
        }

        let ag_blk: XfsAgblock =
            ((inode_number >> superblock.sb_inopblog) & ((1 << superblock.sb_agblklog) - 1)) as u32;
        let blk_ino = (inode_number & ((1 << superblock.sb_inopblog) - 1)) as u32;

        let off = ag_no * superblock.sb_agblocks * superblock.sb_blocksize;
        let off = off + ag_blk * superblock.sb_blocksize;
        let off = off + blk_ino * (superblock.sb_inodesize as u32);

        buf_reader.seek(SeekFrom::Start(off as u64)).unwrap();
        let di_core = DinodeCore::from(buf_reader);

        buf_reader
            .seek(SeekFrom::Current((LITERAL_AREA_OFFSET) as i64))
            .unwrap();
        let di_u: Option<DiU>;
        let format = XfsDinodeFmt::XfsDinodeFmtLocal as i8;
        if (di_core.di_mode as u32) & S_IFMT == S_IFDIR {
            if format == di_core.di_format {
                di_u = Some(DiU::DiDir2Sf(Dir2Sf::from(buf_reader.by_ref())))
            } else {
                panic!("Directory format not yet supported.")
            }
        } else {
            panic!("Inode type not yet supported.")
        }

        Dinode {
            di_core,
            di_u: di_u.unwrap(),
        }
    }
}
