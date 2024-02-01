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
use std::{
    cmp::Ordering,
    io::{prelude::*, SeekFrom},
    mem,
};

use bincode::Decode;
use byteorder::{BigEndian, LittleEndian, ReadBytesExt};
use num_traits::{PrimInt, Unsigned};
use super::utils::Uuid;

use super::{
    bmbt_rec::BmbtRec,
    definitions::{XfsFileoff, XfsFsblock},
    sb::Sb,
};

pub const POINTERS_AREA_OFFSET: u16 = 0x808;

#[derive(Debug)]
pub struct BtreeBlock<T: PrimInt + Unsigned> {
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

impl<T: PrimInt + Unsigned> BtreeBlock<T> {
    pub fn from<R: BufRead + Seek>(buf_reader: &mut R, super_block: &Sb) -> BtreeBlock<T> {
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

        BtreeBlock {
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
pub type XfsBmbtLblock = BtreeBlock<u64>;

#[derive(Debug, Clone)]
pub struct Btree {
    pub bmdr: BmdrBlock,
    pub keys: Vec<BmbtKey>,
    pub ptrs: Vec<XfsBmbtPtr>,
}

impl Btree {
    pub fn map_block<R: BufRead + Seek>(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        logical_block: XfsFileoff,
    ) -> XfsFsblock {
        let mut low: i64 = 0;
        let mut high: i64 = (self.bmdr.bb_numrecs - 1) as i64;

        let mut predecessor = 0;

        while low <= high {
            let mid = low + ((high - low) / 2);

            let key = self.keys[mid as usize].br_startoff;

            match key.cmp(&logical_block) {
                Ordering::Greater => {
                    high = mid - 1;
                }
                Ordering::Less => {
                    low = mid + 1;
                    predecessor = mid;
                }
                Ordering::Equal => {
                    predecessor = mid;
                    break;
                }
            }
        }

        buf_reader
            .seek(SeekFrom::Start(
                self.ptrs[predecessor as usize] * u64::from(super_block.sb_blocksize),
            ))
            .unwrap();

        let mut bmbt_block = XfsBmbtLblock::from(buf_reader.by_ref(), super_block);
        let mut keys_offset = buf_reader.stream_position().unwrap();

        loop {
            if bmbt_block.bb_level == 0 {
                break;
            } else {
                let mut low: i64 = 0;
                let mut high: i64 = (bmbt_block.bb_numrecs - 1) as i64;

                let mut predecessor = 0;

                while low <= high {
                    let mid = low + ((high - low) / 2);

                    buf_reader.seek(SeekFrom::Start(keys_offset)).unwrap();
                    buf_reader
                        .seek(SeekFrom::Current(mid * (mem::size_of::<BmbtKey>() as i64)))
                        .unwrap();

                    let key = BmbtKey::from(buf_reader.by_ref()).br_startoff;

                    match key.cmp(&logical_block) {
                        Ordering::Greater => {
                            high = mid - 1;
                        }
                        Ordering::Less => {
                            low = mid + 1;
                            predecessor = mid;
                        }
                        Ordering::Equal => {
                            predecessor = mid;
                            break;
                        }
                    }
                }

                buf_reader
                    .seek(SeekFrom::Start(
                        keys_offset - (mem::size_of::<XfsBmbtLblock>() as u64)
                            + u64::from(POINTERS_AREA_OFFSET),
                    ))
                    .unwrap();
                buf_reader.seek(SeekFrom::Current(predecessor * 8)).unwrap();

                let ptr = buf_reader.read_u64::<BigEndian>().unwrap();

                buf_reader
                    .seek(SeekFrom::Start(ptr * u64::from(super_block.sb_blocksize)))
                    .unwrap();

                bmbt_block = XfsBmbtLblock::from(buf_reader.by_ref(), super_block);
                keys_offset = buf_reader.stream_position().unwrap();
            }
        }

        let recs_offset = buf_reader.stream_position().unwrap();

        let mut low: i64 = 0;
        let mut high: i64 = (bmbt_block.bb_numrecs - 1) as i64;

        let mut predecessor = 0;

        while low <= high {
            let mid = low + ((high - low) / 2);

            buf_reader.seek(SeekFrom::Start(recs_offset)).unwrap();
            buf_reader
                .seek(SeekFrom::Current(mid * (mem::size_of::<BmbtRec>() as i64)))
                .unwrap();

            let key = BmbtRec::from(buf_reader.by_ref()).br_startoff;

            match key.cmp(&logical_block) {
                Ordering::Greater => {
                    high = mid - 1;
                }
                Ordering::Less => {
                    low = mid + 1;
                    predecessor = mid;
                }
                Ordering::Equal => {
                    predecessor = mid;
                    break;
                }
            }
        }

        buf_reader.seek(SeekFrom::Start(recs_offset)).unwrap();
        buf_reader
            .seek(SeekFrom::Current(
                predecessor * (mem::size_of::<BmbtRec>() as i64),
            ))
            .unwrap();

        let rec = BmbtRec::from(buf_reader.by_ref());

        rec.br_startblock + (logical_block - rec.br_startoff)
    }
}
