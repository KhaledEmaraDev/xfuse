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
use std::cell::RefCell;
use std::convert::TryInto;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, Seek};
use std::ops::{Deref, Range};
use std::os::unix::ffi::OsStringExt;

use super::da_btree::{XfsDaBlkinfo, XfsDa3Blkinfo, hashname, XfsDa3Intnode};
use super::definitions::*;
use super::sb::Sb;
use super::utils::{FileKind, Uuid, decode, get_file_type};
use super::volume::SUPERBLOCK;

use bincode::{
    Decode,
    de::{Decoder, read::Reader},
    error::DecodeError
};
use fuser::FileType;
use libc::{c_int, ENOENT};
use tracing::error;

pub type XfsDir2DataOff = u16;
pub type XfsDir2Dataptr = u32;

#[allow(dead_code)]
mod constants {
    pub const XFS_DIR2_DATA_FD_COUNT: usize = 3;

    pub const XFS_DIR3_FT_UNKNOWN: u8 = 0;
    pub const XFS_DIR3_FT_REG_FILE: u8 = 1;
    pub const XFS_DIR3_FT_DIR: u8 = 2;
    pub const XFS_DIR3_FT_CHRDEV: u8 = 3;
    pub const XFS_DIR3_FT_BLKDEV: u8 = 4;
    pub const XFS_DIR3_FT_FIFO: u8 = 5;
    pub const XFS_DIR3_FT_SOCK: u8 = 6;
    pub const XFS_DIR3_FT_SYMLINK: u8 = 7;
    pub const XFS_DIR3_FT_WHT: u8 = 8;
}
pub use constants::*;

#[derive(Debug, Decode)]
pub struct Dir3BlkHdr {
    pub magic: u32,
    _crc: u32,
    _blkno: u64,
    _lsn: u64,
    _uuid: Uuid,
    _owner: u64,
}

impl Dir3BlkHdr {
    pub const SIZE: u64 = 48;
}

#[derive(Debug, Decode, Clone, Copy)]
pub struct Dir2DataFree {
    _offset: XfsDir2DataOff,
    _length: XfsDir2DataOff,
}

impl Dir2DataFree {
    pub const SIZE: u64 = 4;
}

#[derive(Debug, Decode)]
pub struct Dir2DataHdr {
    pub magic: u32,
    _best_free: [Dir2DataFree; constants::XFS_DIR2_DATA_FD_COUNT],
}

impl Dir2DataHdr {
    pub const SIZE: u64 = 4 + constants::XFS_DIR2_DATA_FD_COUNT as u64 * Dir2DataFree::SIZE;
}

#[derive(Debug, Decode)]
pub struct Dir3DataHdr {
    pub hdr: Dir3BlkHdr,
    _best_free: [Dir2DataFree; constants::XFS_DIR2_DATA_FD_COUNT],
    _pad: u32,
}

impl Dir3DataHdr {
    pub const SIZE: u64 = Dir3BlkHdr::SIZE + constants::XFS_DIR2_DATA_FD_COUNT as u64 * Dir2DataFree::SIZE + 4;
}

#[derive(Debug)]
pub struct Dir2DataEntry {
    pub inumber: XfsIno,
    pub name: OsString,
    ftype: Option<u8>,
    pub tag: XfsDir2DataOff,
}

impl Dir2DataEntry {
    pub fn get_length(sb: &Sb, raw: &[u8]) -> i64 {
        let namelen: u8 = decode(&raw[8..]).unwrap().0;
        if sb.has_ftype() {
            ((namelen as i64 + 19) / 8) * 8
        } else {
            ((namelen as i64 + 18) / 8) * 8
        }
    }
}

impl Decode for Dir2DataEntry {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let inumber = Decode::decode(decoder)?;
        let sb = SUPERBLOCK.get().unwrap();
        let namelen: u8 = Decode::decode(decoder)?;
        let mut namebytes = vec![0u8; namelen.into()];
        decoder.reader().read(&mut namebytes[..])?;
        let name = OsString::from_vec(namebytes);
        let ftype: Option<u8> = if sb.has_ftype() {
            Some(Decode::decode(decoder)?)
        } else {
            None
        };
        // Pad up to 1 less than a multiple of 8 bytes
        let pad: usize = if sb.has_ftype() {
            // current offset is 9 + 1 + namelen + 1
            4 - namelen as i16
        } else {
            // current offset is 9 + 1 + namelen
            5 - namelen as i16
        }.rem_euclid(8).try_into().unwrap();
        decoder.reader().consume(pad);
        let tag = Decode::decode(decoder)?;
        Ok(Dir2DataEntry {
            inumber,
            name,
            ftype,
            tag,
        })
    }
}

#[derive(Debug)]
pub struct Dir2DataUnused {
    _freetag: u16,
    _length: XfsDir2DataOff,
    _tag: XfsDir2DataOff,
}

impl Decode for Dir2DataUnused {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let _freetag = Decode::decode(decoder)?;
        let length = Decode::decode(decoder)?;
        decoder.reader().consume(length as usize - 6);
        let _tag = Decode::decode(decoder)?;
        Ok(Dir2DataUnused {
            _freetag,
            _length: length,
            _tag,
        })
    }
}

#[derive(Debug, Decode)]
pub struct Dir2LeafHdr {
    info: XfsDaBlkinfo,
    pub count: u16,
    _stale: u16,
}

#[derive(Debug, Decode)]
pub struct Dir3LeafHdr {
    pub info: XfsDa3Blkinfo,
    pub count: u16,
    _stale: u16,
    _pad: u32,
}

#[derive(Clone, Copy, Debug, Decode, Default)]
pub struct Dir2LeafEntry {
    pub hashval: XfsDahash,
    pub address: XfsDir2Dataptr,
}

impl Dir2LeafEntry {
    /// On-disk size in bytes
    pub const SIZE: usize = 8;
}

#[derive(Debug)]
pub struct Dir2LeafNDisk {
    forw: u32,
    pub ents: Vec<Dir2LeafEntry>,
}

impl Dir2LeafNDisk {
    /// Return the range of entry indices that include the given hash
    pub fn get_address_range(&self, hash: XfsDahash) -> Range<usize> {
        let l = self.ents.len();
        let i = self.ents.partition_point(|ent| ent.hashval < hash);
        let j = (i..l).find(|x| self.ents[*x].hashval > hash).unwrap_or(l);
        i..j
    }
}

impl Decode for Dir2LeafNDisk {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let magic: u16 = decode(&decoder.reader().peek_read(10).unwrap()[8..])?.0;
        let (count, forw) = match magic {
            XFS_DIR2_LEAFN_MAGIC => {
                let hdr: Dir2LeafHdr = Decode::decode(decoder)?;
                (hdr.count, hdr.info.forw)
            },
            XFS_DIR3_LEAFN_MAGIC => {
                let hdr: Dir3LeafHdr = Decode::decode(decoder)?;
                (hdr.count, hdr.info.forw)
            }
            _ => panic!("Unexpected magic {:#x}", magic)
        };
        let mut ents = Vec::<Dir2LeafEntry>::new();
        for _i in 0..count {
            let leaf_entry: Dir2LeafEntry = Decode::decode(decoder)?;
            ents.push(leaf_entry);
        }

        Ok(Dir2LeafNDisk { forw, ents })
    }
}

/// Stores the "leaf" info (the hash => address map) for Node and Btree directories.  But does not
/// store the freeindex info.
enum Leaf {
    LeafN(Dir2LeafNDisk),
    Btree(XfsDa3Intnode)
}

impl Leaf {
    fn open(raw: &[u8]) -> Self {
        let magic: u16 = decode(&raw[8..]).unwrap().0;
        match magic {
            XFS_DA_NODE_MAGIC | XFS_DA3_NODE_MAGIC => {
                let (leaf_btree, _) = decode::<XfsDa3Intnode>(raw).map_err(|_| libc::EIO).unwrap();
                assert!(leaf_btree.magic == XFS_DA3_NODE_MAGIC || leaf_btree.magic == XFS_DA_NODE_MAGIC);
                Self::Btree(leaf_btree)
            },
            XFS_DIR2_LEAFN_MAGIC | XFS_DIR3_LEAFN_MAGIC => {
                Self::LeafN(decode(raw).unwrap().0)
            },
            magic => panic!("Bad magic in Leaf block! {:#x}", magic),
        }
    }

    fn lookup_leaf_blk<D, R>(
        self,
        buf_reader: &mut R,
        sb: &Sb,
        dir: &D,
        hash: u32,
    ) -> Result<Dir2LeafNDisk, i32>
        where D: NodeLikeDir,
              R: BufRead + Reader + Seek,

    {
        match self {
            Leaf::LeafN(leafn) => Ok(leafn),
            Leaf::Btree(btree) => {
                let dablk: XfsDablk = btree.lookup(buf_reader.by_ref(), sb, hash,
                    |block, br| dir.map_dblock(br, block).unwrap()
                )?;
                let raw = dir.read_dblock(buf_reader.by_ref(), sb, dablk)?;
                Ok(decode(&raw).unwrap().0)
            }
        }
    }
}

/// Iterates through all dirents with a given hash, for NodeLike directories
#[derive(Debug)]
struct NodeLikeAddressIterator<'a, D: NodeLikeDir, R: Reader + BufRead + Seek + 'a> {
    dir: &'a D,
    hash: XfsDahash,
    leaf: Dir2LeafNDisk,
    leaf_range: Range<usize>,
    brrc: &'a RefCell<&'a mut R>,
}

impl<'a, D: NodeLikeDir, R: Reader + BufRead + Seek + 'a> NodeLikeAddressIterator<'a, D, R> {
    pub fn new(dir: &'a D, brrc: &'a RefCell<&'a mut R>, hash: XfsDahash) -> Result<Self, i32>
    {
        let sb = SUPERBLOCK.get().unwrap();
        let dblock = sb.get_dir3_leaf_offset();
        let mut buf_reader = brrc.borrow_mut();
        let leaf_btree = {
            let raw = dir.read_dblock(buf_reader.by_ref(), sb, dblock)?;
            Leaf::open(raw.deref())
        };
        let leaf = leaf_btree.lookup_leaf_blk(buf_reader.by_ref(), sb, dir, hash)?;

        let leaf_range = leaf.get_address_range(hash);

        Ok(Self{dir, hash, leaf, leaf_range, brrc})
    }
}

impl<'a, D: NodeLikeDir, R: Reader + BufRead + Seek + 'a> Iterator for NodeLikeAddressIterator<'a, D, R> {
    type Item = XfsDir2Dataptr;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.leaf_range.is_empty() {
                if self.leaf.ents.last().map(|e| e.hashval == self.hash).unwrap_or(false) {
                    // There was a probably hash collision in the directory.  This happens
                    // frequently, since the hash is only 32 bits.  Tragically, the colliding
                    // entries were located in different leaf blocks.
                    // Traverse the forw pointer
                    let forw = self.leaf.forw;
                    let mut buf_reader = self.brrc.borrow_mut();
                    let sb = SUPERBLOCK.get().unwrap();
                    let raw = match self.dir.read_dblock(buf_reader.by_ref(), sb, forw) {
                        Ok(raw) => raw,
                        Err(e) => {
                            // It would be nice to print inode number here
                            error!("Cannot read dblock {}: {}", forw, e);
                            return None;
                        }
                    };
                    self.leaf = decode(raw.deref()).unwrap().0;
                    self.leaf_range = self.leaf.get_address_range(self.hash);
                } else {
                    return None;
                }
            } else {
                let i = self.leaf_range.start;
                self.leaf_range.start += 1;
                let ent = self.leaf.ents[i];
                debug_assert_eq!(ent.hashval, self.hash);
                return Some(ent.address << 3)
            }
        }
    }
}

/// Directories whose Leaf information takes up more than one block.
pub trait NodeLikeDir: Dir3 {
    fn get_addresses<'a, R>(&'a self, brrc: &'a RefCell<&'a mut R>, hash: XfsDahash)
        -> Box<dyn Iterator<Item=XfsDir2Dataptr> + 'a>
            where R: Reader + BufRead + Seek + 'a, Self: Sized
    {
        if let Ok(ai) = NodeLikeAddressIterator::new(self, brrc, hash) {
            Box::new(ai)
        } else {
            Box::new(std::iter::empty())
        }
    }

    fn map_dblock<R: Reader + BufRead + Seek>(
        &self,
        buf_reader: &mut R,
        logical_block: XfsDablk,
    ) -> Result<XfsFsblock, i32>;
}

#[enum_dispatch::enum_dispatch]
pub trait Dir3 {
    fn get_addresses<'a, R>(&'a self, _buf_reader: &'a RefCell<&'a mut R>, _hash: XfsDahash)
        -> Box<dyn Iterator<Item=XfsDir2Dataptr> + 'a>
            where R: Reader + BufRead + Seek + 'a
    {
        unimplemented!()
    }


    fn lookup<R: Reader + BufRead + Seek>(
        &self,
        buf_reader: &mut R,
        sb: &Sb,
        name: &OsStr,
    ) -> Result<u64, c_int> {
        let hash = hashname(name);

        let brrc = RefCell::new(buf_reader);
        for address in self.get_addresses(&brrc, hash) {
            let blk_offset = (address & ((1u32 << (sb.sb_dirblklog + sb.sb_blocklog)) - 1)) as usize;
            let dblock = address >> sb.sb_blocklog & !((1u32 << sb.sb_dirblklog) - 1);
            let mut guard = brrc.borrow_mut();
            let raw = self.read_dblock(guard.by_ref(), sb, dblock)?;
            let entry: Dir2DataEntry = decode(&raw[blk_offset..]).unwrap().0;
            if entry.name == name {
                return Ok(entry.inumber);
            }
        }
        Err(libc::ENOENT)
    }

    // Ideally this method would use RPIT syntax.  But that doesn't work with the current version
    // of enum_dispatch.
    // https://gitlab.com/antonok/enum_dispatch/-/issues/75
    // TODO: Try to eliminate the box by moving this method out of this trait.
    fn read_dblock<'a, R>(&'a self, _buf_reader: R, _sb: &Sb, _dblock: XfsDablk)
        -> Result<Box<dyn Deref<Target=[u8]> + 'a>, i32>
        where R: Reader + BufRead + Seek
    {
        unimplemented!();
    }

    /// Read the next dirent from a Directory
    fn next<R: Reader + BufRead + Seek>(
        &self,
        buf_reader: &mut R,
        sb: &Sb,
        offset: i64,
    ) -> Result<(XfsIno, i64, Option<FileType>, OsString), c_int> {
        let dblksize: u64 = 1 << (sb.sb_blocklog + sb.sb_dirblklog);
        let dblkmask: u64 = dblksize - 1;
        let mut offset: u64 = offset.try_into().unwrap();
        let mut next = offset == 0;

        loop {
            // Byte offset within this directory block
            let dir_block_offset = offset & ((1 << (sb.sb_dirblklog + sb.sb_blocklog)) - 1);
            // Offset of this directory block within the directory
            let doffset = offset - dir_block_offset;

            let dblock = (offset >> sb.sb_blocklog & !((1u64 << sb.sb_dirblklog) - 1)).try_into().unwrap();
            let raw = self.read_dblock(buf_reader.by_ref(), sb, dblock)?;

            let mut blk_offset = if offset & dblkmask > 0 {
                (offset & dblkmask) as usize
            } else {
                let magic: u32 = decode(&raw[..]).unwrap().0;
                match magic {
                    XFS_DIR2_BLOCK_MAGIC | XFS_DIR2_DATA_MAGIC => Dir2DataHdr::SIZE as usize,
                    XFS_DIR3_BLOCK_MAGIC | XFS_DIR3_DATA_MAGIC => Dir3DataHdr::SIZE as usize,
                    _ => panic!("Unknown magic number for block directory {:#x}", magic)
                }
            };
            while blk_offset < dblksize as usize {
                if blk_offset >= raw.len() {
                    // We reached the end of the provided buffer before reaching the end of the
                    // directory block.  This should only be possible for Block directories.
                    return Err(ENOENT);
                }
                let freetag: u16 = decode(&raw[blk_offset..]).unwrap().0;
                if freetag == 0xffff {
                    let (_, length) = decode::<Dir2DataUnused>(&raw[blk_offset..])
                        .unwrap();
                    offset += length as u64;
                    blk_offset += length;
                } else if !next {
                    let length = Dir2DataEntry::get_length(sb, &raw[blk_offset..]);
                    blk_offset += length as usize;
                    offset += length as u64;
                    next = true;
                } else {
                    let (entry, _l)= decode::<Dir2DataEntry >(&raw[blk_offset..]).unwrap();
                    let kind = match entry.ftype {
                        Some(ftype) => Some(get_file_type(FileKind::Type(ftype))?),
                        None => None
                    };
                    let name = entry.name;
                    let entry_offset = doffset + entry.tag as u64;
                    return Ok((entry.inumber, entry_offset as i64, kind, name));
                }
            }
        }
    }
}

#[derive(Debug)]
#[enum_dispatch::enum_dispatch(Dir3)]
pub enum Directory {
    Sf(super::dir3_sf::Dir2Sf),
    Block(super::dir3_block::Dir2Block),
    Leaf(super::dir3_leaf::Dir2Leaf),
    Node(super::dir3_node::Dir2Node),
    Btree(super::dir3_bptree::Dir2Btree),
}
