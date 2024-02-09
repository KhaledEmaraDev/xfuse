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
    cmp::Ordering,
    ffi::OsStr,
    io::{BufRead, Seek, SeekFrom},
    os::unix::ffi::OsStrExt
};

use super::{definitions::*, sb::Sb};

use bincode::{
    Decode,
    de::Decoder,
    error::DecodeError,
    impl_borrow_decode,
};

use byteorder::{BigEndian, ReadBytesExt};
use super::utils::Uuid;

macro_rules! rol32 {
    ($x:expr, $y:expr) => {
        ((($x) << ($y)) | (($x) >> (32 - ($y))))
    };
}

pub fn hashname(name: &OsStr) -> XfsDahash {
    let name = name.as_bytes();
    let mut namelen = name.len();
    let mut hash: XfsDahash = 0;

    let mut i: usize = 0;
    while namelen >= 4 {
        hash = ((name[i] as u32) << 21)
            ^ ((name[i + 1] as u32) << 14)
            ^ ((name[i + 2] as u32) << 7)
            ^ (name[i + 3] as u32)
            ^ rol32!(hash, 7 * 4);

        namelen -= 4;
        i += 4;
    }

    match namelen {
        3 => {
            ((name[i] as u32) << 14)
                ^ ((name[i + 1] as u32) << 7)
                ^ (name[i + 2] as u32)
                ^ rol32!(hash, 7 * 3)
        }
        2 => ((name[i] as u32) << 7) ^ (name[i + 1] as u32) ^ rol32!(hash, 7 * 2),
        1 => (name[i] as u32) ^ rol32!(hash, 7),
        _ => hash,
    }
}

#[derive(Debug, Decode)]
pub struct XfsDa3Blkinfo {
    pub forw: u32,
    pub back: u32,
    pub magic: u16,
    pub pad: u16,

    pub crc: u32,
    pub blkno: u64,
    pub lsn: u64,
    pub uuid: Uuid,
    pub owner: u64,
}

impl XfsDa3Blkinfo {
    pub fn from<R: BufRead + Seek>(buf_reader: &mut R, super_block: &Sb) -> XfsDa3Blkinfo {
        let forw = buf_reader.read_u32::<BigEndian>().unwrap();
        let back = buf_reader.read_u32::<BigEndian>().unwrap();
        let magic = buf_reader.read_u16::<BigEndian>().unwrap();
        let pad = buf_reader.read_u16::<BigEndian>().unwrap();

        let crc = buf_reader.read_u32::<BigEndian>().unwrap();
        let blkno = buf_reader.read_u64::<BigEndian>().unwrap();
        let lsn = buf_reader.read_u64::<BigEndian>().unwrap();
        let uuid = Uuid::from_u128(buf_reader.read_u128::<BigEndian>().unwrap());
        let owner = buf_reader.read_u64::<BigEndian>().unwrap();

        if uuid != super_block.sb_uuid {
            panic!("UUID mismatch!");
        }

        XfsDa3Blkinfo {
            forw,
            back,
            magic,
            pad,
            crc,
            blkno,
            lsn,
            uuid,
            owner,
        }
    }

    pub fn sanity(&self, super_block: &Sb) {
        if self.uuid != super_block.sb_uuid {
            panic!("UUID mismatch!");
        }
    }
}

#[derive(Debug)]
pub struct XfsDa3NodeHdr {
    pub info: XfsDa3Blkinfo,
    pub count: u16,
    pub level: u16,
    pub pad32: u32,
}

impl XfsDa3NodeHdr {
    pub fn from<R: BufRead + Seek>(buf_reader: &mut R, super_block: &Sb) -> XfsDa3NodeHdr {
        let info = XfsDa3Blkinfo::from(buf_reader.by_ref(), super_block);
        let count = buf_reader.read_u16::<BigEndian>().unwrap();
        let level = buf_reader.read_u16::<BigEndian>().unwrap();
        let pad32 = buf_reader.read_u32::<BigEndian>().unwrap();

        XfsDa3NodeHdr {
            info,
            count,
            level,
            pad32,
        }
    }
}

impl Decode for XfsDa3NodeHdr {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let info: XfsDa3Blkinfo = Decode::decode(decoder)?;
        if info.magic != XFS_DA3_NODE_MAGIC {
            return Err(DecodeError::Other("bad magic"));
        }
        let count = Decode::decode(decoder)?;
        let level = Decode::decode(decoder)?;
        let pad32 = Decode::decode(decoder)?;
        Ok(XfsDa3NodeHdr {
            info,
            count,
            level,
            pad32
        })
    }
}
impl_borrow_decode!(XfsDa3NodeHdr);

#[derive(Debug, Decode)]
pub struct XfsDa3NodeEntry {
    pub hashval: XfsDahash,
    pub before: XfsDablk,
}

impl XfsDa3NodeEntry {
    pub fn from<R: BufRead>(buf_reader: &mut R) -> XfsDa3NodeEntry {
        let hashval = buf_reader.read_u32::<BigEndian>().unwrap();
        let before = buf_reader.read_u32::<BigEndian>().unwrap();

        XfsDa3NodeEntry { hashval, before }
    }
}

#[derive(Debug)]
pub struct XfsDa3Intnode {
    pub hdr: XfsDa3NodeHdr,
    pub btree: Vec<XfsDa3NodeEntry>,
}

impl XfsDa3Intnode {
    pub fn from<R: BufRead + Seek>(buf_reader: &mut R, super_block: &Sb) -> XfsDa3Intnode {
        let hdr = XfsDa3NodeHdr::from(buf_reader.by_ref(), super_block);

        let mut btree = Vec::<XfsDa3NodeEntry>::new();
        for _i in 0..hdr.count {
            btree.push(XfsDa3NodeEntry::from(buf_reader.by_ref()))
        }

        XfsDa3Intnode { hdr, btree }
    }

    pub fn lookup<R: BufRead + Seek, F: Fn(XfsDablk, &mut R) -> XfsFsblock>(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        hash: u32,
        map_da_block_to_fs_block: F,
    ) -> XfsFsblock {
        let mut low: i64 = 0;
        let mut high: i64 = (self.btree.len() - 1) as i64;

        let mut successor = 0;

        while low <= high {
            let mid = low + ((high - low) / 2);

            let key = self.btree[mid as usize].hashval;

            match key.cmp(&hash) {
                Ordering::Greater => {
                    high = mid - 1;
                    successor = mid;
                }
                Ordering::Less => {
                    low = mid + 1;
                }
                Ordering::Equal => {
                    successor = mid;
                    break;
                }
            }
        }

        let blk =
            map_da_block_to_fs_block(self.btree[successor as usize].before, buf_reader.by_ref());

        if self.hdr.level == 1 {
            blk
        } else {
            buf_reader
                .seek(SeekFrom::Start(blk * u64::from(super_block.sb_blocksize)))
                .unwrap();

            let node = XfsDa3Intnode::from(buf_reader.by_ref(), super_block);
            node.lookup(
                buf_reader.by_ref(),
                super_block,
                hash,
                map_da_block_to_fs_block,
            )
        }
    }

    pub fn first_block<R: BufRead + Seek, F: Fn(XfsDablk, &mut R) -> XfsFsblock>(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        map_da_block_to_fs_block: F,
    ) -> XfsFsblock {
        if self.hdr.level == 1 {
            map_da_block_to_fs_block(self.btree.first().unwrap().before, buf_reader.by_ref())
        } else {
            let blk =
                map_da_block_to_fs_block(self.btree.first().unwrap().before, buf_reader.by_ref());
            buf_reader
                .seek(SeekFrom::Start(blk * u64::from(super_block.sb_blocksize)))
                .unwrap();

            let node = XfsDa3Intnode::from(buf_reader.by_ref(), super_block);
            node.first_block(buf_reader.by_ref(), super_block, map_da_block_to_fs_block)
        }
    }
}

impl Decode for XfsDa3Intnode {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let hdr: XfsDa3NodeHdr = Decode::decode(decoder)?;
        let mut btree = Vec::<XfsDa3NodeEntry>::new();
        for _i in 0..hdr.count {
            btree.push(Decode::decode(decoder)?);
        }

        Ok(XfsDa3Intnode {
            hdr,
            btree
        })
    }
}
