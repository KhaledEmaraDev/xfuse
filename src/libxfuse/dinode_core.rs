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
use super::definitions::*;
use super::utils::{get_file_type, FileKind, Uuid};
use super::S_IFMT;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bincode::{
    Decode,
    de::Decoder,
    error::DecodeError,
    impl_borrow_decode
};
use fuser::FileAttr;
use libc::c_int;
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;


#[derive(Debug, FromPrimitive)]
pub enum XfsDinodeFmt {
    Dev,
    Local,
    Extents,
    Btree,
    Uuid,
    Rmap,
}

impl bincode::Decode for XfsDinodeFmt {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let discriminant: u8 = Decode::decode(decoder)?;
        Ok(XfsDinodeFmt::from_u8(discriminant).expect("Unknown dinode fmt"))
    }
}
impl_borrow_decode!(XfsDinodeFmt);

#[derive(Debug, Decode)]
pub struct XfsTimestamp {
    pub t_sec: i32,
    pub t_nsec: u32,
}

#[allow(dead_code)]
mod constants {
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

    pub const XFS_DIFLAG2_BITTIME: u64 = 1 << 3;
}

#[derive(Debug, bincode::Decode)]
pub struct DinodeCore {
    pub di_magic: u16,
    pub di_mode: u16,
    pub di_version: i8,
    pub di_format: XfsDinodeFmt,
    _di_onlink: u16,
    pub di_uid: u32,
    pub di_gid: u32,
    pub di_nlink: u32,
    _di_projid: u16,
    _di_projid_hi: u16,
    _di_pad: [u8; 6],
    _di_flushiter: u16,
    pub di_atime: XfsTimestamp,
    pub di_mtime: XfsTimestamp,
    pub di_ctime: XfsTimestamp,
    pub di_size: XfsFsize,
    pub di_nblocks: XfsRfsblock,
    _di_extsize: XfsExtlen,
    pub di_nextents: XfsExtnum,
    pub di_anextents: XfsAextnum,
    pub di_forkoff: u8,
    pub di_aformat: XfsDinodeFmt,
    _di_dmevmask: u32,
    _di_dmstate: u16,
    _di_flags: u16,
    pub di_gen: u32,
    _di_next_unlinked: u32,

    _di_crc: u32,
    _di_changecount: u64,
    _di_lsn: u64,
    pub di_flags2: u64,
    _di_cowextsize: u32,
    _di_pad2: [u8; 12],
    pub di_crtime: XfsTimestamp,
    pub di_ino: u64,
    _di_uuid: Uuid,
}

impl DinodeCore {
    pub fn sanity(&self) {
        assert_eq!(self.di_magic, XFS_DINODE_MAGIC,
                   "Agi magic number is invalid");
    }

    pub fn stat(&self, ino: XfsIno) -> Result<FileAttr, c_int> {
        let kind = get_file_type(FileKind::Mode(self.di_mode))?;
        // Special case for ino 1.  FUSE requires / to have inode 1, but XFS
        // does not.
        assert!(ino == 1 || ino == self.di_ino);
        Ok(FileAttr {
                ino,
                size: self.di_size as u64,
                blocks: self.di_nblocks,
                atime: self.timestamp(&self.di_atime),
                mtime: self.timestamp(&self.di_mtime),
                ctime: self.timestamp(&self.di_ctime),
                crtime: self.timestamp(&self.di_crtime),
                kind,
                perm: self.di_mode & !S_IFMT,
                nlink: self.di_nlink,
                uid: self.di_uid,
                gid: self.di_gid,
                rdev: 0,
                blksize: 0,
                flags: 0,
        })
    }

    fn timestamp(&self, ts: &XfsTimestamp) -> SystemTime {
        if self.di_version >= 3 && (self.di_flags2 & constants::XFS_DIFLAG2_BITTIME != 0) {
            // XXX this could be made a const if the Rust const_trait_impl
            // feature stabilizes.
            let classic_epoch: SystemTime = UNIX_EPOCH - Duration::from_secs(i32::MAX as u64 + 1);

            classic_epoch + Duration::from_nanos(
                u64::from(ts.t_sec as u32) * (1u64 << 32) + 
                u64::from(ts.t_nsec)
            )
        } else {
            UNIX_EPOCH + Duration::new(
                ts.t_sec as u64,
                ts.t_nsec,
            )
        }
    }
}
