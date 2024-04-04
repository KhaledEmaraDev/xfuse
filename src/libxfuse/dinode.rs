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
use std::ffi::CString;
use std::io::{BufRead, Seek, SeekFrom};

use super::attr::Attributes;
use super::attr_bptree::AttrBtree;
use super::attr_shortform::AttrShortform;
use super::bmbt_rec::BmbtRec;
use super::btree::{BmbtKey, BmdrBlock, BtreeRoot, XfsBmbtPtr};
use super::definitions::*;
use super::dinode_core::{DinodeCore, XfsDinodeFmt};
use super::dir3::Directory;
use super::dir3_block::Dir2Block;
use super::dir3_bptree::Dir2Btree;
use super::dir3_leaf::Dir2Leaf;
use super::dir3_node::Dir2Node;
//use super::dir3_extents::Dir2Extents;
use super::dir3_sf::Dir2Sf;
use super::file::File;
use super::file_btree::FileBtree;
use super::file_extent_list::FileExtentList;
use super::sb::Sb;
use super::symlink_extent::SymlinkExtents;

use bincode::{
    Decode,
    de::{Decoder, read::Reader}
};
use libc::{mode_t, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK, S_IFIFO, S_IFCHR, S_IFBLK};

pub const LITERAL_AREA_OFFSET: usize = 0xb0;

#[derive(Debug)]
pub enum DiU {
    Blk,
    Bmbt((BmdrBlock, Vec<BmbtKey>, Vec<XfsBmbtPtr>)),
    Bmx(Vec<BmbtRec>),
    Chr,
    Dir2Sf(Dir2Sf),
    Fifo,
    Socket,
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
    /// Cache of this inode's directory object, if any.
    directory: Option<Directory>,
    /// Cache of this inode's attribute object, if any
    attributes: Option<Attributes>
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

        let off: u64 = ((ag_no * u64::from(superblock.sb_agblocks)) << superblock.sb_blocklog)
            + (ag_blk << superblock.sb_blocklog)
            + (blk_ino << superblock.sb_inodelog);

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

                    // The XFS Algorithms and Data Structures document contains
                    // an error here.  It says that the array of xfs_bmbt_ptr_t
                    // values immediately follows the array of xfs_bmbt_key_t
                    // values, and the size of both arrays is specified by
                    // bb_numrecs.  HOWEVER, there is actually a gap.  The space
                    // from the end of bmbt to the beginning of the attribute
                    // fork is split in half.  Half for keys and half for
                    // pointers.  The remaining space is padded with zeros.  the
                    // beginning of the attribute fork is given as di_forkoff *
                    // 8 bytes from the start of the literal area, which is
                    // where BmdrBlock is located.
                    let space = if di_core.di_forkoff == 0 {
                        (usize::from(superblock.sb_inodesize) - LITERAL_AREA_OFFSET) / 2
                    } else {
                        let space = usize::from(di_core.di_forkoff) * 8 / 2;
                        // Round up to a multiple of 8
                        let rem = space % 8;
                        if rem == 0 { space } else { space + 8 - rem }
                    };
                    let gap = space -
                        BmdrBlock::SIZE -
                        bmbt.bb_numrecs as usize * BmbtKey::SIZE -
                        /* XXX Why does it need this extra 4? */ 4;
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
                    let space = if di_core.di_forkoff == 0 {
                        (usize::from(superblock.sb_inodesize) - LITERAL_AREA_OFFSET) / 2
                    } else {
                        let space = usize::from(di_core.di_forkoff) * 8 / 2;
                        // Round up to a multiple of 8.  This is probably necessary, but I've never
                        // seen a directory like this in practice.
                        let rem = space % 8;
                        if rem == 0 { space } else { space + 8 - rem }
                    };
                    let gap = space -
                        BmdrBlock::SIZE -
                        bmbt.bb_numrecs as usize * BmbtKey::SIZE -
                        /* XXX Why does it need this extra 4? */ 4;

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
            S_IFBLK => {
                di_u = Some(DiU::Blk)
            },
            S_IFCHR => {
                di_u = Some(DiU::Chr)
            },
            S_IFIFO => {
                di_u = Some(DiU::Fifo)
            },
            S_IFSOCK => {
                di_u = Some(DiU::Socket)
            },
            _ => panic!("Inode type not yet supported."),
        }

        let attr_fork_ofs = LITERAL_AREA_OFFSET + di_core.di_forkoff as usize * 8;
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

                    // The XFS Algorithms and Data Structures document, section 15.4, isn't really
                    // specific about where the pointers are located.  They appear to be halfway
                    // between the start of the attribute fork and the end of the inode, minus 4
                    // bytes.
                    let ptr_ofs = (superblock.sb_inodesize as usize - attr_fork_ofs) / 2
                        + attr_fork_ofs
                        /* XXX Where does the -4 come from? */ - 4;
                    let gap = ptr_ofs - attr_fork_ofs -
                        BmdrBlock::SIZE -
                        usize::from(bmbt.bb_numrecs) * BmbtKey::SIZE;
                    decoder.reader().consume(gap as usize);
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
            directory: None,
            attributes: None
        }
    }

    pub fn get_dir<R: bincode::de::read::Reader + BufRead + Seek>(
        &mut self,
        buf_reader: &mut R,
        superblock: &Sb,
    ) -> &Directory {
        if self.directory.is_none() {
            let directory = match &self.di_u {
                DiU::Dir2Sf(dir) => Directory::Sf(dir.clone()),
                DiU::Bmx(bmx) => {
                    let leaf_start = superblock.get_dir3_leaf_offset().into();
                    if bmx.len() == 1 {
                        Directory::Block(Dir2Block::from(
                            buf_reader.by_ref(),
                            superblock,
                            bmx[0].br_startblock,
                        ))
                    } else if bmx.iter().filter(|e| e.br_startoff >= leaf_start).count() > 1 {
                        Directory::Node(Dir2Node::from(bmx.clone()))
                    } else {
                        Directory::Leaf(Dir2Leaf::from(buf_reader.by_ref(), superblock, bmx))
                    }
                }
                DiU::Bmbt((bmbt, keys, pointers)) => Directory::Btree(Dir2Btree::from(
                    bmbt.clone(),
                    keys.clone(),
                    pointers.clone(),
                )),
                _ => {
                    panic!("Unsupported dir format!");
                }
            };
            self.directory = Some(directory);
        }
        self.directory.as_ref().unwrap()
    }

    pub fn get_file<R: bincode::de::read::Reader + BufRead + Seek>(
        &self,
        _buf_reader: &mut R,
    ) -> Box<dyn File<R>> {
        match &self.di_u {
            DiU::Bmx(bmx) => Box::new(FileExtentList {
                bmx: bmx.clone(),
                size: self.di_core.di_size,
            }),
            DiU::Bmbt((bmdr, keys, pointers)) => Box::new(FileBtree {
                btree: BtreeRoot::new(bmdr.clone(), keys.clone(), pointers.clone()),
                size: self.di_core.di_size,
            }),
            _ => {
                panic!("Unsupported file format!");
            }
        }
    }

    pub fn get_link_data<R>(&self, buf_reader: &mut R, superblock: &Sb) -> CString
        where R: BufRead + Reader + Seek
    {
        match &self.di_u {
            DiU::Symlink(data) => CString::new(data.clone()).unwrap(),
            DiU::Bmx(bmx) => SymlinkExtents::get_target(buf_reader.by_ref(), bmx, superblock),
            _ => {
                panic!("Unsupported link format!");
            }
        }
    }

    pub fn get_attrs<R: Reader + BufRead + Seek>(
        &mut self,
        buf_reader: &mut R,
        superblock: &Sb,
    ) -> &mut Option<Attributes> {
        if self.attributes.is_none() {
            self.attributes = match &self.di_a {
                Some(DiA::Attrsf(attr)) => Some(Attributes::Sf(attr.clone())),
                Some(DiA::Abmx(bmx)) => {
                    if self.di_core.di_anextents > 0 {
                        Some(crate::libxfuse::attr::open(
                            buf_reader.by_ref(),
                            superblock,
                            bmx.clone(),
                        ))
                    } else {
                        None
                    }
                }
                Some(DiA::Abmbt((bmdr, keys, pointers))) => Some(Attributes::Btree(AttrBtree {
                    btree: BtreeRoot::new(bmdr.clone(), keys.clone(), pointers.clone()),
                    total_size: -1,
                })),
                None => None,
            };
        }
        &mut self.attributes
    }
}
