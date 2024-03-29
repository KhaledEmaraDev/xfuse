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
use std::{
    io::{prelude::*, SeekFrom},
    mem,
};

use bincode::{
    Decode,
    de::{Decoder, read::Reader},
    error::DecodeError
};
use num_traits::{PrimInt, Unsigned};
use super::utils::Uuid;

use super::{
    bmbt_rec::BmbtRec,
    definitions::{XfsFileoff, XfsFsblock},
    utils::decode_from,
    volume::SUPERBLOCK
};

#[derive(Clone, Copy, Debug, Decode)]
pub struct BtreeBlockHdr<T: PrimInt + Unsigned> {
    _bb_magic: u32,
    pub bb_level: u16,
    pub bb_numrecs: u16,
    _bb_leftsib: T,
    _bb_rightsib: T,
    _bb_blkno: u64,
    _bb_lsn: u64,
    pub bb_uuid: Uuid,
    _bb_owner: u64,
    _bb_crc: u32,
    _bb_pad: u32,
}

impl<T: PrimInt + Unsigned> BtreeBlockHdr<T> {
    pub const SIZE: usize = 56 + 2 * mem::size_of::<T>();
}

#[derive(Debug, Clone, Decode)]
pub struct BmdrBlock {
    pub bb_level: u16,
    pub bb_numrecs: u16,
}

impl BmdrBlock {
    pub const SIZE: usize = 4;
}

#[derive(Debug, Clone, Decode)]
pub struct BmbtKey {
    pub br_startoff: XfsFileoff,
}

impl BmbtKey {
    pub const SIZE: usize = 8;
}

pub type XfsBmbtPtr = XfsFsblock;
pub type XfsBmdrPtr = XfsFsblock;
pub type XfsBmbtLblock = BtreeBlockHdr<u64>;

/// Methods that are common to both BtreeRoot and BtreeIntermediate
pub trait Btree {
    fn keys(&self) -> &[BmbtKey];
    fn level(&self) -> u16;

    /// Return the extent, if any, that contains the given block within the file.
    /// Return its starting position as an FSblock, and its length in file system block units.
    /// If the length extents to EoF, return None for length.
    fn map_block<R: bincode::de::read::Reader + BufRead + Seek>(
        &self,
        buf_reader: &mut R,
        logical_block: XfsFileoff,
    ) -> Result<(Option<XfsFsblock>, Option<u64>), i32> {
        let super_block = SUPERBLOCK.get().unwrap();
        let pp = self.keys().partition_point(|k| k.br_startoff <= logical_block);
        // If there's a hole at the start, we should still descend into the leftmost child.
        // BtreeLeaf::get_extent will calculate the hole's size.
        let idx = pp.saturating_sub(1);

        let offset = super_block.fsb_to_offset(self.ptrs()[idx]);
        buf_reader
            .seek(SeekFrom::Start(offset))
            .map_err(|e| e.raw_os_error().unwrap())?;

        if self.level() > 1 {
            let bti: BtreeIntermediate = decode_from(buf_reader.by_ref()).map_err(|_| libc::EDESTADDRREQ)?;
            assert_eq!(bti.hdr.bb_uuid, super_block.sb_uuid);
            bti.map_block(buf_reader, logical_block)
        } else {
            let btl: BtreeLeaf = decode_from(buf_reader.by_ref()).map_err(|_| libc::ENOTSOCK)?;
            assert_eq!(btl.hdr.bb_uuid, super_block.sb_uuid);
            Ok(btl.get_extent(logical_block))
        }
    }

    fn ptrs(&self) -> &[XfsBmbtPtr];
}

/// A root BTree in an extent list.
///
/// This is actually part of the inode, not a separate disk block.  Note that root and intermediate
/// nodes are stored differently on disk: root btrees are stored in the inode, whereas intermediate
/// btrees are stored in a BMA3 block.
#[derive(Debug, Clone)]
pub struct BtreeRoot {
    pub bmdr: BmdrBlock,
    pub keys: Vec<BmbtKey>,
    pub ptrs: Vec<XfsBmdrPtr>,
}

impl BtreeRoot {
    pub fn new(bmdr: BmdrBlock, keys: Vec<BmbtKey>, ptrs: Vec<XfsBmdrPtr>) -> Self {
        Self {bmdr, keys, ptrs}
    }
}

impl Btree for BtreeRoot {
    fn keys(&self) -> &[BmbtKey] {
        &self.keys
    }

    fn level(&self) -> u16 {
        self.bmdr.bb_level
    }

    fn ptrs(&self) -> &[XfsBmdrPtr] {
        &self.ptrs
    }
}

/// An intermediate Btree.
#[derive(Debug)]
struct BtreeIntermediate {
    hdr: XfsBmbtLblock,
    keys: Vec<BmbtKey>,
    ptrs: Vec<XfsBmbtPtr>,
}

impl Btree for BtreeIntermediate {
    fn keys(&self) -> &[BmbtKey] {
        &self.keys
    }

    fn level(&self) -> u16 {
        self.hdr.bb_level
    }

    fn ptrs(&self) -> &[XfsBmbtPtr] {
        &self.ptrs
    }
}


impl Decode for BtreeIntermediate {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let hdr: XfsBmbtLblock = Decode::decode(decoder)?;
        assert!(hdr.bb_level > 0);

        let keys = (0..hdr.bb_numrecs).map(|_| {
            Decode::decode(decoder).unwrap()
        }).collect::<Vec<BmbtKey>>();

        // Now skip ahead to the pointers.  The XFS Algorithms & Data Structures document section
        // 16.2 says that they start at offset 0x808 within the block.  But it looks to me like
        // they really start at offset 0x820.
        let read_so_far = XfsBmbtLblock::SIZE + usize::from(hdr.bb_numrecs) * BmbtKey::SIZE;
        let blocksize = SUPERBLOCK.get().unwrap().sb_blocksize;
        let pointers_area_offset = blocksize as usize / 2 + 0x20;
        decoder.reader().consume(pointers_area_offset - read_so_far);

        let ptrs = (0..hdr.bb_numrecs).map(|_| {
            Decode::decode(decoder).unwrap()
        }).collect::<Vec<XfsBmbtPtr>>();

        Ok(Self {
            hdr,
            keys,
            ptrs
        })
    }
}

/// A Leaf Btree.
#[derive(Debug)]
struct BtreeLeaf {
    hdr: XfsBmbtLblock,
    recs: Vec<BmbtRec>,
}

impl BtreeLeaf {
    /// Return the extent, if any, that contains the given block within the file.
    /// Return its starting position as an FSblock, and its length in file system block units.
    /// If the length extents to EoF, return None for length.
    pub fn get_extent(&self, dblock: XfsFileoff) -> (Option<XfsFsblock>, Option<u64>) {
        match self.recs.partition_point(|entry| entry.br_startoff <= dblock) {
            0 => {
                // A hole at the beginning of the file
                let len = self.recs.first()
                    .map(|b| b.br_startoff - dblock);
                (None, len)
            },
            i => {
                let entry = &self.recs[i - 1];
                let skip = dblock - entry.br_startoff;
                if entry.br_startoff + entry.br_blockcount > dblock {
                    (Some(entry.br_startblock + skip), Some(entry.br_blockcount - skip))
                } else {
                    // It's a hole
                    let len = self.recs.get(i)
                        .map(|e| e.br_startoff - entry.br_startblock - skip);
                    (None, len)
                }
            }
        }
    }
}

impl Decode for BtreeLeaf {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let hdr: XfsBmbtLblock = Decode::decode(decoder)?;
        assert_eq!(hdr.bb_level, 0);

        let recs = (0..hdr.bb_numrecs).map(|_| {
            Decode::decode(decoder).unwrap()
        }).collect::<Vec<BmbtRec>>();

        Ok(Self {
            hdr,
            recs
        })
    }
}
