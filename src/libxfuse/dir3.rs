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
use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, Seek, SeekFrom};
use std::mem;
use std::os::unix::ffi::OsStringExt;

use super::da_btree::XfsDa3Blkinfo;
use super::definitions::*;
use super::sb::Sb;

use byteorder::{BigEndian, ReadBytesExt};
use fuser::{FileAttr, FileType};
use libc::{c_int, ENOENT};
use uuid::Uuid;

pub type XfsDir2DataOff = u16;
pub type XfsDir2Dataptr = u32;

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

#[derive(Debug)]
pub struct Dir3BlkHdr {
    pub magic: u32,
    pub crc: u32,
    pub blkno: u64,
    pub lsn: u64,
    pub uuid: Uuid,
    pub owner: u64,
}

impl Dir3BlkHdr {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir3BlkHdr {
        let magic = buf_reader.read_u32::<BigEndian>().unwrap();
        let crc = buf_reader.read_u32::<BigEndian>().unwrap();
        let blkno = buf_reader.read_u64::<BigEndian>().unwrap();
        let lsn = buf_reader.read_u64::<BigEndian>().unwrap();
        let uuid = Uuid::from_u128(buf_reader.read_u128::<BigEndian>().unwrap());
        let owner = buf_reader.read_u64::<BigEndian>().unwrap();

        Dir3BlkHdr {
            magic,
            crc,
            blkno,
            lsn,
            uuid,
            owner,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Dir2DataFree {
    pub offset: XfsDir2DataOff,
    pub length: XfsDir2DataOff,
}

impl Dir2DataFree {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir2DataFree {
        let offset = buf_reader.read_u16::<BigEndian>().unwrap();
        let length = buf_reader.read_u16::<BigEndian>().unwrap();

        Dir2DataFree { offset, length }
    }
}

#[derive(Debug)]
pub struct Dir3DataHdr {
    pub hdr: Dir3BlkHdr,
    pub best_free: [Dir2DataFree; XFS_DIR2_DATA_FD_COUNT],
    pub pad: u32,
}

impl Dir3DataHdr {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir3DataHdr {
        let hdr = Dir3BlkHdr::from(buf_reader.by_ref());

        let mut best_free = [Dir2DataFree {
            offset: 0,
            length: 0,
        }; XFS_DIR2_DATA_FD_COUNT];
        for entry in best_free.iter_mut().take(XFS_DIR2_DATA_FD_COUNT) {
            *entry = Dir2DataFree::from(buf_reader.by_ref());
        }

        let pad = buf_reader.read_u32::<BigEndian>().unwrap();

        Dir3DataHdr {
            hdr,
            best_free,
            pad,
        }
    }
}

#[derive(Debug)]
pub struct Dir2DataEntry {
    pub inumber: XfsIno,
    pub namelen: u8,
    pub name: OsString,
    pub ftype: u8,
    pub tag: XfsDir2DataOff,
}

impl Dir2DataEntry {
    pub fn from<T: BufRead + Seek>(buf_reader: &mut T) -> Dir2DataEntry {
        let inumber = buf_reader.read_u64::<BigEndian>().unwrap();
        let namelen = buf_reader.read_u8().unwrap();

        let mut namebytes = vec![0u8; namelen.into()];
        buf_reader.read_exact(&mut namebytes).unwrap();
        let name = OsString::from_vec(namebytes);

        let ftype = buf_reader.read_u8().unwrap();

        let pad_off = (((buf_reader.stream_position().unwrap() + 2 + 8 - 1) / 8) * 8)
            - (buf_reader.stream_position().unwrap() + 2);
        buf_reader.seek(SeekFrom::Current(pad_off as i64)).unwrap();

        let tag = buf_reader.read_u16::<BigEndian>().unwrap();

        Dir2DataEntry {
            inumber,
            namelen,
            name,
            ftype,
            tag,
        }
    }

    pub fn get_length<T: BufRead + Seek>(buf_reader: &mut T) -> i64 {
        buf_reader.seek(SeekFrom::Current(8)).unwrap();
        let namelen = buf_reader.read_u8().unwrap();
        buf_reader.seek(SeekFrom::Current(-9)).unwrap();

        ((((namelen as i64) + 8 + 1 + 2) + 8 - 1) / 8) * 8
    }
}

#[derive(Debug)]
pub struct Dir2DataUnused {
    pub freetag: u16,
    pub length: XfsDir2DataOff,
    pub tag: XfsDir2DataOff,
}

impl Dir2DataUnused {
    pub fn from<T: BufRead + Seek>(buf_reader: &mut T) -> Dir2DataUnused {
        let freetag = buf_reader.read_u16::<BigEndian>().unwrap();
        let length = buf_reader.read_u16::<BigEndian>().unwrap();

        buf_reader
            .seek(SeekFrom::Current((length - 6) as i64))
            .unwrap();

        let tag = buf_reader.read_u16::<BigEndian>().unwrap();

        Dir2DataUnused {
            freetag,
            length,
            tag,
        }
    }
}

#[derive(Debug)]
pub enum Dir2DataUnion {
    Entry(Dir2DataEntry),
    Unused(Dir2DataUnused),
}

#[derive(Debug)]
pub struct Dir2Data {
    pub hdr: Dir3DataHdr,

    pub offset: u64,
}

impl Dir2Data {
    pub fn from<T: BufRead + Seek>(
        buf_reader: &mut T,
        superblock: &Sb,
        start_block: u64,
    ) -> Dir2Data {
        let offset = start_block * (superblock.sb_blocksize as u64);
        buf_reader.seek(SeekFrom::Start(offset)).unwrap();

        let hdr = Dir3DataHdr::from(buf_reader.by_ref());

        Dir2Data { hdr, offset }
    }
}

#[derive(Debug)]
pub struct Dir3LeafHdr {
    pub info: XfsDa3Blkinfo,
    pub count: u16,
    pub stale: u16,
    pub pad: u32,
}

impl Dir3LeafHdr {
    pub fn from<T: BufRead + Seek>(buf_reader: &mut T, super_block: &Sb) -> Dir3LeafHdr {
        let info = XfsDa3Blkinfo::from(buf_reader, super_block);
        let count = buf_reader.read_u16::<BigEndian>().unwrap();
        let stale = buf_reader.read_u16::<BigEndian>().unwrap();
        let pad = buf_reader.read_u32::<BigEndian>().unwrap();

        Dir3LeafHdr {
            info,
            count,
            stale,
            pad,
        }
    }
}

#[derive(Debug)]
pub struct Dir2LeafEntry {
    pub hashval: XfsDahash,
    pub address: XfsDir2Dataptr,
}

impl Dir2LeafEntry {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir2LeafEntry {
        let hashval = buf_reader.read_u32::<BigEndian>().unwrap();
        let address = buf_reader.read_u32::<BigEndian>().unwrap();

        Dir2LeafEntry { hashval, address }
    }
}

#[derive(Debug)]
pub struct Dir2LeafTail {
    pub bestcount: u32,
}

impl Dir2LeafTail {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir2LeafTail {
        let bestcount = buf_reader.read_u32::<BigEndian>().unwrap();

        Dir2LeafTail { bestcount }
    }
}

#[derive(Debug)]
pub struct Dir2LeafNDisk {
    pub hdr: Dir3LeafHdr,
    pub ents: Vec<Dir2LeafEntry>,
}

impl Dir2LeafNDisk {
    pub fn from<T: BufRead + Seek>(buf_reader: &mut T, super_block: &Sb) -> Dir2LeafNDisk {
        let hdr = Dir3LeafHdr::from(buf_reader.by_ref(), super_block);

        let mut ents = Vec::<Dir2LeafEntry>::new();
        for _i in 0..hdr.count {
            let leaf_entry = Dir2LeafEntry::from(buf_reader.by_ref());
            ents.push(leaf_entry);
        }

        Dir2LeafNDisk { hdr, ents }
    }

    pub fn get_address(&self, hash: XfsDahash) -> Result<XfsDir2Dataptr, c_int> {
        let mut low: i64 = 0;
        let mut high: i64 = (self.ents.len() - 1) as i64;

        while low <= high {
            let mid = low + ((high - low) / 2);

            let entry = &self.ents[mid as usize];

            match entry.hashval.cmp(&hash) {
                Ordering::Greater => {
                    high = mid - 1;
                }
                Ordering::Less => {
                    low = mid + 1;
                }
                Ordering::Equal => return Ok(entry.address),
            }
        }

        Err(ENOENT)
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
        super_block: &Sb,
        offset: u64,
        size: u32,
    ) -> Dir2LeafDisk {
        buf_reader.seek(SeekFrom::Start(offset)).unwrap();

        let hdr = Dir3LeafHdr::from(buf_reader.by_ref(), super_block);

        let mut ents = Vec::<Dir2LeafEntry>::new();
        for _i in 0..hdr.count {
            let leaf_entry = Dir2LeafEntry::from(buf_reader.by_ref());
            ents.push(leaf_entry);
        }

        buf_reader
            .seek(SeekFrom::Start(
                offset + (size as u64) - (mem::size_of::<Dir2LeafTail>() as u64),
            ))
            .unwrap();

        let tail = Dir2LeafTail::from(buf_reader.by_ref());

        let data_end = offset + (size as u64)
            - (mem::size_of::<Dir2LeafTail>() as u64)
            - ((mem::size_of::<XfsDir2DataOff>() as u64) * (tail.bestcount as u64));
        buf_reader.seek(SeekFrom::Start(data_end)).unwrap();

        let mut bests = Vec::<XfsDir2DataOff>::new();
        for _i in 0..tail.bestcount {
            bests.push(buf_reader.read_u16::<BigEndian>().unwrap());
        }

        Dir2LeafDisk {
            hdr,
            ents,
            bests,
            tail,
        }
    }

    pub fn get_address(&self, hash: XfsDahash) -> Result<XfsDir2Dataptr, c_int> {
        let mut low: i64 = 0;
        let mut high: i64 = (self.ents.len() - 1) as i64;

        while low <= high {
            let mid = low + ((high - low) / 2);

            let entry = &self.ents[mid as usize];

            match entry.hashval.cmp(&hash) {
                Ordering::Greater => {
                    high = mid - 1;
                }
                Ordering::Less => {
                    low = mid + 1;
                }
                Ordering::Equal => return Ok(entry.address),
            }
        }

        Err(ENOENT)
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
