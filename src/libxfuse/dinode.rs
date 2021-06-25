use std::io::{BufRead, Seek, SeekFrom};

use super::bmbt_rec::BmbtRec;
use super::definitions::*;
use super::dinode_core::{DinodeCore, XfsDinodeFmt};
use super::dir2_block::Dir2Block;
use super::dir2_sf::Dir2Sf;
use super::sb::Sb;

use libc::{mode_t, S_IFDIR, S_IFMT};

#[derive(Debug)]
pub enum DiU {
    DiDir2Sf(Dir2Sf),
    DiBmx(Vec<BmbtRec>),
}

#[derive(Debug)]
pub enum InodeType {
    Dir2Sf(Dir2Sf),
    Dir2Block(Dir2Block),
}

#[derive(Debug)]
pub struct Dinode {
    pub di_core: DinodeCore,

    pub inode_type: InodeType,
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

        let inode_type: Option<InodeType>;
        if (di_core.di_mode as mode_t) & S_IFMT == S_IFDIR {
            match di_core.di_format {
                XfsDinodeFmt::XfsDinodeFmtLocal => {
                    let dir_sf = Dir2Sf::from(buf_reader.by_ref());
                    inode_type = Some(InodeType::Dir2Sf(dir_sf));
                }
                XfsDinodeFmt::XfsDinodeFmtExtents => {
                    let mut bmx = Vec::<BmbtRec>::new();
                    for _i in 0..di_core.di_nextents {
                        bmx.push(BmbtRec::from(buf_reader.by_ref()))
                    }

                    if bmx.len() == 1 {
                        let dir_blk =
                            Dir2Block::from(buf_reader.by_ref(), superblock, bmx[0].br_startblock);
                        inode_type = Some(InodeType::Dir2Block(dir_blk));
                    } else {
                        panic!("Directory format not yet supported.");
                    }
                }
                _ => {
                    panic!("Directory format not yet supported.");
                }
            }
        } else {
            panic!("Inode type not yet supported.")
        }

        Dinode {
            di_core,
            inode_type: inode_type.unwrap(),
        }
    }
}
