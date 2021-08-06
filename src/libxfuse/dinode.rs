use std::ffi::CString;
use std::io::{BufRead, Seek, SeekFrom};

use super::attr::Attr;
use super::attr_bptree::AttrBtree;
use super::attr_leaf::AttrLeaf;
use super::attr_shortform::AttrShortform;
use super::bmbt_rec::BmbtRec;
use super::btree::{BmbtKey, BmdrBlock, Btree, XfsBmbtPtr};
use super::definitions::*;
use super::dinode_core::{DinodeCore, XfsDinodeFmt};
use super::dir3_block::Dir2Block;
use super::dir3_bptree::Dir2Btree;
use super::dir3_leaf::Dir2Leaf;
use super::dir3_node::Dir2Node;
use super::dir3_sf::Dir2Sf;
use super::file::File;
use super::file_btree::FileBtree;
use super::file_extent_list::FileExtentList;
use super::sb::Sb;
use super::symlink_extent::SymlinkExtents;

use byteorder::BigEndian;
use byteorder::ReadBytesExt;
use libc::{mode_t, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};

pub const LITERAL_AREA_OFFSET: u8 = 0xb0;

#[derive(Debug)]
pub enum DiU {
    DiDir2Sf(Dir2Sf),
    DiBmx(Vec<BmbtRec>),
    DiBmbt((BmdrBlock, Vec<BmbtKey>, Vec<XfsBmbtPtr>)),
    DiSymlink(Vec<u8>),
}

#[derive(Debug)]
pub enum DiA {
    DiAttrsf(AttrShortform),
    DiAbmx(Vec<BmbtRec>),
    DiAbmbt((BmdrBlock, Vec<BmbtKey>, Vec<XfsBmbtPtr>)),
}

#[derive(Debug)]
pub enum InodeType {
    Dir2Sf(Dir2Sf),
    Dir2Block(Dir2Block),
    Dir2Leaf(Dir2Leaf),
    Dir2Node(Dir2Node),
    Dir2Btree(Dir2Btree),
    SymlinkSf(CString),
    SymlinkExtents(CString),
}

#[derive(Debug)]
pub struct Dinode {
    pub di_core: DinodeCore,
    pub di_u: DiU,
    pub di_a: Option<DiA>,
}

impl Dinode {
    pub fn from<R: BufRead + Seek>(
        buf_reader: &mut R,
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

        let di_u: Option<DiU>;
        match (di_core.di_mode as mode_t) & S_IFMT {
            S_IFREG => match di_core.di_format {
                XfsDinodeFmt::Extents => {
                    let mut bmx = Vec::<BmbtRec>::new();
                    for _i in 0..di_core.di_nextents {
                        bmx.push(BmbtRec::from(buf_reader.by_ref()))
                    }
                    di_u = Some(DiU::DiBmx(bmx));
                }
                XfsDinodeFmt::Btree => {
                    let bmbt = BmdrBlock::from(buf_reader.by_ref());

                    let mut keys = Vec::<BmbtKey>::new();
                    for _i in 0..bmbt.bb_numrecs {
                        keys.push(BmbtKey::from(buf_reader.by_ref()))
                    }

                    let mut pointers = Vec::<XfsBmbtPtr>::new();
                    for _i in 0..bmbt.bb_numrecs {
                        let pointer = buf_reader.read_u64::<BigEndian>().unwrap();
                        pointers.push(pointer)
                    }

                    di_u = Some(DiU::DiBmbt((bmbt, keys, pointers)));
                }
                _ => {
                    panic!("Directory format not yet supported.");
                }
            },
            S_IFDIR => match di_core.di_format {
                XfsDinodeFmt::Local => {
                    let dir_sf = Dir2Sf::from(buf_reader.by_ref());
                    di_u = Some(DiU::DiDir2Sf(dir_sf));
                }
                XfsDinodeFmt::Extents => {
                    let mut bmx = Vec::<BmbtRec>::new();
                    for _i in 0..di_core.di_nextents {
                        bmx.push(BmbtRec::from(buf_reader.by_ref()))
                    }
                    di_u = Some(DiU::DiBmx(bmx));
                }
                XfsDinodeFmt::Btree => {
                    let bmbt = BmdrBlock::from(buf_reader.by_ref());

                    let mut keys = Vec::<BmbtKey>::new();
                    for _i in 0..bmbt.bb_numrecs {
                        keys.push(BmbtKey::from(buf_reader.by_ref()))
                    }

                    let mut pointers = Vec::<XfsBmbtPtr>::new();
                    for _i in 0..bmbt.bb_numrecs {
                        let pointer = buf_reader.read_u64::<BigEndian>().unwrap();
                        pointers.push(pointer)
                    }

                    di_u = Some(DiU::DiBmbt((bmbt, keys, pointers)));
                }
                _ => {
                    panic!("Directory format not yet supported.");
                }
            },
            S_IFLNK => match di_core.di_format {
                XfsDinodeFmt::Local => {
                    let mut data = Vec::<u8>::with_capacity(di_core.di_size as usize);
                    for _i in 0..di_core.di_size {
                        let byte = buf_reader.read_u8().unwrap();
                        data.push(byte)
                    }
                    di_u = Some(DiU::DiSymlink(data))
                }
                XfsDinodeFmt::Extents => {
                    let mut bmx = Vec::<BmbtRec>::new();
                    for _i in 0..di_core.di_nextents {
                        bmx.push(BmbtRec::from(buf_reader.by_ref()))
                    }
                    di_u = Some(DiU::DiBmx(bmx));
                }
                _ => {
                    panic!("Unexpected format for symlink");
                }
            },
            _ => panic!("Inode type not yet supported."),
        }

        buf_reader.seek(SeekFrom::Start(off as u64)).unwrap();
        buf_reader
            .seek(SeekFrom::Current(
                (LITERAL_AREA_OFFSET as i64) + ((di_core.di_forkoff as i64) * 8),
            ))
            .unwrap();

        let di_a: Option<DiA>;
        if di_core.di_forkoff != 0 {
            match di_core.di_aformat {
                XfsDinodeFmt::Local => {
                    let attr_shortform = AttrShortform::from(buf_reader.by_ref());
                    di_a = Some(DiA::DiAttrsf(attr_shortform));
                }
                XfsDinodeFmt::Extents => {
                    let mut bmx = Vec::<BmbtRec>::new();
                    for _i in 0..di_core.di_anextents {
                        bmx.push(BmbtRec::from(buf_reader.by_ref()))
                    }
                    di_a = Some(DiA::DiAbmx(bmx));
                }
                XfsDinodeFmt::Btree => {
                    let bmbt = BmdrBlock::from(buf_reader.by_ref());

                    let mut keys = Vec::<BmbtKey>::new();
                    for _i in 0..bmbt.bb_numrecs {
                        keys.push(BmbtKey::from(buf_reader.by_ref()))
                    }

                    let mut pointers = Vec::<XfsBmbtPtr>::new();
                    for _i in 0..bmbt.bb_numrecs {
                        let pointer = buf_reader.read_u64::<BigEndian>().unwrap();
                        pointers.push(pointer)
                    }

                    di_a = Some(DiA::DiAbmbt((bmbt, keys, pointers)));
                }
                _ => {
                    panic!("Attributes format not yet supported.");
                }
            }
        } else {
            di_a = None;
        }

        Dinode {
            di_core,
            di_u: di_u.unwrap(),
            di_a,
        }
    }

    pub fn get_file<R: BufRead + Seek>(
        &self,
        _buf_reader: &mut R,
        superblock: &Sb,
    ) -> Box<dyn File<R>> {
        match &self.di_u {
            DiU::DiBmx(bmx) => {
                return Box::new(FileExtentList {
                    bmx: bmx.clone(),
                    size: self.di_core.di_size,
                    block_size: superblock.sb_blocksize,
                });
            }
            DiU::DiBmbt((bmdr, keys, pointers)) => {
                return Box::new(FileBtree {
                    btree: Btree {
                        bmdr: bmdr.clone(),
                        keys: keys.clone(),
                        ptrs: pointers.clone(),
                    },
                    size: self.di_core.di_size,
                    block_size: superblock.sb_blocksize,
                });
            }
            _ => {
                panic!("Unsupported file format!");
            }
        }
    }

    pub fn get_data<R: BufRead + Seek>(&self, buf_reader: &mut R, superblock: &Sb) -> InodeType {
        match &self.di_u {
            DiU::DiDir2Sf(dir) => InodeType::Dir2Sf(dir.clone()),
            DiU::DiBmx(bmx) => match (self.di_core.di_mode as mode_t) & S_IFMT {
                S_IFDIR => {
                    if bmx.len() == 1 {
                        let dir_blk =
                            Dir2Block::from(buf_reader.by_ref(), superblock, bmx[0].br_startblock);
                        InodeType::Dir2Block(dir_blk)
                    } else if bmx.len() > 4 {
                        let dir_node = Dir2Node::from(bmx.clone(), superblock.sb_blocksize);
                        InodeType::Dir2Node(dir_node)
                    } else {
                        let dir_leaf = Dir2Leaf::from(buf_reader.by_ref(), superblock, bmx);
                        InodeType::Dir2Leaf(dir_leaf)
                    }
                }
                S_IFLNK => InodeType::SymlinkExtents(SymlinkExtents::get_target(
                    buf_reader.by_ref(),
                    bmx,
                    superblock,
                )),
                _ => {
                    panic!("This shouldn't be reachable.");
                }
            },
            DiU::DiBmbt((bmbt, keys, pointers)) => InodeType::Dir2Btree(Dir2Btree::from(
                bmbt.clone(),
                keys.clone(),
                pointers.clone(),
                superblock.sb_blocksize,
            )),
            DiU::DiSymlink(data) => InodeType::SymlinkSf(CString::new(data.clone()).unwrap()),
        }
    }

    pub fn get_attrs<R: BufRead + Seek>(
        &self,
        buf_reader: &mut R,
        superblock: &Sb,
    ) -> Option<Box<dyn Attr<R>>> {
        match &self.di_a {
            Some(DiA::DiAttrsf(attr)) => Some(Box::new(attr.clone())),
            Some(DiA::DiAbmx(bmx)) => {
                if self.di_core.di_anextents > 0 {
                    buf_reader.seek(SeekFrom::Current(8)).unwrap();
                    let magic = buf_reader.read_u16::<BigEndian>().unwrap();
                    buf_reader.seek(SeekFrom::Current(-8)).unwrap();

                    match magic {
                        XFS_ATTR3_LEAF_MAGIC => {
                            return Some(Box::new(AttrLeaf::from(
                                buf_reader.by_ref(),
                                superblock,
                                bmx.clone(),
                            )));
                        }
                        XFS_DA3_NODE_MAGIC => {
                            return Some(Box::new(AttrLeaf::from(
                                buf_reader.by_ref(),
                                superblock,
                                bmx.clone(),
                            )));
                        }
                        _ => panic!("Unkown magic number!"),
                    }
                } else {
                    return None;
                }
            }
            Some(DiA::DiAbmbt((bmdr, keys, pointers))) => {
                return Some(Box::new(AttrBtree {
                    btree: Btree {
                        bmdr: bmdr.clone(),
                        keys: keys.clone(),
                        ptrs: pointers.clone(),
                    },
                    total_size: -1,
                }));
            }
            None => None,
        }
    }
}
