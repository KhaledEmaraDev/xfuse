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
use std::convert::TryInto;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, Seek, SeekFrom};
use std::mem;
use std::os::unix::ffi::OsStringExt;

use super::da_btree::XfsDa3Blkinfo;
use super::definitions::*;
use super::sb::Sb;
use super::utils::{Uuid, decode, decode_from};

use bincode::{
    Decode,
    de::{Decoder, read::Reader},
    error::DecodeError
};
use fuser::{FileAttr, FileType};
use libc::{c_int, ENOENT};

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
    pub crc: u32,
    pub blkno: u64,
    pub lsn: u64,
    pub uuid: Uuid,
    pub owner: u64,
}

impl Dir3BlkHdr {
    pub const SIZE: u64 = 48;
}

#[derive(Debug, Decode, Clone, Copy)]
pub struct Dir2DataFree {
    pub offset: XfsDir2DataOff,
    pub length: XfsDir2DataOff,
}

impl Dir2DataFree {
    pub const SIZE: u64 = 4;
}

#[derive(Debug, Decode)]
pub struct Dir3DataHdr {
    pub hdr: Dir3BlkHdr,
    pub best_free: [Dir2DataFree; constants::XFS_DIR2_DATA_FD_COUNT],
    pub pad: u32,
}

impl Dir3DataHdr {
    pub const SIZE: u64 = Dir3BlkHdr::SIZE + constants::XFS_DIR2_DATA_FD_COUNT as u64 * Dir2DataFree::SIZE + 4;
}

#[derive(Debug)]
pub struct Dir2DataEntry {
    pub inumber: XfsIno,
    pub name: OsString,
    pub ftype: u8,
    pub tag: XfsDir2DataOff,
}

impl Dir2DataEntry {
    pub fn get_length(raw: &[u8]) -> i64 {
        let namelen: u8 = decode(&raw[8..]).unwrap().0;
        ((((namelen as i64) + 8 + 1 + 2) + 8 - 1) / 8) * 8
    }
}

impl Decode for Dir2DataEntry {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let inumber = Decode::decode(decoder)?;
        let namelen: u8 = Decode::decode(decoder)?;
        let mut namebytes = vec![0u8; namelen.into()];
        decoder.reader().read(&mut namebytes[..])?;
        let name = OsString::from_vec(namebytes);
        let ftype = Decode::decode(decoder)?;
        // Pad up to 2 less than a multiple of 8 bytes
        // current offset is 8 + 1 + namelen + 1
        let pad: usize = (4 - namelen as i8).rem_euclid(8).try_into().unwrap();
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
    pub freetag: u16,
    pub length: XfsDir2DataOff,
    pub tag: XfsDir2DataOff,
}

impl From<&[u8]> for Dir2DataUnused {
    fn from(raw: &[u8]) -> Self {
        let freetag = decode(raw).unwrap().0;
        let length = decode(&raw[2..]).unwrap().0;
        let tag  = decode(&raw[(length - 2) as usize..]).unwrap().0;

        Dir2DataUnused {
            freetag,
            length,
            tag,
        }
    }
}

impl Decode for Dir2DataUnused {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let freetag = Decode::decode(decoder)?;
        let length = Decode::decode(decoder)?;
        decoder.reader().consume(length as usize - 6);
        let tag = Decode::decode(decoder)?;
        Ok(Dir2DataUnused {
            freetag,
            length,
            tag,
        })
    }
}

#[derive(Debug)]
pub struct Dir2Data {
    pub hdr: Dir3DataHdr,

    pub offset: u64,
}

impl Dir2Data {
    pub fn from<T: bincode::de::read::Reader + BufRead + Seek>(
        buf_reader: &mut T,
        superblock: &Sb,
        start_block: u64,
    ) -> Dir2Data {
        let offset = superblock.fsb_to_offset(start_block);
        buf_reader.seek(SeekFrom::Start(offset)).unwrap();

        let hdr = decode_from(buf_reader.by_ref()).unwrap();

        Dir2Data { hdr, offset }
    }
}

#[derive(Debug, Decode)]
pub struct Dir3LeafHdr {
    pub info: XfsDa3Blkinfo,
    pub count: u16,
    pub stale: u16,
    pub pad: u32,
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

#[derive(Debug, Decode)]
pub struct Dir2LeafTail {
    pub bestcount: u32,
}

impl Dir2LeafTail {
    pub const SIZE: usize = 4;
}

#[derive(Debug)]
pub struct Dir2LeafNDisk {
    pub hdr: Dir3LeafHdr,
    pub ents: Vec<Dir2LeafEntry>,
}

impl Dir2LeafNDisk {
    pub fn get_address(
        &self,
        hash: XfsDahash,
        collision_resolver: usize) -> Result<XfsDir2Dataptr, c_int>
    {
        let i = self.ents.partition_point(|ent| ent.hashval < hash);
        let ent = self.ents.get(i + collision_resolver).ok_or(ENOENT)?;
        if ent.hashval == hash {
            Ok(ent.address)
        } else {
            Err(ENOENT)
        }
    }
}

impl Decode for Dir2LeafNDisk {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let hdr: Dir3LeafHdr = Decode::decode(decoder)?;
        let mut ents = Vec::<Dir2LeafEntry>::new();
        for _i in 0..hdr.count {
            let leaf_entry: Dir2LeafEntry = Decode::decode(decoder)?;
            ents.push(leaf_entry);
        }
        assert_eq!(hdr.info.magic, XFS_DIR3_LEAFN_MAGIC,
            "bad magic! expected {:#x} but found {:#x}", XFS_DIR3_LEAFN_MAGIC, hdr.info.magic);

        Ok(Dir2LeafNDisk { hdr, ents })
    }
}

#[derive(Debug)]
pub struct Dir2LeafDisk {
    pub hdr: Dir3LeafHdr,
    pub ents: Vec<Dir2LeafEntry>,
    pub bests: Vec<XfsDir2DataOff>,
    pub tail: Dir2LeafTail,
}

impl Dir2LeafDisk {
    pub fn from<T: BufRead + Seek>(
        buf_reader: &mut T,
        offset: u64,
        size: usize,
    ) -> Dir2LeafDisk {
        buf_reader.seek(SeekFrom::Start(offset)).unwrap();
        let mut raw = vec![0u8; size];
        buf_reader.read_exact(&mut raw).unwrap();
        let config = bincode::config::standard()
            .with_big_endian()
            .with_fixed_int_encoding();
        let reader = bincode::de::read::SliceReader::new(&raw[..]);
        let mut decoder = bincode::de::DecoderImpl::new(reader, config);
        let hdr = Dir3LeafHdr::decode(&mut decoder).unwrap();
        assert_eq!(hdr.info.magic, XFS_DIR3_LEAF1_MAGIC,
            "bad magic! expected {:#x} but found {:#x}", XFS_DIR3_LEAF1_MAGIC, hdr.info.magic);

        let ents = (0..hdr.count).map(|_| {
            Dir2LeafEntry::decode(&mut decoder).unwrap()
        }).collect::<Vec<_>>();

        // bests and tail grow from the end of the block. And, annoyingly, the
        // length of bests is stored in tail, so we must read tail first.
        let tail: Dir2LeafTail = decode(&raw[raw.len() - 4..]).unwrap().0;

        let bests_size = mem::size_of::<XfsDir2DataOff>() * tail.bestcount as usize;
        let bests_start = size - Dir2LeafTail::SIZE - bests_size;
        let reader = bincode::de::read::SliceReader::new(&raw[bests_start..]);
        let mut decoder = bincode::de::DecoderImpl::new(reader, config);

        let bests = (0..tail.bestcount).map(|_| {
            XfsDir2DataOff::decode(&mut decoder).unwrap()
        }).collect::<Vec<_>>();

        Dir2LeafDisk {
            hdr,
            ents,
            bests,
            tail,
        }
    }

    pub fn get_address(&self, hash: XfsDahash) -> Result<XfsDir2Dataptr, c_int> {
        match self.ents.binary_search_by_key(&hash, |ent| ent.hashval) {
            Ok(i) => Ok(self.ents[i].address),
            Err(_) => Err(ENOENT)
        }
    }
}

pub trait Dir3<R: BufRead + Seek> {
    fn lookup(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        name: &OsStr,
    ) -> Result<(FileAttr, u64), c_int>;

    fn next(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        offset: i64,
    ) -> Result<(XfsIno, i64, FileType, OsString), c_int>;
}
