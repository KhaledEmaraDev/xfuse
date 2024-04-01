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
use std::io::{prelude::*, SeekFrom};

use super::definitions::*;
use super::utils::Uuid;

use byteorder::{BigEndian, LittleEndian, ReadBytesExt};
use crc::{Crc, CRC_32_ISCSI};

#[allow(dead_code)]
mod constants {
    pub const XFS_SB_VERSION_ATTRBIT: u16 = 0x0010;
    pub const XFS_SB_VERSION_NLINKBIT: u16 = 0x0020;
    pub const XFS_SB_VERSION_QUOTABIT: u16 = 0x0040;
    pub const XFS_SB_VERSION_ALIGNBIT: u16 = 0x0080;
    pub const XFS_SB_VERSION_DALIGNBIT: u16 = 0x0100;
    pub const XFS_SB_VERSION_SHAREDBIT: u16 = 0x0200;
    pub const XFS_SB_VERSION_LOGV2BIT: u16 = 0x0400;
    pub const XFS_SB_VERSION_SECTORBIT: u16 = 0x0800;
    pub const XFS_SB_VERSION_EXTFLGBIT: u16 = 0x1000;
    pub const XFS_SB_VERSION_DIRV2BIT: u16 = 0x2000;
    pub const XFS_SB_VERSION_MOREBITSBIT: u16 = 0x4000;

    pub const XFS_UQUOTA_ACCT: u16 = 0x0001;
    pub const XFS_UQUOTA_ENFD: u16 = 0x0002;
    pub const XFS_UQUOTA_CHKD: u16 = 0x0004;
    pub const XFS_PQUOTA_ACCT: u16 = 0x0008;
    pub const XFS_OQUOTA_ENFD: u16 = 0x0010;
    pub const XFS_OQUOTA_CHKD: u16 = 0x0020;
    pub const XFS_GQUOTA_ACCT: u16 = 0x0040;
    pub const XFS_GQUOTA_ENFD: u16 = 0x0080;
    pub const XFS_GQUOTA_CHKD: u16 = 0x0100;
    pub const XFS_PQUOTA_ENFD: u16 = 0x0200;
    pub const XFS_PQUOTA_CHKD: u16 = 0x0400;

    pub const XFS_SBF_READONLY: u8 = 0x01;

    pub const XFS_SB_VERSION2_LAZYSBCOUNTBIT: u32 = 0x00000002;
    pub const XFS_SB_VERSION2_ATTR2BIT: u32 = 0x00000008;
    pub const XFS_SB_VERSION2_PARENTBIT: u32 = 0x00000010;
    pub const XFS_SB_VERSION2_PROJID32BIT: u32 = 0x00000080;
    pub const XFS_SB_VERSION2_CRCBIT: u32 = 0x00000100;
    pub const XFS_SB_VERSION2_FTYPE: u32 = 0x00000200;

    pub const XFS_SB_FEAT_INCOMPAT_FTYPE: u32 = 0x00000001;
    pub const XFS_SB_FEAT_INCOMPAT_SPINODES: u32 = 0x00000002;
    pub const XFS_SB_FEAT_INCOMPAT_META_UUID: u32 = 0x00000004;
    pub const XFS_SB_FEAT_INCOMPAT_BIGTIME: u32 = 0x00000008;
    pub const XFS_SB_FEAT_INCOMPAT_NEEDSREPAIR: u32 = 0x00000010;
    pub const XFS_SB_FEAT_INCOMPAT_NREXT64: u32 = 0x00000020;
}

#[derive(Clone, Copy, Debug)]
pub struct Sb {
    // sb_magicnum: u32,
    pub sb_blocksize: u32,
    pub sb_dblocks: XfsRfsblock,
    // sb_rblocks: XfsRfsblock,
    // sb_rextents: XfsRtblock,
    pub sb_uuid: Uuid,
    // sb_logstart: XfsFsblock,
    pub sb_rootino: XfsIno,
    // sb_rbmino: XfsIno,
    // sb_rsumino: XfsIno,
    // sb_rextsize: XfsAgblock,
    pub sb_agblocks: XfsAgblock,
    pub sb_agcount: XfsAgnumber,
    // sb_rbmblocks: XfsExtlen,
    pub sb_logblocks: XfsExtlen,
    // sb_versionnum: u16,
    // sb_sectsize: u16,
    pub sb_inodesize: u16,
    // sb_inopblock: u16,
    // sb_fname: [u8; 12],
    pub sb_blocklog: u8,
    // sb_sectlog: u8,
    pub sb_inodelog: u8,
    pub sb_inopblog: u8,
    pub sb_agblklog: u8,
    // sb_rextslog: u8,
    // sb_inprogress: u8,
    // sb_imax_pct: u8,
    pub sb_icount: u64,
    pub sb_ifree: u64,
    pub sb_fdblocks: u64,
    // sb_frextents: u64,
    // sb_uquotino: XfsIno,
    // sb_gquotino: XfsIno,
    // sb_qflags: u16,
    // sb_flags: u8,
    // sb_shared_vn: u8,
    // sb_inoalignmt: XfsExtlen,
    // sb_unit: u32,
    // sb_width: u32,
    pub sb_dirblklog: u8,
    // sb_logsectlog: u8,
    // sb_logsectsize: u16,
    // sb_logsunit: u32,
    // sb_features2: u32,
    // sb_bad_features2: u32,
    // sb_features_compat: u32,
    // sb_features_ro_compat: u32,
    // sb_features_incompat: u32,
    // sb_features_log_incompat: u32,
}

impl Sb {
    const BBSHIFT: u8 = 9;

    pub fn from<T: BufRead + Seek>(buf_reader: &mut T) -> Sb {
        let sb_magicnum = buf_reader.read_u32::<BigEndian>().unwrap();
        if sb_magicnum != XFS_SB_MAGIC {
            panic!("Superblock magic number is invalid");
        }

        let sb_blocksize = buf_reader.read_u32::<BigEndian>().unwrap();
        let sb_dblocks = buf_reader.read_u64::<BigEndian>().unwrap();
        let _sb_rblocks = buf_reader.read_u64::<BigEndian>().unwrap();
        let _sb_rextents = buf_reader.read_u64::<BigEndian>().unwrap();
        let sb_uuid = Uuid::from_u128(buf_reader.read_u128::<BigEndian>().unwrap());
        let _sb_logstart = buf_reader.read_u64::<BigEndian>().unwrap();
        let sb_rootino = buf_reader.read_u64::<BigEndian>().unwrap();
        let _sb_rbmino = buf_reader.read_u64::<BigEndian>().unwrap();
        let _sb_rsumino = buf_reader.read_u64::<BigEndian>().unwrap();
        let _sb_rextsize = buf_reader.read_u32::<BigEndian>().unwrap();
        let sb_agblocks = buf_reader.read_u32::<BigEndian>().unwrap();
        let sb_agcount = buf_reader.read_u32::<BigEndian>().unwrap();
        let _sb_rbmblocks = buf_reader.read_u32::<BigEndian>().unwrap();
        let sb_logblocks = buf_reader.read_u32::<BigEndian>().unwrap();
        let sb_versionnum = buf_reader.read_u16::<BigEndian>().unwrap();
        let _sb_sectsize = buf_reader.read_u16::<BigEndian>().unwrap();
        let sb_inodesize = buf_reader.read_u16::<BigEndian>().unwrap();
        let _sb_inopblock = buf_reader.read_u16::<BigEndian>().unwrap();

        let mut buf_fname = [0u8; 12];
        buf_reader.read_exact(&mut buf_fname[..]).unwrap();
        let _sb_fname = buf_fname;

        let sb_blocklog = buf_reader.read_u8().unwrap();
        let _sb_sectlog = buf_reader.read_u8().unwrap();
        let sb_inodelog = buf_reader.read_u8().unwrap();
        let sb_inopblog = buf_reader.read_u8().unwrap();
        let sb_agblklog = buf_reader.read_u8().unwrap();
        let _sb_rextslog = buf_reader.read_u8().unwrap();
        let _sb_inprogress = buf_reader.read_u8().unwrap();
        let _sb_imax_pct = buf_reader.read_u8().unwrap();
        let sb_icount = buf_reader.read_u64::<BigEndian>().unwrap();
        let sb_ifree = buf_reader.read_u64::<BigEndian>().unwrap();
        let sb_fdblocks = buf_reader.read_u64::<BigEndian>().unwrap();
        let _sb_frextents = buf_reader.read_u64::<BigEndian>().unwrap();
        let _sb_uquotino = buf_reader.read_u64::<BigEndian>().unwrap();
        let _sb_gquotino = buf_reader.read_u64::<BigEndian>().unwrap();
        let _sb_qflags = buf_reader.read_u16::<BigEndian>().unwrap();
        let _sb_flags = buf_reader.read_u8().unwrap();
        let _sb_shared_vn = buf_reader.read_u8().unwrap();
        let _sb_inoalignmt = buf_reader.read_u32::<BigEndian>().unwrap();
        let _sb_unit = buf_reader.read_u32::<BigEndian>().unwrap();
        let _sb_width = buf_reader.read_u32::<BigEndian>().unwrap();
        let sb_dirblklog = buf_reader.read_u8().unwrap();
        let _sb_logsectlog = buf_reader.read_u8().unwrap();
        let _sb_logsectsize = buf_reader.read_u16::<BigEndian>().unwrap();
        let _sb_logsunit = buf_reader.read_u32::<BigEndian>().unwrap();
        let sb_features2 = buf_reader.read_u32::<BigEndian>().unwrap();
        let _sb_bad_features2 = buf_reader.read_u32::<BigEndian>().unwrap();

        /* Version 5 superblock features */
        let _sb_features_compat = buf_reader.read_u32::<BigEndian>().unwrap();
        let _sb_features_ro_compat = buf_reader.read_u32::<BigEndian>().unwrap();
        let _sb_features_incompat = buf_reader.read_u32::<BigEndian>().unwrap();
        let _sb_features_log_incompat = buf_reader.read_u32::<BigEndian>().unwrap();

        buf_reader.seek(SeekFrom::Start(0)).unwrap();

        const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);
        let mut digest = CASTAGNOLI.digest();

        let mut buf_bcrc = [0u8; 224];
        buf_reader.read_exact(&mut buf_bcrc).unwrap();
        digest.update(&buf_bcrc);
        digest.update(&[0u8; 4]);

        let sb_crc = buf_reader.read_u32::<LittleEndian>().unwrap();

        let mut buf_acrc = [0u8; 284];
        buf_reader.read_exact(&mut buf_acrc).unwrap();
        digest.update(&buf_acrc);

        if digest.finalize() != sb_crc {
            panic!("Crc check failed!");
        }

        if sb_versionnum & 0xF != 5 {
            panic!("Unsupported filesystem version number {}", sb_versionnum & 0xF);
        }
        if sb_features2 & constants::XFS_SB_VERSION2_ATTR2BIT == 0 {
            panic!("Version 1 extended attributes are not supported");
        }

        Sb {
            sb_blocksize,
            sb_dblocks,
            sb_uuid,
            sb_rootino,
            sb_agblocks,
            sb_agcount,
            sb_logblocks,
            sb_inodesize,
            sb_blocklog,
            sb_inodelog,
            sb_inopblog,
            sb_agblklog,
            sb_icount,
            sb_ifree,
            sb_fdblocks,
            sb_dirblklog,
        }
    }

    #[inline]
    pub fn get_dir3_leaf_offset(&self) -> XfsDablk {
        1 << (35 - self.sb_blocklog)
    }

    /// Given a file system block number, calculate its disk address in units of 512B blocks
    fn fsb_to_daddr(&self, fsbno: XfsFsblock) -> u64 {

        let blkbb_log = self.sb_blocklog - Self::BBSHIFT;
        let agno = fsbno >> self.sb_agblklog;
        let agbno = fsbno & ((1 << self.sb_agblklog) - 1);
        (agno * u64::from(self.sb_agblocks) + agbno) << blkbb_log
    }

    /// Given a file system block number, calculate its disk byte offset
    pub fn fsb_to_offset(&self, fsbno: XfsFsblock) -> u64 {
        self.fsb_to_daddr(fsbno) << Self::BBSHIFT
    }
}
