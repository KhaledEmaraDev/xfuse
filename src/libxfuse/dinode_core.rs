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
use std::io::BufRead;

use super::definitions::*;

use byteorder::{BigEndian, LittleEndian, ReadBytesExt};
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;
use uuid::Uuid;

#[derive(Debug, FromPrimitive)]
pub enum XfsDinodeFmt {
    Dev,
    Local,
    Extents,
    Btree,
    Uuid,
    Rmap,
}

#[derive(Debug)]
pub struct XfsTimestamp {
    pub t_sec: i32,
    pub t_nsec: i32,
}

pub const XFS_DIFLAG_REALTIME: u16 = 1 << 0;
pub const XFS_DIFLAG_PREALLOC: u16 = 1 << 1;
pub const XFS_DIFLAG_NEWRTBM: u16 = 1 << 2;
pub const XFS_DIFLAG_IMMUTABLE: u16 = 1 << 3;
pub const XFS_DIFLAG_APPEND: u16 = 1 << 4;
pub const XFS_DIFLAG_SYNC: u16 = 1 << 5;
pub const XFS_DIFLAG_NOATIME: u16 = 1 << 6;
pub const XFS_DIFLAG_NODUMP: u16 = 1 << 7;
pub const XFS_DIFLAG_RTINHERIT: u16 = 1 << 8;
pub const XFS_DIFLAG_PROJINHERIT: u16 = 1 << 9;
pub const XFS_DIFLAG_NOSYMLINKS: u16 = 1 << 10;
pub const XFS_DIFLAG_EXTSIZE: u16 = 1 << 11;
pub const XFS_DIFLAG_EXTSZINHERIT: u16 = 1 << 12;
pub const XFS_DIFLAG_NODEFRAG: u16 = 1 << 13;
pub const XFS_DIFLAG_FILESTREAMS: u16 = 1 << 14;

#[derive(Debug)]
pub struct DinodeCore {
    pub di_magic: u16,
    pub di_mode: u16,
    pub di_version: i8,
    pub di_format: XfsDinodeFmt,
    pub di_onlink: u16,
    pub di_uid: u32,
    pub di_gid: u32,
    pub di_nlink: u32,
    pub di_projid: u16,
    pub di_projid_hi: u16,
    pub di_pad: [u8; 6],
    pub di_flushiter: u16,
    pub di_atime: XfsTimestamp,
    pub di_mtime: XfsTimestamp,
    pub di_ctime: XfsTimestamp,
    pub di_size: XfsFsize,
    pub di_nblocks: XfsRfsblock,
    pub di_extsize: XfsExtlen,
    pub di_nextents: XfsExtnum,
    pub di_anextents: XfsAextnum,
    pub di_forkoff: u8,
    pub di_aformat: XfsDinodeFmt,
    pub di_dmevmask: u32,
    pub di_dmstate: u16,
    pub di_flags: u16,
    pub di_gen: u32,
    pub di_next_unlinked: u32,

    pub di_crc: u32,
    pub di_changecount: u64,
    pub di_lsn: u64,
    pub di_flags2: u64,
    pub di_cowextsize: u32,
    pub di_pad2: [u8; 12],
    pub di_crtime: XfsTimestamp,
    pub di_ino: u64,
    pub di_uuid: Uuid,
}

impl DinodeCore {
    pub fn from<R: BufRead>(buf_reader: &mut R) -> DinodeCore {
        let di_magic = buf_reader.read_u16::<BigEndian>().unwrap();
        if di_magic != XFS_DINODE_MAGIC {
            panic!("Agi magic number is invalid");
        }

        let di_mode = buf_reader.read_u16::<BigEndian>().unwrap();
        let di_version = buf_reader.read_i8().unwrap();
        let di_format = XfsDinodeFmt::from_u8(buf_reader.read_u8().unwrap()).unwrap();
        let di_onlink = buf_reader.read_u16::<BigEndian>().unwrap();
        let di_uid = buf_reader.read_u32::<BigEndian>().unwrap();
        let di_gid = buf_reader.read_u32::<BigEndian>().unwrap();
        let di_nlink = buf_reader.read_u32::<BigEndian>().unwrap();
        let di_projid = buf_reader.read_u16::<BigEndian>().unwrap();
        let di_projid_hi = buf_reader.read_u16::<BigEndian>().unwrap();

        let mut buf_pad = [0u8; 6];
        buf_reader.read_exact(&mut buf_pad[..]).unwrap();
        let di_pad = buf_pad;

        let di_flushiter = buf_reader.read_u16::<BigEndian>().unwrap();

        let di_atime = XfsTimestamp {
            t_sec: buf_reader.read_i32::<BigEndian>().unwrap(),
            t_nsec: buf_reader.read_i32::<BigEndian>().unwrap(),
        };
        let di_mtime = XfsTimestamp {
            t_sec: buf_reader.read_i32::<BigEndian>().unwrap(),
            t_nsec: buf_reader.read_i32::<BigEndian>().unwrap(),
        };
        let di_ctime = XfsTimestamp {
            t_sec: buf_reader.read_i32::<BigEndian>().unwrap(),
            t_nsec: buf_reader.read_i32::<BigEndian>().unwrap(),
        };

        let di_size = buf_reader.read_i64::<BigEndian>().unwrap();
        let di_nblocks = buf_reader.read_u64::<BigEndian>().unwrap();
        let di_extsize = buf_reader.read_u32::<BigEndian>().unwrap();
        let di_nextents = buf_reader.read_i32::<BigEndian>().unwrap();
        let di_anextents = buf_reader.read_i16::<BigEndian>().unwrap();
        let di_forkoff = buf_reader.read_u8().unwrap();
        let di_aformat = XfsDinodeFmt::from_u8(buf_reader.read_u8().unwrap()).unwrap();
        let di_dmevmask = buf_reader.read_u32::<BigEndian>().unwrap();
        let di_dmstate = buf_reader.read_u16::<BigEndian>().unwrap();
        let di_flags = buf_reader.read_u16::<BigEndian>().unwrap();
        let di_gen = buf_reader.read_u32::<BigEndian>().unwrap();
        let di_next_unlinked = buf_reader.read_u32::<BigEndian>().unwrap();

        let di_crc = buf_reader.read_u32::<LittleEndian>().unwrap();
        let di_changecount = buf_reader.read_u64::<BigEndian>().unwrap();
        let di_lsn = buf_reader.read_u64::<BigEndian>().unwrap();
        let di_flags2 = buf_reader.read_u64::<BigEndian>().unwrap();
        let di_cowextsize = buf_reader.read_u32::<BigEndian>().unwrap();

        let mut buf_pad2 = [0u8; 12];
        buf_reader.read_exact(&mut buf_pad2[..]).unwrap();
        let di_pad2 = buf_pad2;

        let di_crtime = XfsTimestamp {
            t_sec: buf_reader.read_i32::<BigEndian>().unwrap(),
            t_nsec: buf_reader.read_i32::<BigEndian>().unwrap(),
        };

        let di_ino = buf_reader.read_u64::<BigEndian>().unwrap();
        let di_uuid = Uuid::from_u128(buf_reader.read_u128::<BigEndian>().unwrap());

        DinodeCore {
            di_magic,
            di_mode,
            di_version,
            di_format,
            di_onlink,
            di_uid,
            di_gid,
            di_nlink,
            di_projid,
            di_projid_hi,
            di_pad,
            di_flushiter,
            di_atime,
            di_mtime,
            di_ctime,
            di_size,
            di_nblocks,
            di_extsize,
            di_nextents,
            di_anextents,
            di_forkoff,
            di_aformat,
            di_dmevmask,
            di_dmstate,
            di_flags,
            di_gen,
            di_next_unlinked,
            di_crc,
            di_changecount,
            di_lsn,
            di_flags2,
            di_cowextsize,
            di_pad2,
            di_crtime,
            di_ino,
            di_uuid,
        }
    }
}
