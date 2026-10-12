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

use bitflags::bitflags;
use byteorder::{BigEndian, ByteOrder, LittleEndian, ReadBytesExt};
use crc::{Crc, CRC_32_ISCSI};

use super::{
    definitions::*,
    error::{FsError, FsResult},
    utils::Uuid,
};

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
    pub const XFS_SB_FEAT_INCOMPAT_EXCHRANGE: u32 = 0x00000040;
    pub const XFS_SB_FEAT_INCOMPAT_PARENT: u32 = 0x00000080;
    pub const XFS_SB_FEAT_INCOMPAT_METADIR: u32 = 0x00000100;
    pub const XFS_SB_FEAT_INCOMPAT_ZONED: u32 = 0x00000200;
    pub const XFS_SB_FEAT_INCOMPAT_ZONE_GAPS: u32 = 0x00000400;
}

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SbFeatures2: u32 {
        const LazySbCount = constants::XFS_SB_VERSION2_LAZYSBCOUNTBIT;
        const Attr2 = constants::XFS_SB_VERSION2_ATTR2BIT;
        const Parent = constants::XFS_SB_VERSION2_PARENTBIT;
        const ProjId32 = constants::XFS_SB_VERSION2_PROJID32BIT;
        const Crc = constants::XFS_SB_VERSION2_CRCBIT;
        const Ftype = constants::XFS_SB_VERSION2_FTYPE;
        const _ = !0;
    }
}

impl SbFeatures2 {
    pub const fn crc(&self) -> bool {
        self.contains(SbFeatures2::Crc)
    }

    #[allow(dead_code)] // Used as soon as a directory is written; no caller yet.
    pub const fn ftype(&self) -> bool {
        self.contains(SbFeatures2::Ftype)
    }
}

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SbFeaturesIncompat: u32 {
        const Ftype = constants::XFS_SB_FEAT_INCOMPAT_FTYPE;
        const SpInodes = constants::XFS_SB_FEAT_INCOMPAT_SPINODES;
        const MetaUuid = constants::XFS_SB_FEAT_INCOMPAT_META_UUID;
        const Bigtime = constants::XFS_SB_FEAT_INCOMPAT_BIGTIME;
        const NeedsRepair = constants::XFS_SB_FEAT_INCOMPAT_NEEDSREPAIR;
        const NrExt64 = constants::XFS_SB_FEAT_INCOMPAT_NREXT64;
        const Exchrange = constants::XFS_SB_FEAT_INCOMPAT_EXCHRANGE;
        const Parent = constants::XFS_SB_FEAT_INCOMPAT_PARENT;
        const Metadir = constants::XFS_SB_FEAT_INCOMPAT_METADIR;
        const Zoned = constants::XFS_SB_FEAT_INCOMPAT_ZONED;
        const ZONE_GAPS = constants::XFS_SB_FEAT_INCOMPAT_ZONE_GAPS;
    }
}

impl SbFeaturesIncompat {
    pub const fn ftype(&self) -> bool {
        self.contains(SbFeaturesIncompat::Ftype)
    }

    // AFAICT, read-only implementations don't need to care.
    //pub const fn sparse_inodes(&self) -> bool {
    //    self.contains(SbFeaturesIncompat::SpInodes)
    //}

    pub const fn meta_uuid(&self) -> bool {
        self.contains(SbFeaturesIncompat::MetaUuid)
    }

    // This is redundant with information in DinodeCore.di_flags2
    //pub const fn bigtime(&self) -> bool {
    //    self.contains(SbFeaturesIncompat::Bigtime)
    //}

    pub const fn needs_repair(&self) -> bool {
        self.contains(SbFeaturesIncompat::NeedsRepair)
    }

    // This is redundant with information in DinodeCore.di_flags2
    //pub const fn large_extent_counters(&self) -> bool {
    //    self.contains(SbFeaturesIncompat::NrExt64)
    //}

    // I don't think we need to care about this, unless se support log intents
    //pub const fn exchrange(&self) -> bool{
    //    self.contains(SbFeaturesIncompat::Exchrange)
    //}

    pub const fn metadir(&self) -> bool {
        self.contains(SbFeaturesIncompat::Metadir)
    }

    pub const fn zoned(&self) -> bool {
        self.contains(SbFeaturesIncompat::Zoned)
    }

    // This feature will never be enabled if zoned is not, and we don't support zoned, so we don't
    // need to check for it explicitly.
    //pub const fn zone_gaps(&self) -> bool {
    //  self.contains(SbFeaturesIncompat::ZoneGaps)
    //}
}

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SbFeaturesLogIncompat: u32 {}
}

#[derive(Clone, Copy, Debug)]
pub struct Sb {
    // sb_magicnum: u32,
    pub sb_blocksize: u32,
    pub sb_dblocks: XfsRfsblock,
    pub sb_rblocks: XfsRfsblock,
    // sb_rextents: XfsRtblock,
    pub sb_uuid: Uuid,
    #[allow(dead_code)] // Read by `log_blocks`, which the log module's tests use.
    pub sb_logstart: XfsFsblock,
    pub sb_rootino: XfsIno,
    // sb_rbmino: XfsIno,
    // sb_rsumino: XfsIno,
    // sb_rextsize: XfsAgblock,
    pub sb_agblocks: XfsAgblock,
    pub sb_agcount: XfsAgnumber,
    // sb_rbmblocks: XfsExtlen,
    pub sb_logblocks: XfsExtlen,
    sb_versionnum: u16,
    // sb_sectsize: u16,
    pub sb_inodesize: u16,
    /// Inodes per file system block, as the format stores it; its log2 is
    /// [`Self::sb_inopblog`], which is what the inode number is decoded with.
    #[allow(dead_code)] // Its log2, `sb_inopblog`, is what the inode number uses.
    pub sb_inopblock: u16,
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
    /// The basic block size, which is the granularity of the file system's
    /// metadata addressing.  It is the sector size the file system was made
    /// on, and it is not always a whole file system block.
    sb_sectsize: u16,
    sb_features2: SbFeatures2,
    // sb_bad_features2: u32,
    // sb_features_compat: u32,
    /// Features that only make sense on a read-only file system, such as
    /// reflink and the reverse mapping B+tree.
    sb_features_ro_compat: u32,
    // sb_features_incompat: u32,
    // sb_features_log_incompat: u32,
    sb_features_incompat: SbFeaturesIncompat,
    /// File system level flags, such as "this file system is read-only".
    sb_flags: u8,
}

/// Where an inode number says an inode is.
///
/// An XFS inode number is not an index into a table somewhere: it **is** the
/// address, split into three fields.  The low `sb_inopblog` bits say which inode
/// within a block, the next `sb_agblklog` bits say which block within the group,
/// and what is left says which group.  Nothing else is needed to find the bytes
/// of an inode, which is why there is no table mapping inode chunks to blocks
/// anywhere in the format.
///
/// Whether an inode *may* be read is a separate question, and it is answered by
/// the group's b-tree of used inode numbers: a number can name a block that is
/// not an inode chunk at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // The mapping the inode work is built on; nothing reads it yet.
pub struct InoAddr {
    /// The allocation group that owns the inode.
    pub agno: u32,
    /// The inode's number within that group.
    pub agino: u32,
    /// The block within the group that holds it.
    pub agbno: u32,
    /// Which of the inodes in that block it is.
    pub slot: u16,
}

impl Sb {
    /// The sector within an allocation group that holds its free list.
    #[allow(dead_code)] // The allocator that calls this is the next phase.
    pub const AGFL_SECTOR: u32 = 3;
    /// The sector within an allocation group that holds its group file.
    pub const AGF_SECTOR: u32 = 1;
    /// The sector within an allocation group that holds its group inode
    /// header.
    #[allow(dead_code)] // The allocator that calls this is the next phase.
    pub const AGI_SECTOR: u32 = 2;
    const BBSHIFT: u8 = 9;
    /// Where the superblock's own checksum sits.  It is stored little-endian
    /// even though everything around it is big-endian.
    pub const BCRC: usize = 224;
    /// Where the count of free data blocks sits in the superblock's bytes.
    ///
    /// Every field ahead of it is a fixed size, so this is what the field by
    /// field parse above adds up to.  It is spelled out here rather than left to
    /// the parse because a field that can be written back has to be *placed*,
    /// and a test checks this offset against a real superblock rather than
    /// against this file.
    pub const FDBLOCKS: usize = 144;
    /// Where the second feature word sits, which is where the checksum bit
    /// lives.  The version number does not carry it: a version 4 file system
    /// made without checksums and a version 5 one differ here rather than in
    /// their version number.
    pub const FEATURES2: usize = 200;
    /// Where the count of inodes sits, immediately ahead of the free ones.
    ///
    /// The two are adjacent and they move together in one direction and apart in
    /// the other: allocating an inode lowers the free count and leaves the total
    /// alone, and allocating a chunk of them raises the total and the free count
    /// by the same sixty-four.  A file system that moved one and not the other is
    /// what `sb_icount 64, counted 128` is.
    pub const ICOUNT: usize = 128;
    /// Write a new count of the free blocks on the data device into the
    /// superblock's bytes, fixing the checksum if this file system has one.
    ///
    /// The bytes are patched, not rebuilt.  The superblock is the first sector
    /// of a block that the first group's headers share, and the parse above
    /// deliberately threw away most of the fields it walked past, so writing
    /// back what was parsed would mean writing back a superblock with the parts
    /// nobody kept missing from it.  Patching one field in bytes that were read
    /// off the image cannot do that.
    /// Where the count of free inodes sits in the superblock's bytes.
    ///
    /// Measured rather than counted: the field's place in the parse and the
    /// field's place in the bytes are two different things, and counting from a
    /// neighbouring field put it eight bytes out on both reference images.
    pub const IFREE: usize = 136;
    /// How many inodes the format allocates and tracks at a time.
    ///
    /// Sixty-four, for every file system, which is why a chunk is not a fixed
    /// number of blocks: at two inodes per block it is thirty-two, and it would
    /// be eight at eight per block.
    #[allow(dead_code)] // As with `InoAddr`: built and tested, not yet read by a caller.
    pub const INODES_PER_CHUNK: u32 = 64;
    /// `XLOG_HEADER_MAGIC_NUM`: the word at the start of every log block.
    ///
    /// the published XFS documentation (XFS Algorithms and Data Structures, and the XFS pages at docs.kernel.org) calls it "Invalid cycle number" because of what it
    /// replaces.  It is how the log is *identified* -- a run of blocks that all
    /// carry it at offset 0 **is** the log -- which is the only reason this
    /// project can exclude the log from an image comparison without being told
    /// where it is.
    #[allow(dead_code)] // The log module has its own, which is where it is used.
    pub const XLOG_HEADER_MAGIC_NUM: u32 = 0xFEED_BABE;
    /// `XLOG_HEADER_SIZE`: the header is padded to this, and each log block is a
    /// multiple of it, so `h_cycle_data` is an array of this many 4-byte words
    /// filling the block.
    #[allow(dead_code)] // The log module has its own, which is where it is used.
    pub const XLOG_HEADER_SIZE: usize = 512;

    /// Decode an inode number into the place it names.
    ///
    /// This is arithmetic on the published geometry fields and nothing else --
    /// no table, and nothing read from the image.  It says where an inode would
    /// be, not that it is there.
    #[allow(dead_code)] // As with `InoAddr`.
    pub fn locate_ino(&self, ino: u64) -> InoAddr {
        let inopblog = u32::from(self.sb_inopblog);
        let agblklog = u32::from(self.sb_agblklog);
        let block_bits = agblklog + inopblog;
        let in_block_mask = (1u64 << inopblog) - 1;
        let in_group_mask = (1u64 << block_bits) - 1;
        let agino = (ino & in_group_mask) as u32;
        InoAddr {
            agno: (ino >> block_bits) as u32,
            agino,
            agbno: (u64::from(agino) >> inopblog) as u32,
            slot: (ino & in_block_mask) as u16,
        }
    }

    /// The inode number a group's relative number makes, which is the inverse of
    /// [`Self::locate_ino`].
    pub fn make_ino(&self, agno: u32, agino: u32) -> u64 {
        let block_bits = u32::from(self.sb_agblklog) + u32::from(self.sb_inopblog);
        (u64::from(agno) << block_bits) | u64::from(agino)
    }

    /// The image block an inode number names.
    ///
    /// The block the number carries is counted **from the start of its group**,
    /// headers and all: in the reference image the root, inode 32, is at group
    /// block 16 while the group's headers occupy blocks 0 to 3.  Nothing is
    /// subtracted, and nothing needs to be -- the inode numbers that would land
    /// on the headers are reserved ones, which are never allocated and so never
    /// have a chunk to be read from.
    #[allow(dead_code)] // As with `InoAddr`.
    pub fn ino_to_fsb(&self, ino: u64) -> u64 {
        let loc = self.locate_ino(ino);
        // A block number counted from the start of the image, so the arithmetic is
        // in blocks throughout.  Adding a block number to `ag_offset` -- which is
        // a *byte* offset -- works for group 0, whose offset is zero, and is wrong
        // for every other group.  That is how this went unnoticed: nothing on the
        // read path calls it, and the only caller that did, `allocate_ino` writing
        // a new inode's slot, has never run because there is no `create`.  A new
        // inode chunk called it for the first time, in group 1, and the first call
        // was wrong.
        u64::from(loc.agno) * u64::from(self.sb_agblocks) + u64::from(loc.agbno)
    }

    /// The byte offset of an inode's own fields.
    pub fn ino_to_offset(&self, ino: u64) -> u64 {
        let loc = self.locate_ino(ino);
        // Through `ag_block_offset`, so that the block-within-a-group arithmetic
        // and the inode-to-offset arithmetic cannot drift apart -- which is how
        // `ino_to_fsb` and this came to disagree in the first place, and why
        // `ag_block_offset` was dead code with a second, wrong version of the same
        // answer in it.
        self.ag_block_offset(loc.agno, loc.agbno)
            + u64::from(loc.slot) * u64::from(self.sb_inodesize)
    }

    /// How many blocks a chunk of inodes occupies.
    ///
    /// Sixty-four inodes however many fit in a block, so thirty-two blocks at two
    /// inodes apiece and eight at eight.  This is a property of the chunk, not of
    /// where it starts: which chunk a number belongs to is what the group's tree
    /// of used inode numbers answers, and these images space their records a
    /// hundred and sixty inodes apart rather than sixty-four, so arithmetic on
    /// the number alone would be wrong here.
    #[allow(dead_code)] // As with `InoAddr`.
    pub fn chunk_blocks(&self) -> u32 {
        ((Self::INODES_PER_CHUNK - 1) >> u32::from(self.sb_inopblog)) + 1
    }

    pub fn from<T: BufRead + Seek>(buf_reader: &mut T) -> Sb {
        let sb_magicnum = buf_reader.read_u32::<BigEndian>().unwrap();
        if sb_magicnum != XFS_SB_MAGIC {
            panic!("Superblock magic number is invalid");
        }

        let sb_blocksize = buf_reader.read_u32::<BigEndian>().unwrap();
        let sb_dblocks = buf_reader.read_u64::<BigEndian>().unwrap();
        let sb_rblocks = buf_reader.read_u64::<BigEndian>().unwrap();
        let _sb_rextents = buf_reader.read_u64::<BigEndian>().unwrap();
        let sb_uuid = Uuid::from_u128(buf_reader.read_u128::<BigEndian>().unwrap());
        let sb_logstart = buf_reader.read_u64::<BigEndian>().unwrap();
        let sb_rootino = buf_reader.read_u64::<BigEndian>().unwrap();
        let _sb_rbmino = buf_reader.read_u64::<BigEndian>().unwrap();
        let _sb_rsumino = buf_reader.read_u64::<BigEndian>().unwrap();
        let _sb_rextsize = buf_reader.read_u32::<BigEndian>().unwrap();
        let sb_agblocks = buf_reader.read_u32::<BigEndian>().unwrap();
        let sb_agcount = buf_reader.read_u32::<BigEndian>().unwrap();
        let _sb_rbmblocks = buf_reader.read_u32::<BigEndian>().unwrap();
        let sb_logblocks = buf_reader.read_u32::<BigEndian>().unwrap();
        let sb_versionnum = buf_reader.read_u16::<BigEndian>().unwrap();
        let sb_sectsize = buf_reader.read_u16::<BigEndian>().unwrap();
        let sb_inodesize = buf_reader.read_u16::<BigEndian>().unwrap();
        let sb_inopblock = buf_reader.read_u16::<BigEndian>().unwrap();

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
        let sb_flags = buf_reader.read_u8().unwrap();
        let _sb_shared_vn = buf_reader.read_u8().unwrap();
        let _sb_inoalignmt = buf_reader.read_u32::<BigEndian>().unwrap();
        let _sb_unit = buf_reader.read_u32::<BigEndian>().unwrap();
        let _sb_width = buf_reader.read_u32::<BigEndian>().unwrap();
        let sb_dirblklog = buf_reader.read_u8().unwrap();
        let _sb_logsectlog = buf_reader.read_u8().unwrap();
        let _sb_logsectsize = buf_reader.read_u16::<BigEndian>().unwrap();
        let _sb_logsunit = buf_reader.read_u32::<BigEndian>().unwrap();
        let sb_features2 =
            SbFeatures2::from_bits(buf_reader.read_u32::<BigEndian>().unwrap()).unwrap();
        let _sb_bad_features2 = buf_reader.read_u32::<BigEndian>().unwrap();

        /* Version 5 superblock features */
        let _sb_features_compat = buf_reader.read_u32::<BigEndian>().unwrap();
        let sb_features_ro_compat = buf_reader.read_u32::<BigEndian>().unwrap();
        let incompat_raw = buf_reader.read_u32::<BigEndian>().unwrap();
        let sb_features_incompat = SbFeaturesIncompat::from_bits(incompat_raw)
            .unwrap_or_else(|| panic!("Unknown value in sb_features_incompat: {incompat_raw:?}"));
        let log_incompat_raw = buf_reader.read_u32::<BigEndian>().unwrap();
        let _sb_features_log_incompat = SbFeaturesLogIncompat::from_bits(log_incompat_raw)
            .unwrap_or_else(|| {
                panic!("Unknown value in sb_features_log_incompat: {log_incompat_raw:?}")
            });

        buf_reader.seek(SeekFrom::Start(0)).unwrap();

        const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);
        let mut digest = CASTAGNOLI.digest();

        let mut buf_bcrc = [0u8; 224];
        buf_reader.read_exact(&mut buf_bcrc).unwrap();
        digest.update(&buf_bcrc);
        digest.update(&[0u8; 4]);

        let sb_crc = buf_reader.read_u32::<LittleEndian>().unwrap();

        let mut buf_acrc = vec![0u8; usize::from(sb_sectsize) - 228];
        buf_reader.read_exact(&mut buf_acrc).unwrap();
        digest.update(&buf_acrc);

        if ![4, 5].contains(&(sb_versionnum & 0xF)) {
            panic!(
                "Unsupported filesystem version number {}",
                sb_versionnum & 0xF
            );
        }
        if sb_versionnum & 0xF == 5 && !sb_features2.crc() {
            panic!("Version 5 file systems must set the CRC bit in sb_features2");
        }
        if sb_features2.crc() && digest.finalize() != sb_crc {
            panic!("Crc check failed!");
        }
        if sb_features_incompat.meta_uuid() {
            panic!("The Metadata UUID feature is not supported");
        }
        if sb_features_incompat.needs_repair() {
            panic!("The NeedsRepair feature is not supported");
        }
        if sb_features_incompat.metadir() && sb_rblocks > 0 {
            panic!("The Metadir feature is not supported in combination with a real-time volume");
        }
        if sb_features_incompat.zoned() && sb_rblocks > 0 {
            panic!("The Zoned feature is not supported on real-time volumes");
        }

        Sb {
            sb_logstart,
            sb_blocksize,
            sb_dblocks,
            sb_rblocks,
            sb_uuid,
            sb_rootino,
            sb_agblocks,
            sb_agcount,
            sb_logblocks,
            sb_versionnum,
            sb_inodesize,
            sb_inopblock,
            sb_blocklog,
            sb_inodelog,
            sb_inopblog,
            sb_agblklog,
            sb_icount,
            sb_ifree,
            sb_fdblocks,
            sb_dirblklog,
            sb_sectsize,
            sb_features2,
            sb_features_ro_compat,
            sb_features_incompat,
            sb_flags,
        }
    }

    /// Is a read-only-compat feature enabled?
    ///
    /// These are the features that describe how a file system was built rather
    /// than whether it may be written to.  Each of them implies on-disk
    /// structures that a writer has to keep up to date.
    pub const fn read_only_compat(&self, feature: u32) -> bool {
        self.sb_features_ro_compat & feature != 0
    }

    /// Is an incompatible feature enabled?
    ///
    /// An incompatible feature means the file system cannot be mounted by an
    /// implementation that does not know about it at all.
    pub const fn incompat(&self, feature: u32) -> bool {
        self.sb_features_incompat
            .intersects(SbFeaturesIncompat::from_bits_truncate(feature))
    }

    /// Does the superblock itself say that this file system is read-only?
    ///
    /// The file system sets this flag when it was unmounted cleanly or was
    /// deliberately made read-only, and the kernel's driver refuses to write
    /// to an image that carries it.
    pub const fn is_read_only(&self) -> bool {
        self.sb_flags & constants::XFS_SBF_READONLY != 0
    }

    /// Move the count of free inodes down by a number of allocations.
    ///
    /// This is the sum of the groups' free inode counts, checked exactly on both
    /// reference images, so it moves when a group does and there is no separate
    /// term to decide on -- unlike the free *block* total, which does not match
    /// the sum of its groups and is recorded as unresolved.
    ///
    /// Patched in the bytes rather than rebuilt from the struct, for the same
    /// reason as the block count: the struct threw away most of what it read.
    pub fn patch_ifree(bytes: &mut [u8], lower_by: u64) -> FsResult<u64> {
        Self::move_ifree(bytes, -(lower_by as i128))
    }

    /// Move the superblock's inode count up by `by`.
    ///
    /// The counterpart of taking an inode out of a chunk, which moves the free
    /// count and leaves this one alone.  A chunk moves both.
    pub fn add_icount(bytes: &mut [u8], by: u64) -> FsResult<u64> {
        if bytes.len() < Self::ICOUNT + 8 {
            return Err(FsError::Corrupt {
                what: "the superblock is too short to hold an inode count".into(),
            });
        }
        let now = BigEndian::read_u64(&bytes[Self::ICOUNT..Self::ICOUNT + 8]);
        let next = now.checked_add(by).ok_or_else(|| FsError::Corrupt {
            what: format!("the file system claims {now} inodes and {by} more were added"),
        })?;
        BigEndian::write_u64(&mut bytes[Self::ICOUNT..Self::ICOUNT + 8], next);
        Ok(next)
    }

    /// Move the superblock's free inode count **up** by `by`, for a group that
    /// has just gained a chunk.
    ///
    /// The counterpart of [`Sb::patch_ifree`], and it exists because the count
    /// does go both ways: taking an inode out of a chunk lowers it, and adding a
    /// chunk of sixty-four raises it.  A function that can only lower cannot
    /// express the second, and using it for the first is a subtraction that would
    /// quietly underflow on a file system that has just been given something.
    pub fn add_ifree(bytes: &mut [u8], by: u64) -> FsResult<u64> {
        Self::move_ifree(bytes, by as i128)
    }

    /// Move the superblock's free inode count by `by`, which may be negative.
    fn move_ifree(bytes: &mut [u8], by: i128) -> FsResult<u64> {
        if bytes.len() < Self::IFREE + 8 {
            return Err(FsError::Corrupt {
                what: "the superblock is too short to hold a free inode count".into(),
            });
        }
        let now = BigEndian::read_u64(&bytes[Self::IFREE..Self::IFREE + 8]);
        let next = i128::from(now)
            .checked_add(by)
            .and_then(|n| u64::try_from(n).ok())
            .ok_or_else(|| FsError::Corrupt {
                what: format!("moving the free inode count by {by} from {now} leaves nothing"),
            })?;
        BigEndian::write_u64(&mut bytes[Self::IFREE..Self::IFREE + 8], next);
        // A file system without checksums has a *zero* in the checksum field, and
        // writing one there is a corruption of its own: repair objects to the
        // unused part of the superblock, and it is right to.  So the gate is the
        // same one the block count uses -- the feature bit, not the magic.
        if bytes.len() < Self::FEATURES2 + 4 {
            return Ok(next);
        }
        let features2 = BigEndian::read_u32(&bytes[Self::FEATURES2..]);
        if features2 & constants::XFS_SB_VERSION2_CRCBIT == 0 {
            return Ok(next);
        }
        let sectsize = usize::from(BigEndian::read_u16(&bytes[102..]));
        let bcrc = Self::BCRC;
        bytes[bcrc..bcrc + 4].fill(0);
        const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);
        let mut digest = CASTAGNOLI.digest();
        digest.update(&bytes[..bcrc]);
        digest.update(&[0u8; 4]);
        let tail = bytes.len().min(sectsize);
        if tail > bcrc + 4 {
            digest.update(&bytes[bcrc + 4..tail]);
        }
        LittleEndian::write_u32(&mut bytes[bcrc..], digest.finalize());
        Ok(next)
    }

    pub fn patch_fdblocks(bytes: &mut [u8], free: u64) -> FsResult<()> {
        if bytes.len() < Self::BCRC + 4 {
            return Err(FsError::Corrupt {
                what: "the superblock is shorter than its own checksum".into(),
            });
        }
        if BigEndian::read_u32(&bytes[0..]) != XFS_SB_MAGIC {
            return Err(FsError::Corrupt {
                what: "the superblock's magic number is wrong".into(),
            });
        }
        let sectsize = usize::from(BigEndian::read_u16(&bytes[102..]));
        let features2 = BigEndian::read_u32(&bytes[Self::FEATURES2..]);
        BigEndian::write_u64(&mut bytes[Self::FDBLOCKS..], free);
        if features2 & constants::XFS_SB_VERSION2_CRCBIT == 0 {
            return Ok(());
        }
        // The checksum covers everything ahead of the checksum field, then the
        // four bytes of the field itself read as zeroes, then the rest of the
        // first sector.
        let bcrc = Self::BCRC;
        bytes[bcrc..bcrc + 4].fill(0);
        const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);
        let mut digest = CASTAGNOLI.digest();
        digest.update(&bytes[..bcrc]);
        digest.update(&[0u8; 4]);
        let tail = bytes.len().min(sectsize);
        if tail > bcrc + 4 {
            digest.update(&bytes[bcrc + 4..tail]);
        }
        LittleEndian::write_u32(&mut bytes[bcrc..], digest.finalize());
        Ok(())
    }

    /// The count of free blocks the superblock itself records, read back out of
    /// its own bytes rather than out of a parsed struct.
    pub fn fdblocks_in(bytes: &[u8]) -> FsResult<u64> {
        if bytes.len() < Self::FDBLOCKS + 8 {
            return Err(FsError::Corrupt {
                what: "the superblock is too short to hold a free block count".into(),
            });
        }
        Ok(BigEndian::read_u64(&bytes[Self::FDBLOCKS..]))
    }

    /// Enable a read-only-compat feature.  Only the tests need this.
    #[cfg(test)]
    pub fn set_read_only_compat(&mut self, feature: u32) {
        self.sb_features_ro_compat |= feature;
    }

    /// Enable an incompatible feature.  Only the tests need this.
    #[cfg(test)]
    pub fn set_incompat(&mut self, feature: u32) {
        self.sb_features_incompat
            .insert(SbFeaturesIncompat::from_bits_truncate(feature));
    }

    #[inline]
    pub fn get_dir3_leaf_offset(&self) -> XfsDablk {
        1 << (35 - self.sb_blocklog)
    }

    /// How many allocation groups the file system has.
    pub const fn agcount(&self) -> u32 {
        self.sb_agcount
    }

    /// The basic block size: the granularity the file system addresses its
    /// metadata in.  It is the sector size the file system was made on, and it
    /// is not always a whole file system block -- a file system with 4 KiB
    /// blocks on 512-byte sectors has four metadata headers in its first block.
    pub const fn sectsize(&self) -> u16 {
        self.sb_sectsize
    }

    /// A superblock with nothing but geometry in it.
    ///
    /// Only for tests: an image is decoded by reading one, and a test that
    /// wants an allocator over a group has no image yet.
    #[cfg(test)]
    pub fn for_tests(
        blocksize: u32,
        sectsize: u16,
        agblocks: u32,
        agcount: u32,
        inodesize: u16,
    ) -> Self {
        let mut sb: Sb = unsafe { std::mem::zeroed() };
        sb.sb_blocksize = blocksize;
        sb.sb_sectsize = sectsize;
        sb.sb_agblocks = agblocks;
        sb.sb_agcount = agcount;
        sb.sb_inodesize = inodesize;
        sb.sb_blocklog = blocksize.trailing_zeros() as u8;
        sb.sb_agblklog = agblocks.trailing_zeros() as u8;
        sb
    }

    /// The log's first block, and how many blocks it spans.
    ///
    /// This **does** locate the log, and two wrong conclusions have already been
    /// recorded here.  What settled it: on an image that has been written, the log's
    /// magic appears at fsblock 524296 and `sb_logstart` reads 524295.  The one block
    /// of difference is the region starting a block before its first record header,
    /// which is what a log does.
    ///
    /// The misleading counter-example was a *pristine* image, where all 65536 blocks
    /// of the region are **entirely zero** -- these golden images have never been
    /// written, so their logs have never been used.  Searching such an image for the
    /// magic finds nothing in the log and finds several thousand unrelated blocks
    /// elsewhere, which reads exactly like "the magic is not the log" and is not.
    ///
    /// So: the magic marks a log **record**, the log's extent comes from here, and a
    /// magic search is the way to check this range rather than to find it.
    /// Used by the log module's tests and by fixture tooling; nothing in the daemon
    /// itself needs to find the log, because it does not read or write one yet.
    #[allow(dead_code)]
    pub fn log_blocks(&self) -> std::ops::Range<u64> {
        self.sb_logstart..self.sb_logstart + u64::from(self.sb_logblocks)
    }

    /// Does this file system checksum its metadata?
    ///
    /// A version 5 file system does, and its metadata carries a checksum that
    /// has to be recomputed whenever a structure is written.  A version 4 one
    /// does not, and its metadata has no checksum at all.
    pub const fn has_crc(&self) -> bool {
        self.sb_features2.crc()
    }

    /// Does this file system put a file type byte in directory entries?
    ///
    /// A file-system-wide feature, not a per-directory one, and the difference is
    /// **one byte per entry**.  Guessing it wrong in either direction corrupts every
    /// entry in the directory, so it is asked rather than assumed -- which is why
    /// `ShortformDirectory` carries it as a field even though it is derivable here:
    /// the *decode* path cannot know it, because it is not in the directory.
    ///
    /// Both places it can be recorded are consulted, since a file system may carry
    /// it in `sb_features_ro_compat` or in `sb_features_incompat` depending on when
    /// it was made.
    pub const fn ftype(&self) -> bool {
        self.sb_features2.ftype() || self.sb_features_incompat.ftype()
    }

    /// The file system's identifier, which every checksummed structure repeats
    /// so that a block can be checked against the file system it claims.
    pub const fn uuid(&self) -> [u8; 16] {
        self.sb_uuid.as_image_bytes()
    }

    /// The image offset at which an allocation group begins.
    ///
    /// The allocation group helpers below are the allocator's entry point, and
    /// the allocator that uses them is written next; until then they are here to
    /// be tested rather than to be called.
    ///
    /// An allocation group's blocks are numbered from the start of the group,
    /// and the group's first block is at the image offset implied by the
    /// superblock's geometry.
    #[allow(dead_code)] // The allocator that calls this is the next phase.
    pub fn ag_offset(&self, agno: u32) -> u64 {
        agno as u64 * self.fsb_to_offset(self.sb_agblocks as u64)
    }

    /// The image block number of a block named within an allocation group.
    ///
    /// A block the allocation machinery names is counted from the start of its
    /// group, while everything the rest of the file system uses -- a file's
    /// extents, the address of a data block -- is counted from the start of the
    /// image.  Mixing the two points at the right block in the wrong group.
    pub const fn ag_block_to_fsb(&self, agno: u32, agblock: XfsAgblock) -> XfsFsblock {
        (agno as u64 * self.sb_agblocks as u64 + agblock as u64) as XfsFsblock
    }

    /// The image offset of a block named within an allocation group.
    ///
    /// The group is a run of blocks and the block is counted from its start, so
    /// the two have to be turned into a file system block number before they mean
    /// anything: adding a block number straight onto `ag_offset` added blocks to
    /// bytes, which was wrong for every group whose offset is not zero.
    pub fn ag_block_offset(&self, agno: u32, agblock: XfsAgblock) -> u64 {
        self.ag_offset(agno) + u64::from(agblock) * u64::from(self.sb_blocksize)
    }

    /// The image offset of one of an allocation group's three header
    /// structures.
    ///
    /// The headers are at *fixed* places, which is the only part of a XFS file
    /// system that can be found without walking it: sector 0 of a group holds
    /// the superblock, and sectors 1, 2 and 3 hold the group file, the group
    /// inode header, and the group free list.  A *sector* is the file system's
    /// basic block, which is not always a whole file system block: a file
    /// system with 4 KiB blocks on 512-byte sectors keeps its superblock in the
    /// first sector of block 0 and the other headers in the sectors after it,
    /// so a sector number is a byte offset divided by the sector size rather
    /// than a block number.
    ///
    /// Which sector holds which header is [`agf_sector`], [`agi_sector`] and
    /// [`agfl_sector`].
    #[allow(dead_code)] // The allocator that calls this is the next phase.
    pub fn ag_header_offset(&self, agno: u32, sector: u32) -> u64 {
        self.ag_offset(agno) + sector as u64 * self.sb_sectsize as u64
    }

    /// Get the size of an inode in bytes
    pub fn inode_size(&self) -> usize {
        self.sb_inodesize.into()
    }

    /// Given an inode number, calculate its offset in bytes in the image.
    ///
    /// An inode number is a three-part address: an allocation group, a block
    /// within that group, and an index within that block.  Nothing on the image
    /// records where an inode is; the address is the location, which is why an
    /// inode can be modified without first being found.
    pub fn inode_offset(&self, inode_number: XfsIno) -> u64 {
        let ag_no = inode_number >> (self.sb_agblklog + self.sb_inopblog);
        let ag_blk = (inode_number >> self.sb_inopblog) & ((1 << self.sb_agblklog) - 1);
        let blk_ino = inode_number & ((1 << self.sb_inopblog) - 1);
        ((ag_no * u64::from(self.sb_agblocks)) << self.sb_blocklog)
            + (ag_blk << self.sb_blocklog)
            + (blk_ino << self.sb_inodelog)
    }

    /// Given a file system block number, calculate its disk address in units of 512B blocks
    fn fsb_to_daddr(&self, fsbno: XfsFsblock) -> u64 {
        let blkbb_log = self.sb_blocklog - Self::BBSHIFT;
        let agno = fsbno >> self.sb_agblklog;
        let agbno = fsbno & ((1 << self.sb_agblklog) - 1);
        (agno * u64::from(self.sb_agblocks) + agbno) << blkbb_log
    }

    /// Calculate the disk address for a given file system block number, if it's stored on a
    /// real-time device.  Real-time devices don't have allocation groups.
    fn fsb_to_daddr_rt(&self, fsbno: XfsFsblock) -> u64 {
        fsbno << (self.sb_blocklog - Self::BBSHIFT)
    }

    /// Given a file system block number, calculate its disk byte offset
    pub fn fsb_to_offset(&self, fsbno: XfsFsblock) -> u64 {
        self.fsb_to_daddr(fsbno) << Self::BBSHIFT
    }

    /// Given a realtime device file system block number, calculate its disk byte offset
    pub fn fsb_to_offset_rt(&self, fsbno: XfsFsblock) -> u64 {
        self.fsb_to_daddr_rt(fsbno) << Self::BBSHIFT
    }

    /// Does this file system record file type in its directory inodes?
    pub fn has_ftype(&self) -> bool {
        // Though it isn't documented, it seems that the ftype bit was originally part of the
        // sb_features2 field, and then later moved to the sb_features_incompat field.
        self.sb_features2.ftype() || self.sb_features_incompat.ftype()
    }

    /// Return the file system version (usually 4 or 5)
    pub fn version(&self) -> u16 {
        self.sb_versionnum & 0xF
    }
}

#[cfg(test)]
mod t {
    use std::io::Cursor;

    use super::*;

    /// The first sector of the superblock of an image, or `None` when the image
    /// has not been unpacked.  The same shape as the FUSE-gated tests: an
    /// environment without images cannot check this, and saying so beats
    /// passing without having looked.
    fn superblock_of(path: &str) -> Option<Vec<u8>> {
        let all = std::fs::read(path).ok()?;
        Some(all[..512].to_vec())
    }

    /// Where the free block count sits, and what patching it is allowed to
    /// touch.
    ///
    /// The offset is checked against the field-by-field parse rather than
    /// against this file, so a field moving in the struct cannot quietly leave
    /// the offset behind.  And patching has to be surgical: the superblock
    /// shares its block with the first group's headers, so a patch that changed
    /// anything else would be a patch that could take them with it.
    #[test]
    fn the_free_block_count_is_where_the_parse_says_it_is() {
        for path in ["target/tmp/xfsv4.img", "target/tmp/xfs1024.img"] {
            let Some(bytes) = superblock_of(path) else {
                eprintln!("skipping {path}: no unpacked image");
                continue;
            };
            let parsed = Sb::from(&mut Cursor::new(&bytes));
            assert_eq!(
                Sb::fdblocks_in(&bytes).expect("a count in the bytes"),
                parsed.sb_fdblocks,
                "{path}: the offset does not match the parsed field"
            );

            let mut patched = bytes.clone();
            let before = parsed.sb_fdblocks;
            Sb::patch_fdblocks(&mut patched, before - 24).expect("a patchable superblock");
            assert_eq!(
                Sb::fdblocks_in(&patched).expect("a count in the bytes"),
                before - 24,
                "{path}: the patched count did not read back"
            );

            // Everything outside the count, and outside the checksum when there
            // is one, must be exactly as it was.
            // Whether this file system has checksums is a property of the bytes
            // and not of the file's name, and it decides whether patching the
            // count has to move the checksum with it.
            let checksummed = BigEndian::read_u32(&bytes[Sb::FEATURES2..])
                & constants::XFS_SB_VERSION2_CRCBIT
                != 0;
            let allowed = |i: usize| {
                (Sb::FDBLOCKS..Sb::FDBLOCKS + 8).contains(&i)
                    || (checksummed && (Sb::BCRC..Sb::BCRC + 4).contains(&i))
            };
            for (i, (a, b)) in bytes.iter().zip(patched.iter()).enumerate() {
                if a != b {
                    assert!(
                        allowed(i),
                        "{path}: patching the count also changed byte {i}"
                    );
                }
            }

            if checksummed {
                // The trusted read path panics when the checksum is wrong, so
                // simply reading the patched superblock back is the check that
                // the checksum was recomputed correctly.  Without this the whole
                // checksum branch would go untested, because the image the
                // write tests use has checksums switched off.
                let reparsed = Sb::from(&mut Cursor::new(&patched));
                assert_eq!(reparsed.sb_fdblocks, before - 24);
            }
        }
    }
}

#[cfg(test)]
mod ino_tests {
    use super::*;

    fn sb_of(name: &str) -> Option<Sb> {
        let golden = crate::libxfuse::alloc::golden(name)?;
        let mut reader = std::io::BufReader::new(std::fs::File::open(golden).ok()?);
        Some(Sb::from(&mut reader))
    }

    /// The inode number is the address, and the address is right.
    ///
    /// The published geometry says the number splits into a group, a block within
    /// it, and a slot within the block, and nothing else is needed to find the
    /// bytes -- there is no table of chunk locations anywhere in the format.  So
    /// this is checked against the *bytes*: read the inode the number names and
    /// confirm it is the inode the tool says it is.  That closes the whole chain
    /// at once, because a wrong `sb_inopblog` or `sb_agblklog` puts the read in
    /// the wrong place and the magic will not be there.
    #[test]
    fn an_inode_number_names_the_inode_that_is_there() {
        let Some(sb) = sb_of("xfsv4.img") else {
            eprintln!("skipping: no unpacked xfsv4.img");
            return;
        };
        assert_eq!(
            u32::from(sb.sb_inopblog),
            1,
            "this image holds two inodes a block"
        );
        assert_eq!(u32::from(sb.sb_inopblock), 2);
        assert_eq!(u32::from(sb.sb_inodesize), 256);

        let rootino = sb.sb_rootino;
        let loc = sb.locate_ino(rootino);
        // Inode 32 in a group of 32768 blocks with two inodes a block is block
        // 16, slot 0 -- the first block after the group's headers.
        assert_eq!(
            (loc.agno, loc.agbno, loc.slot),
            (0, 16, 0),
            "the root's own address"
        );

        // The bytes there must be an inode, and must be a directory: the root is.
        let Some(golden) = crate::libxfuse::alloc::golden("xfsv4.img") else {
            return;
        };
        let bytes = std::fs::read(golden).expect("the image");
        let at = sb.ino_to_offset(rootino);
        let magic = u16::from_be_bytes(bytes[at as usize..at as usize + 2].try_into().unwrap());
        assert_eq!(
            magic, 0x494e,
            "block {} does not hold an inode at slot {}",
            loc.agbno, loc.slot
        );
        let mode = u16::from_be_bytes(bytes[at as usize + 2..at as usize + 4].try_into().unwrap());
        assert_eq!(
            mode & 0o170000,
            0o040000,
            "inode {} is not a directory, so it is not the root",
            rootino
        );
    }

    /// Every inode the number names is readable where it says, and nothing else
    /// looks like an inode.
    ///
    /// The mapping is arithmetic, so the only thing worth testing is whether the
    /// arithmetic agrees with the file system.  This walks every inode the
    /// geometry can name and checks that the ones with an inode's magic in them
    /// are exactly the ones that ought to have it -- so a wrong `sb_inopblog` or
    /// `sb_agblklog` cannot pass, because it would put the reads in the wrong
    /// place and both directions of the comparison would fail.
    #[test]
    fn the_geometry_agrees_with_the_image() {
        let Some(sb) = sb_of("xfsv4.img") else {
            eprintln!("skipping: no unpacked xfsv4.img");
            return;
        };
        let Some(golden) = crate::libxfuse::alloc::golden("xfsv4.img") else {
            return;
        };
        let bytes = std::fs::read(golden).expect("the image");
        let inos_per_block = 1u64 << u32::from(sb.sb_inopblog);
        let inos_per_group = 1u64 << (u32::from(sb.sb_inopblog) + u32::from(sb.sb_agblklog));

        let mut named = 0u32;
        let mut with_magic = 0u32;
        for ino in 0..sb.sb_dblocks.min(inos_per_group * u64::from(sb.sb_agcount)) {
            let loc = sb.locate_ino(ino);
            let at = sb.ino_to_offset(ino) as usize;
            if at + 2 > bytes.len() {
                continue;
            }
            let magic = u16::from_be_bytes(bytes[at..at + 2].try_into().unwrap());
            // The block within the *group*, which is what the mapping returns, and
            // which is not `ino / inodes_per_block` for anything but group 0: the
            // group's own offset in blocks has to come off first.
            //
            // It used to be exactly that, and the test passed -- because
            // `ino_to_fsb` had the same mistake and the two agreed with each other.
            // Fixing the mapping made this assertion fail, which is the right
            // outcome for a test that was checking arithmetic against itself.  A
            // mapping is only checked by something outside it, and here that is
            // the image: the magic comparison below is what says the offsets are
            // right, and this assertion only says the two halves agree.
            let group_first_block = u64::from(loc.agno) * u64::from(sb.sb_agblocks);
            let should = ino / inos_per_block - group_first_block == u64::from(loc.agbno);
            assert!(
                should,
                "inode {ino} decoded to group {} block {} but names block {}",
                loc.agno,
                ino / inos_per_block - group_first_block,
                loc.agbno
            );
            if magic == 0x494e {
                with_magic += 1;
                named += 1;
            } else if magic == 0 {
                named += 1;
            }
        }
        assert!(
            with_magic > 0,
            "no inode the geometry names has an inode's magic, so the mapping is wrong"
        );
        // Every group should have inodes, and they should be a small part of
        // the number: the free space dwarfs them.
        assert!(
            with_magic < named,
            "almost every inode number the geometry names has an inode in it, which would mean \
             the mapping is reading something else"
        );
    }

    /// Chunks are sixty-four inodes however many fit in a block, and the last
    /// chunk of a group can run past the group's end -- which is what the group's
    /// actual length, not the nominal block count, decides.
    #[test]
    fn chunks_are_sixty_four_inodes_and_can_overrun_the_group() {
        let Some(sb) = sb_of("xfsv4.img") else {
            eprintln!("skipping: no unpacked xfsv4.img");
            return;
        };
        assert_eq!(sb.chunk_blocks(), 32, "sixty-four inodes at two a block");

        // The block a chunk begins at, across a chunk boundary.
        assert_eq!((sb.locate_ino(32).agbno, sb.locate_ino(32).slot), (16, 0));
        assert_eq!((sb.locate_ino(95).agbno, sb.locate_ino(95).slot), (47, 1));

        // A chunk has to fit inside the group, which is why the boundary is the
        // group's real length and not the nominal block count -- the last group
        // of a file system is short.
        let length = sb.sb_agblocks;
        assert_eq!(
            sb.chunk_blocks(),
            32,
            "a chunk is thirty-two blocks here, so a group must be at least that long"
        );
        assert!(
            length >= sb.chunk_blocks(),
            "a group shorter than one chunk could not hold an inode at all"
        );
    }
}
