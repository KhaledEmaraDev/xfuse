/**
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
use std::ffi::CString;
use std::io::{BufRead, Seek, SeekFrom};
use std::mem;

use super::attr::Attr;
use super::attr_bptree::AttrBtree;
use super::attr_leaf::AttrLeaf;
use super::attr_shortform::AttrShortform;
use super::bmbt_rec::BmbtRec;
use super::btree::{BmbtKey, BmdrBlock, Btree, XfsBmbtPtr};
use super::definitions::*;
use super::dinode_core::{DinodeCore, XfsDinodeFmt};
use super::dir3::Dir3;
use super::dir3_block::Dir2Block;
use super::dir3_bptree::Dir2Btree;
use super::dir3_leaf::Dir2Leaf;
use super::dir3_node::Dir2Node;
use super::dir3_sf::Dir2Sf;
use super::file::File;
use super::file_btree::FileBtree;
use super::file_extent_list::FileExtentList;
use super::sb::Sb;
use super::utils::decode_from;
use super::symlink_extent::SymlinkExtents;

use bincode::{
    Decode,
    de::{Decoder, read::Reader}
};
use byteorder::BigEndian;
use byteorder::ReadBytesExt;
use libc::{mode_t, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};

pub const LITERAL_AREA_OFFSET: u8 = 0xb0;

#[derive(Debug)]
pub enum DiU {
    Dir2Sf(Dir2Sf),
    Bmx(Vec<BmbtRec>),
    Bmbt((BmdrBlock, Vec<BmbtKey>, Vec<XfsBmbtPtr>)),
    Symlink(Vec<u8>),
}

#[derive(Debug)]
pub enum DiA {
    Attrsf(AttrShortform),
    Abmx(Vec<BmbtRec>),
    Abmbt((BmdrBlock, Vec<BmbtKey>, Vec<XfsBmbtPtr>)),
}

#[derive(Debug)]
pub struct Dinode {
    pub di_core: DinodeCore,
    pub di_u: DiU,
    pub di_a: Option<DiA>,
}

impl Dinode {
    pub fn from<R: bincode::de::read::Reader + BufRead + Seek>(
        buf_reader: &mut R,
        superblock: &Sb,
        inode_number: XfsIno,
    ) -> Dinode {
        let ag_no: u64 =
            inode_number >> (superblock.sb_agblklog + superblock.sb_inopblog);
        if ag_no >= superblock.sb_agcount.into() {
            panic!("Wrong AG number!");
        }

        let ag_blk: u64 =
            (inode_number >> superblock.sb_inopblog) & ((1 << superblock.sb_agblklog) - 1);
        let blk_ino = inode_number & ((1 << superblock.sb_inopblog) - 1);

        let off = ag_no * u64::from(superblock.sb_agblocks) * u64::from(superblock.sb_blocksize)
            + ag_blk * u64::from(superblock.sb_blocksize)
            + blk_ino * u64::from(superblock.sb_inodesize);

        buf_reader.seek(SeekFrom::Start(off)).unwrap();
        let mut raw = vec![0u8; superblock.sb_inodesize.into()];
        buf_reader.read_exact(&mut raw).unwrap();
        let config = bincode::config::standard()
            .with_big_endian()
            .with_fixed_int_encoding();
        let reader = bincode::de::read::SliceReader::new(&raw[..]);
        let mut decoder = bincode::de::DecoderImpl::new(reader, config);

        let di_core = DinodeCore::decode(&mut decoder).unwrap();
        di_core.sanity();

        let di_u: Option<DiU>;
        match (di_core.di_mode as mode_t) & S_IFMT {
            S_IFREG => match di_core.di_format {
                XfsDinodeFmt::Extents => {
                    let mut bmx = Vec::<BmbtRec>::new();
                    for _i in 0..di_core.di_nextents {
                        bmx.push(BmbtRec::decode(&mut decoder).unwrap())
                    }
                    di_u = Some(DiU::Bmx(bmx));
                }
                XfsDinodeFmt::Btree => {
                    let bmbt = BmdrBlock::decode(&mut decoder).unwrap();

                    let mut keys = Vec::<BmbtKey>::new();
                    for _i in 0..bmbt.bb_numrecs {
                        keys.push(BmbtKey::decode(&mut decoder).unwrap())
                    }

                    let mut pointers = Vec::<XfsBmbtPtr>::new();
                    for _i in 0..bmbt.bb_numrecs {
                        let pointer = decode_from(buf_reader.by_ref()).unwrap();
                        pointers.push(pointer)
                    }

                    di_u = Some(DiU::Bmbt((bmbt, keys, pointers)));
                }
                _ => {
                    panic!("Directory format not yet supported.");
                }
            },
            S_IFDIR => match di_core.di_format {
                XfsDinodeFmt::Local => {
                    let mut dir_sf = Dir2Sf::decode(&mut decoder).unwrap();
                    dir_sf.set_ino(inode_number);
                    di_u = Some(DiU::Dir2Sf(dir_sf));
                }
                XfsDinodeFmt::Extents => {
                    let mut bmx = Vec::<BmbtRec>::new();
                    for _i in 0..di_core.di_nextents {
                        bmx.push(BmbtRec::decode(&mut decoder).unwrap())
                    }
                    di_u = Some(DiU::Bmx(bmx));
                }
                XfsDinodeFmt::Btree => {
                    let bmbt = BmdrBlock::decode(&mut decoder).unwrap();

                    let mut keys = Vec::<BmbtKey>::new();
                    for _i in 0..bmbt.bb_numrecs {
                        keys.push(BmbtKey::decode(&mut decoder).unwrap());
                    }

                    // The XFS Algorithms and Data Structures document contains
                    // an error here.  It says that the pointers start
                    // immediately after the keys, and the size of each array is
                    // given by bb_numrecs.  HOWEVER, there is actually a gap.
                    // The space from the end of bmbt to the end of the inode is
                    // divided by half.  Half for keys and half for pointers,
                    // even if only one of each are allocated.  The remaining
                    // space is padded with zeros.
                    assert_eq!(di_core.di_forkoff, 0, "TODO");
                    let maxrecs = (superblock.sb_inodesize as usize - (DinodeCore::SIZE + BmdrBlock::SIZE)) /
                        (BmbtKey::SIZE + mem::size_of::<XfsBmbtPtr>());
                    let gap = (maxrecs - bmbt.bb_numrecs as usize) as i64 * BmbtKey::SIZE as i64;
                    decoder.reader().consume(gap as usize);
                    let mut pointers = Vec::<XfsBmbtPtr>::new();
                    for _i in 0..bmbt.bb_numrecs {
                        let pointer = u64::decode(&mut decoder).unwrap();
                        pointers.push(pointer)
                    }

                    di_u = Some(DiU::Bmbt((bmbt, keys, pointers)));
                }
                _ => {
                    panic!("Directory format not yet supported.");
                }
            },
            S_IFLNK => match di_core.di_format {
                XfsDinodeFmt::Local => {
                    let mut data = vec![0u8; di_core.di_size as usize];
                    decoder.reader().read(&mut data[..]).unwrap();
                    di_u = Some(DiU::Symlink(data))
                }
                XfsDinodeFmt::Extents => {
                    let mut bmx = Vec::<BmbtRec>::new();
                    for _i in 0..di_core.di_nextents {
                        bmx.push(BmbtRec::decode(&mut decoder).unwrap());
                    }
                    di_u = Some(DiU::Bmx(bmx));
                }
                _ => {
                    panic!("Unexpected format for symlink");
                }
            },
            _ => panic!("Inode type not yet supported."),
        }

        let attr_fork_ofs = LITERAL_AREA_OFFSET as usize + di_core.di_forkoff as usize * 8;
        let config = bincode::config::standard()
            .with_big_endian()
            .with_fixed_int_encoding();
        let reader = bincode::de::read::SliceReader::new(&raw[attr_fork_ofs..]);
        let mut decoder = bincode::de::DecoderImpl::new(reader, config);

        let di_a: Option<DiA>;
        if di_core.di_forkoff != 0 {
            match di_core.di_aformat {
                XfsDinodeFmt::Local => {
                    let attr_shortform = AttrShortform::decode(&mut decoder).unwrap();
                    di_a = Some(DiA::Attrsf(attr_shortform));
                }
                XfsDinodeFmt::Extents => {
                    let mut bmx = Vec::<BmbtRec>::new();
                    for _i in 0..di_core.di_anextents {
                        bmx.push(BmbtRec::decode(&mut decoder).unwrap());
                    }
                    di_a = Some(DiA::Abmx(bmx));
                }
                XfsDinodeFmt::Btree => {
                    let bmbt = BmdrBlock::decode(&mut decoder).unwrap();

                    let mut keys = Vec::<BmbtKey>::new();
                    for _i in 0..bmbt.bb_numrecs {
                        keys.push(BmbtKey::decode(&mut decoder).unwrap());
                    }

                    let mut pointers = Vec::<XfsBmbtPtr>::new();
                    for _i in 0..bmbt.bb_numrecs {
                        pointers.push(XfsBmbtPtr::decode(&mut decoder).unwrap());
                    }

                    di_a = Some(DiA::Abmbt((bmbt, keys, pointers)));
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

    pub fn get_dir<R: bincode::de::read::Reader + BufRead + Seek>(
        &self,
        buf_reader: &mut R,
        superblock: &Sb,
    ) -> Box<dyn Dir3<R>> {
        match &self.di_u {
            DiU::Dir2Sf(dir) => Box::new(dir.clone()),
            DiU::Bmx(bmx) => {
                if bmx.len() == 1 {
                    Box::new(Dir2Block::from(
                        buf_reader.by_ref(),
                        superblock,
                        bmx[0].br_startblock,
                    ))
                } else if bmx.len() > 4 {
                    Box::new(Dir2Node::from(bmx.clone(), superblock.sb_blocksize))
                } else {
                    Box::new(Dir2Leaf::from(buf_reader.by_ref(), superblock, bmx))
                }
            }
            DiU::Bmbt((bmbt, keys, pointers)) => Box::new(Dir2Btree::from(
                bmbt.clone(),
                keys.clone(),
                pointers.clone(),
                superblock.sb_blocksize,
            )),
            _ => {
                panic!("Unsupported dir format!");
            }
        }
    }

    pub fn get_file<R: BufRead + Seek>(
        &self,
        _buf_reader: &mut R,
        superblock: &Sb,
    ) -> Box<dyn File<R>> {
        match &self.di_u {
            DiU::Bmx(bmx) => Box::new(FileExtentList {
                bmx: bmx.clone(),
                size: self.di_core.di_size,
                block_size: superblock.sb_blocksize,
            }),
            DiU::Bmbt((bmdr, keys, pointers)) => Box::new(FileBtree {
                btree: Btree {
                    bmdr: bmdr.clone(),
                    keys: keys.clone(),
                    ptrs: pointers.clone(),
                },
                size: self.di_core.di_size,
                block_size: superblock.sb_blocksize,
            }),
            _ => {
                panic!("Unsupported file format!");
            }
        }
    }

    pub fn get_link_data<R: BufRead + Seek>(&self, buf_reader: &mut R, superblock: &Sb) -> CString {
        match &self.di_u {
            DiU::Symlink(data) => CString::new(data.clone()).unwrap(),
            DiU::Bmx(bmx) => SymlinkExtents::get_target(buf_reader.by_ref(), bmx, superblock),
            _ => {
                panic!("Unsupported link format!");
            }
        }
    }

    pub fn get_attrs<R: Reader + BufRead + Seek>(
        &self,
        buf_reader: &mut R,
        superblock: &Sb,
    ) -> Option<Box<dyn Attr<R>>> {
        match &self.di_a {
            Some(DiA::Attrsf(attr)) => Some(Box::new(attr.clone())),
            Some(DiA::Abmx(bmx)) => {
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
                        _ => panic!("Unknown magic number!"),
                    }
                } else {
                    None
                }
            }
            Some(DiA::Abmbt((bmdr, keys, pointers))) => Some(Box::new(AttrBtree {
                btree: Btree {
                    bmdr: bmdr.clone(),
                    keys: keys.clone(),
                    ptrs: pointers.clone(),
                },
                total_size: -1,
            })),
            None => None,
        }
    }
}
