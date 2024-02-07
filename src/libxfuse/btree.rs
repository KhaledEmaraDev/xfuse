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
use byteorder::{BigEndian, LittleEndian, ReadBytesExt};
use num_traits::{PrimInt, Unsigned};
use super::utils::Uuid;

use super::{
    bmbt_rec::BmbtRec,
    definitions::{XfsFileoff, XfsFsblock},
    sb::Sb,
    utils::decode_from
};

pub const POINTERS_AREA_OFFSET: u16 = 0x808;

#[derive(Clone, Copy, Debug, Decode)]
pub struct BtreeBlockHdr<T: PrimInt + Unsigned> {
    pub bb_magic: u32,
    pub bb_level: u16,
    pub bb_numrecs: u16,
    pub bb_leftsib: T,
    pub bb_rightsib: T,
    pub bb_blkno: u64,
    pub bb_lsn: u64,
    pub bb_uuid: Uuid,
    pub bb_owner: u64,
    pub bb_crc: u32,
    pub bb_pad: u32,
}

impl<T: PrimInt + Unsigned> BtreeBlockHdr<T> {
    pub const SIZE: usize = 56 + 2 * mem::size_of::<T>();

    pub fn from<R: BufRead + Seek>(buf_reader: &mut R, super_block: &Sb) -> Self {
        let bb_magic = buf_reader.read_u32::<BigEndian>().unwrap();
        let bb_level = buf_reader.read_u16::<BigEndian>().unwrap();
        let bb_numrecs = buf_reader.read_u16::<BigEndian>().unwrap();

        let type_size = mem::size_of::<T>();
        let bb_leftsib = T::from(buf_reader.read_uint::<BigEndian>(type_size).unwrap()).unwrap();
        let bb_rightsib = T::from(buf_reader.read_uint::<BigEndian>(type_size).unwrap()).unwrap();

        let bb_blkno = buf_reader.read_u64::<BigEndian>().unwrap();
        let bb_lsn = buf_reader.read_u64::<BigEndian>().unwrap();
        let bb_uuid = Uuid::from_u128(buf_reader.read_u128::<BigEndian>().unwrap());
        let bb_owner = buf_reader.read_u64::<BigEndian>().unwrap();
        let bb_crc = buf_reader.read_u32::<LittleEndian>().unwrap();
        let bb_pad = buf_reader.read_u32::<BigEndian>().unwrap();

        if bb_uuid != super_block.sb_uuid {
            panic!("UUID mismatch!");
        }

        Self {
            bb_magic,
            bb_level,
            bb_numrecs,
            bb_leftsib,
            bb_rightsib,
            bb_blkno,
            bb_lsn,
            bb_uuid,
            bb_owner,
            bb_crc,
            bb_pad,
        }
    }
}

#[derive(Debug, Clone, Decode)]
pub struct BmdrBlock {
    pub bb_level: u16,
    pub bb_numrecs: u16,
}

impl BmdrBlock {
    pub const SIZE: usize = 4;

    pub fn from<R: BufRead>(buf_reader: &mut R) -> BmdrBlock {
        let bb_level = buf_reader.read_u16::<BigEndian>().unwrap();
        let bb_numrecs = buf_reader.read_u16::<BigEndian>().unwrap();

        BmdrBlock {
            bb_level,
            bb_numrecs,
        }
    }
}

#[derive(Debug, Clone, Decode)]
pub struct BmbtKey {
    pub br_startoff: XfsFileoff,
}

impl BmbtKey {
    pub const SIZE: usize = 8;

    pub fn from<R: BufRead>(buf_reader: &mut R) -> BmbtKey {
        let br_startoff = buf_reader.read_u64::<BigEndian>().unwrap();

        BmbtKey { br_startoff }
    }
}

pub type XfsBmbtPtr = XfsFsblock;
pub type XfsBmdrPtr = XfsFsblock;
pub type XfsBmbtLblock = BtreeBlockHdr<u64>;

/// Methods that are common to both BtreeRoot and BtreeIntermediate
pub trait Btree {
    fn keys(&self) -> &[BmbtKey];
    fn level(&self) -> u16;

    fn map_block<R: bincode::de::read::Reader + BufRead + Seek>(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        logical_block: XfsFileoff,
    ) -> XfsFsblock {
        let idx = self.keys().partition_point(|k| k.br_startoff <= logical_block) - 1;
        buf_reader
            .seek(SeekFrom::Start(
                self.ptrs()[idx] * u64::from(super_block.sb_blocksize),
            ))
            .unwrap();

        if self.level() > 1 {
            let bti: BtreeIntermediate = decode_from(buf_reader.by_ref()).unwrap();
            assert_eq!(bti.hdr.bb_uuid, super_block.sb_uuid);
            bti.map_block(buf_reader, super_block, logical_block)
        } else {
            let btl: BtreeLeaf = decode_from(buf_reader.by_ref()).unwrap();
            assert_eq!(btl.hdr.bb_uuid, super_block.sb_uuid);
            btl.map_block(logical_block)
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
    pub ptrs: Vec<XfsBmbtPtr>,
}

impl Btree for BtreeRoot {
    fn keys(&self) -> &[BmbtKey] {
        &self.keys
    }

    fn level(&self) -> u16 {
        self.bmdr.bb_level
    }

    fn ptrs(&self) -> &[XfsBmbtPtr] {
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
        decoder.reader().consume(0x820 - read_so_far);

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
    pub fn map_block( &self, logical_block: XfsFileoff,) -> XfsFsblock {
        let i = self.recs.partition_point(|k| k.br_startoff <= logical_block) - 1;
        let rec = &self.recs[i];

        rec.br_startblock + (logical_block - rec.br_startoff)
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
