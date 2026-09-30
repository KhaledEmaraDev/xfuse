/*
 * BSD 2-Clause License
 *
 * Copyright (c) 2026, Pedro Giffuni
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
//! The header of an allocation group: what the allocator reads to find out what
//! the group knows about itself.
//!
//! # What this is
//!
//! XFS divides a file system into equal-sized *allocation groups*, and divides
//! the blocks of a file among them.  Every group carries a fixed header that
//! says how large the group is, where its free space is recorded, and which
//! blocks are sitting in the group's free list.  That header is the
//! [`Agf`] this module reads.
//!
//! Almost nothing in an XFS file system is at a fixed address; the allocation
//! group headers are the exception, and they are the only way in.  Every other
//! structure is found by walking from one of them, which is why this module
//! comes first in the write path: there is nowhere else to start.
//!
//! # Where it is
//!
//! The group file is the *second sector* of the group.  Sector 0 holds the
//! superblock.  The numbers are sectors rather than blocks because a sector is
//! the file system's basic block, which on a file system with larger blocks
//! holds several headers:
//!
//! ```text
//! sector 0            the superblock
//! sector 1            the group file          (this structure)
//! sector 2            the group inode header
//! sector 3            the group free list
//! ```
//!
//! See [`Sb::ag_header_offset`](super::sb::Sb::ag_header_offset).
//!
//! # What it holds, and why the allocator cares
//!
//! * `bnoroot` is the block holding the root of the btree that records this
//!   group's free blocks, keyed by where each free run starts.  Reading the
//!   free space at all means walking that btree.
//! * `flfirst`, `fllast` and `flcount` describe a window of the group's free
//!   list, which is a flat array of block numbers in
//!   [`Agfl`](super::alloc::agfl::Agfl).  It is a small, pre-extracted supply
//!   of free blocks: taking one is a matter of moving the window, while
//!   searching the btree is a matter of a walk.  A group with an empty window
//!   has to be refilled from the btree.
//! * `freeblks` and `longest` are the group's free block count and the length
//!   of its longest free run.  Both are summaries of the btree, and a group can
//!   refuse to satisfy an allocation from them without walking anything.
//!
//! # Invariants worth knowing
//!
//! * The free list window is `flfirst ..= fllast` *inclusive*, and `flcount` is
//!   how many of those entries are live.  An empty window has `flcount == 0`.
//! * An entry that is not a block number is the null block, which XFS uses to
//!   mark a slot in the array that holds nothing.
//! * The structure's own checksum covers the whole file system block, with the
//!   checksum field itself read as zeroes, and is stored least significant
//!   byte first.  A file system that has the version 5 checksum feature has
//!   one here; a version 4 file system leaves the field at zero and there is
//!   nothing to verify.
//!
//! # Modifying it
//!
//! Like an inode, this is kept as its own bytes and changed in place, so that
//! the fields this program has no opinion about -- the reverse-mapping and
//! reference-count roots, the reserved space, any feature XFS may add -- are
//! preserved by construction rather than by remembering to copy them.
//!
//! Anything that changes it must go through a
//! [`Transaction`](super::transaction::Transaction), and the group file is one
//! of the structures the journal will have to log, because a change to it that
//! is not logged would be undone by recovery while its effects on the free
//! space survive.

use std::fmt;

use byteorder::{BigEndian, ByteOrder, LittleEndian};
use crc::{Crc, CRC_32_ISCSI};

use super::super::{
    definitions::{XfsAgblock, XfsFsblock},
    error::{FsError, FsResult},
    sb::Sb,
};

/// The magic number that opens a group file.
pub const XFS_AGF_MAGIC: u32 = 0x5841_4746; // "XAGF"

/// Byte offsets of the group file's fields.
///
/// The group file has the same shape in every file system version, because the
/// fields the newer features need were reserved rather than inserted: a version
/// 4 file system has zeroes where a version 5 one has a reverse-mapping root.
mod offset {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 4;
    pub const SEQNO: usize = 8;
    pub const LENGTH: usize = 12;
    pub const BNOROOT: usize = 16;
    pub const CNTROOT: usize = 20;
    pub const BNORELEVEL: usize = 28;
    pub const CNTRELEVEL: usize = 32;
    pub const FLFIRST: usize = 40;
    pub const FLLAST: usize = 44;
    pub const FLCOUNT: usize = 48;
    pub const FREEBLKS: usize = 52;
    pub const LONGEST: usize = 56;
    pub const BTREEBLKS: usize = 60;
    pub const UUID: usize = 64;
    pub const LSN: usize = 208;
    pub const CRC: usize = 216;
    /// The number of bytes the structure occupies, checksum and the trailing
    /// metadata uuid included.
    pub const LENGTH_OF_STRUCT: usize = 236;
}

/// The block, within a group, that one of the group's header structures
/// occupies.
///
/// The headers are at fixed *sectors*, and a sector is not necessarily a whole
/// file system block: on a file system with 4 KiB blocks and 512-byte sectors
/// the first four headers share block 0.  This converts a header's sector into
/// the block a reader has to fetch.
pub fn ag_header_block(sb: &Sb, sector: u32) -> XfsAgblock {
    (sector as u64 * sb.sectsize() as u64 / sb.sb_blocksize as u64) as XfsAgblock
}

/// The byte offset of a header within the block that holds it.
///
/// Most of a header's block belongs to something else, so reading one means
/// reading this many bytes from the start of the block.
pub fn ag_header_offset_in_block(sb: &Sb, sector: u32) -> u64 {
    sector as u64 * sb.sectsize() as u64 % sb.sb_blocksize as u64
}

/// The block that means "no block".
///
/// The free list's array is fixed in size, so the slots past its live window
/// are filled with this rather than being left to whatever was in the block
/// before.  An allocator that reads one has to treat it as empty space, not as
/// block 4294967295.
pub const NULL_AGBLOCK: XfsAgblock = u32::MAX;

/// The header of one allocation group.
///
/// This is the group's own bytes, with typed accessors.  See the [module
/// documentation](self) for where it is and what it is for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Agf {
    bytes: Box<[u8]>,
}

impl Agf {
    /// Take ownership of a group header's bytes.
    ///
    /// The bytes must be at least one file system block long, and must open
    /// with the group file's magic number: a block that does not is not a group
    /// header, and treating it as one would mean reading a free block count out
    /// of a file's data.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>, blocksize: usize) -> FsResult<Self> {
        let bytes = bytes.into();
        if bytes.len() < blocksize {
            return Err(FsError::corrupt(format!(
                "an allocation group header needs {blocksize} bytes, got {}",
                bytes.len()
            )));
        }
        let magic = BigEndian::read_u32(&bytes[offset::MAGIC..]);
        if magic != XFS_AGF_MAGIC {
            return Err(FsError::corrupt(format!(
                "expected an allocation group header and found magic {magic:#010x}"
            )));
        }
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
        })
    }

    /// The header's bytes, as they are on the image.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consume the header and take its bytes.
    pub fn into_bytes(self) -> Box<[u8]> {
        self.bytes
    }

    fn u32_at(&self, at: usize) -> u32 {
        BigEndian::read_u32(&self.bytes[at..])
    }

    fn set_u32_at(&mut self, at: usize, value: u32) {
        BigEndian::write_u32(&mut self.bytes[at..], value);
    }

    /// The version of the group header itself, which is not the version of the
    /// file system.
    pub fn version(&self) -> u32 {
        self.u32_at(offset::VERSION)
    }

    /// How many times this group has been written, as a counter that lets a
    /// reader notice that the group changed under it.
    pub fn seqno(&self) -> u32 {
        self.u32_at(offset::SEQNO)
    }

    /// How many blocks the group has, which is the file system's group size.
    ///
    /// The value is a summary: it is what the group header says, and a group
    /// whose header and geometry disagree is a damaged file system.
    pub fn length(&self) -> u32 {
        self.u32_at(offset::LENGTH)
    }

    /// The block holding the root of the btree of free blocks, keyed by where
    /// each free run starts.
    pub fn block_btree_root(&self) -> XfsAgblock {
        self.u32_at(offset::BNOROOT)
    }

    /// The block holding the root of the btree of free blocks, keyed by how
    /// long each free run is.
    ///
    /// Both btrees describe the same free space; they are searched in
    /// different orders, one for "a run that starts late" and one for "a run
    /// that is long".
    pub fn extent_btree_root(&self) -> XfsAgblock {
        self.u32_at(offset::CNTROOT)
    }

    /// How many levels deep the btree of free blocks is; 0 means the root is
    /// itself a leaf.
    pub fn block_btree_level(&self) -> u32 {
        self.u32_at(offset::BNORELEVEL)
    }

    /// How many levels deep the btree of free blocks is, keyed by length.
    pub fn extent_btree_level(&self) -> u32 {
        self.u32_at(offset::CNTRELEVEL)
    }

    /// The index into the free list's array of the first live entry.
    pub fn free_list_first(&self) -> u32 {
        self.u32_at(offset::FLFIRST)
    }

    /// The index into the free list's array of the last live entry.
    pub fn free_list_last(&self) -> u32 {
        self.u32_at(offset::FLLAST)
    }

    /// How many entries of the free list's array are live.
    pub fn free_list_count(&self) -> u32 {
        self.u32_at(offset::FLCOUNT)
    }

    /// How many blocks the group believes are free.
    pub fn free_blocks(&self) -> u32 {
        self.u32_at(offset::FREEBLKS)
    }

    /// The length of the longest run of free blocks the group knows of, which
    /// lets it refuse an allocation that cannot be satisfied without walking
    /// anything.
    pub fn longest_free(&self) -> u32 {
        self.u32_at(offset::LONGEST)
    }

    /// How many blocks the group's own btrees occupy, which is a count of
    /// blocks charged to the group file itself rather than to any file.
    pub fn btree_blocks(&self) -> u32 {
        self.u32_at(offset::BTREEBLKS)
    }

    /// The file system's identifier, which the header carries so that a block
    /// can be checked against the file system it claims to belong to.
    pub fn uuid(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out.copy_from_slice(&self.bytes[offset::UUID..offset::UUID + 16]);
        out
    }

    /// The log sequence number of the last change to this group.
    pub fn lsn(&self) -> u64 {
        BigEndian::read_u64(&self.bytes[offset::LSN..])
    }

    /// Is the group header's checksum correct?
    ///
    /// A file system without the version 5 checksum feature has no checksum to
    /// check, and this reports those as correct rather than as damaged.
    pub fn verify_crc(&self) -> bool {
        if self.stored_crc() == 0 {
            return true;
        }
        self.stored_crc() == self.computed_crc()
    }

    fn stored_crc(&self) -> u32 {
        LittleEndian::read_u32(&self.bytes[offset::CRC..])
    }

    fn computed_crc(&self) -> u32 {
        const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);
        let mut copy = self.bytes.to_vec();
        LittleEndian::write_u32(&mut copy[offset::CRC..], 0);
        CASTAGNOLI.checksum(&copy)
    }

    /// Recompute the group header's checksum, if the file system has one.
    pub fn update_crc(&mut self) {
        if self.stored_crc() == 0 {
            return;
        }
        let crc = self.computed_crc();
        LittleEndian::write_u32(&mut self.bytes[offset::CRC..], crc);
    }

    // --- the free list window, which is what an allocator changes ---

    /// Move the free list window so that it holds `count` entries starting at
    /// `first`.
    ///
    /// The window is written as a whole rather than adjusted field by field
    /// because the three fields describe one thing, and a window whose
    /// `first` is past its `last` is not a window at all.
    pub fn set_free_list_window(&mut self, first: u32, last: u32, count: u32) {
        self.set_u32_at(offset::FLFIRST, first);
        self.set_u32_at(offset::FLLAST, last);
        self.set_u32_at(offset::FLCOUNT, count);
    }

    /// Record how many blocks the group now has free.
    ///
    /// This is a summary of the free space btrees, kept so that a group can be
    /// passed over without being read.  It is not a second record of the same
    /// fact: the trees are, and this is a copy that can disagree if the trees
    /// are changed without it.  That is why every change to the trees changes
    /// this in the same transaction.
    pub fn set_free_blocks(&mut self, free: u32) {
        self.set_u32_at(offset::FREEBLKS, free);
    }

    /// Record the length of the group's longest run of free blocks.
    pub fn set_longest_free(&mut self, longest: u32) {
        self.set_u32_at(offset::LONGEST, longest);
    }

    /// Is the group's free list window consistent with itself?
    ///
    /// A group that has never handed out a block has no window, and neither
    /// has one that has been emptied; both report zero.  Anything else is a
    /// damaged header, and allocating from it would corrupt the group.
    pub fn free_list_window_is_sane(&self) -> bool {
        let (first, last, count) = (
            self.free_list_first(),
            self.free_list_last(),
            self.free_list_count(),
        );
        if count == 0 {
            return true;
        }
        first <= last
    }

    /// Check that the header is one this implementation can work with.
    ///
    /// This is where a file system that is being mounted read-write gets to say
    /// no: a group whose geometry disagrees with the superblock, or whose
    /// checksum does not verify, is not something to allocate in.
    pub fn check_usable(&self, sb: &Sb) -> FsResult<()> {
        if !self.verify_crc() {
            return Err(FsError::corrupt("allocation group header checksum"));
        }
        if self.length() != sb.sb_agblocks {
            return Err(FsError::corrupt(format!(
                "allocation group says it has {} blocks, the superblock says {}",
                self.length(),
                sb.sb_agblocks
            )));
        }
        if sb.has_crc() && self.uuid() != sb.uuid() {
            return Err(FsError::corrupt(
                "allocation group belongs to a different file system",
            ));
        }
        if !self.free_list_window_is_sane() {
            return Err(FsError::corrupt("allocation group free list window"));
        }
        Ok(())
    }

    /// The block number within the group of the group header itself.
    pub fn self_block(&self, sb: &Sb) -> XfsAgblock {
        ag_header_block(sb, Sb::AGF_SECTOR)
    }
}

impl fmt::Display for Agf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "AGF seq {} of {} blocks, {} free, longest run {}, btrees at {}/{}, free list {}+{} \
             with {} entries",
            self.seqno(),
            self.length(),
            self.free_blocks(),
            self.longest_free(),
            self.block_btree_root(),
            self.extent_btree_root(),
            self.free_list_first(),
            self.free_list_last(),
            self.free_list_count(),
        )
    }
}

/// A free space btree root, as a block number within a group.
pub type AgfBtreeRoot = XfsAgblock;

/// A file system block number for a block named within its group.
///
/// Allocation groups are numbered from zero, and a block number is relative to
/// the group, so a group-relative block becomes a file system block only by
/// adding the group's start.
pub const fn agf_root_to_fsb(agno: u32, agblocks: u32, agblock: XfsAgblock) -> XfsFsblock {
    (agno as u64 * agblocks as u64 + agblock as u64) as XfsFsblock
}

#[cfg(test)]
mod t {
    use super::*;

    /// The first 64 bytes of a version 4 group header, taken from the first
    /// group of `resources/xfsv4.img`.  Everything after them in that block is
    /// zero, so a test can pad it out.  The expectations below are what
    /// `xfs_db` prints for the same group, which is the point: the test is not
    /// checking this code against itself.
    const V4_AGF: &str = concat!(
        "58 41 47 46 00 00 00 01 00 00 00 00 00 00 80 00 ",
        "00 00 00 04 00 00 00 05 00 00 00 00 00 00 00 01 ",
        "00 00 00 01 00 00 00 00 00 00 00 01 00 00 00 04 ",
        "00 00 00 04 00 00 75 c0 00 00 73 58 00 00 00 00 ",
    );

    /// The first 224 bytes of a version 5 group header, from the first group of
    /// `resources/xfs_4kn.img`, which reaches the checksum at byte 216.  The
    /// rest of that block is zero.
    const V5_AGF: &str = concat!(
        "58 41 47 46 00 00 00 01 00 00 00 00 00 00 10 00 ",
        "00 00 00 04 00 00 00 05 00 00 00 00 00 00 00 01 ",
        "00 00 00 01 00 00 00 00 00 00 00 01 00 00 00 04 ",
        "00 00 00 04 00 00 0f e3 00 00 0f de 00 00 00 00 ",
        "8d 0c 39 d3 96 de 47 ef a4 76 1c 07 14 0c b9 36 ",
        "00 00 00 00 00 00 00 01 00 00 00 08 00 00 00 01 ",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ",
        "00 00 00 01 00 00 00 08 1a 1d 7d 5c 00 00 00 00 ",
    );

    /// Turn the rows above into a block of the file system's size.
    ///
    /// `concat!` leaves the rows as one string of hex bytes separated by
    /// spaces, so the whole thing splits the same way either.
    fn block_of(hex_rows: &str, blocksize: usize) -> Vec<u8> {
        let mut out: Vec<u8> = hex_rows
            .split_whitespace()
            .map(|byte| u8::from_str_radix(byte, 16).expect("two hex digits per byte"))
            .collect();
        assert!(out.len() <= blocksize, "fixture is larger than a block");
        out.resize(blocksize, 0);
        out
    }

    /// A version 4 group header, decoded, must agree with the reference
    /// implementation about every field.
    #[test]
    fn real_v4_agf_decodes() {
        let agf = Agf::from_bytes(block_of(V4_AGF, 512), 512).unwrap();
        assert_eq!(agf.version(), 1);
        assert_eq!(agf.seqno(), 0);
        assert_eq!(agf.length(), 32768);
        assert_eq!(agf.block_btree_root(), 4);
        assert_eq!(agf.extent_btree_root(), 5);
        assert_eq!(agf.block_btree_level(), 1);
        assert_eq!(agf.extent_btree_level(), 1);
        assert_eq!(agf.free_list_first(), 1);
        assert_eq!(agf.free_list_last(), 4);
        assert_eq!(agf.free_list_count(), 4);
        assert_eq!(agf.free_blocks(), 30144);
        assert_eq!(agf.longest_free(), 29528);
        assert_eq!(agf.btree_blocks(), 0);
    }

    /// The same for a version 5 header, whose group is a different size and
    /// whose checksum has to verify.
    #[test]
    fn real_v5_agf_decodes() {
        let agf = Agf::from_bytes(block_of(V5_AGF, 4096), 4096).unwrap();
        assert_eq!(agf.version(), 1);
        assert_eq!(agf.length(), 4096);
        assert_eq!(agf.block_btree_root(), 4);
        assert_eq!(agf.extent_btree_root(), 5);
        assert_eq!(agf.free_list_first(), 1);
        assert_eq!(agf.free_list_last(), 4);
        assert_eq!(agf.free_list_count(), 4);
        assert_eq!(agf.free_blocks(), 4067);
        assert_eq!(agf.longest_free(), 4062);
        assert_eq!(agf.lsn(), 0x0000_0001_0000_0008);
        assert!(
            agf.verify_crc(),
            "a real version 5 group header must verify"
        );
    }

    /// A file system that has no checksums has nothing to verify, and saying
    /// otherwise would make every version 4 image look damaged.
    #[test]
    fn a_version_4_header_has_no_checksum_to_verify() {
        let agf = Agf::from_bytes(block_of(V4_AGF, 512), 512).unwrap();
        assert!(agf.verify_crc());
    }

    /// One changed byte anywhere in a version 5 header has to be caught, since
    /// a header whose counters do not match its btrees would hand the same
    /// block out twice.
    #[test]
    fn a_changed_byte_breaks_the_checksum() {
        let mut bytes = block_of(V5_AGF, 4096);
        let agf = Agf::from_bytes(bytes.clone(), 4096).unwrap();
        assert!(agf.verify_crc());
        // A counter, in the middle of the free list window.
        bytes[48] ^= 0x40;
        let agf = Agf::from_bytes(bytes.clone(), 4096).unwrap();
        assert!(
            !agf.verify_crc(),
            "a changed counter must break the checksum"
        );
        // A field the checksum covers but nothing here reads.
        bytes[48] ^= 0x40;
        bytes[200] ^= 0x01;
        let agf = Agf::from_bytes(bytes, 4096).unwrap();
        assert!(
            !agf.verify_crc(),
            "a change in the fields this code has no opinion about must still be caught"
        );
    }

    /// Moving the free list window has to move all three numbers together.
    #[test]
    fn the_window_is_three_numbers() {
        let mut agf = Agf::from_bytes(block_of(V4_AGF, 512), 512).unwrap();
        agf.set_free_list_window(3, 7, 5);
        assert_eq!(agf.free_list_first(), 3);
        assert_eq!(agf.free_list_last(), 7);
        assert_eq!(agf.free_list_count(), 5);
        assert!(agf.free_list_window_is_sane());
    }

    /// A window that is not a window is a damaged header, and allocating from
    /// one would hand out blocks that are still in use.
    #[test]
    fn a_nonsensical_window_is_rejected() {
        let mut agf = Agf::from_bytes(block_of(V4_AGF, 512), 512).unwrap();
        agf.set_free_list_window(9, 4, 6);
        assert!(!agf.free_list_window_is_sane());
        agf.set_free_list_window(0, 0, 0);
        assert!(
            agf.free_list_window_is_sane(),
            "an empty window is not a mistake"
        );
    }

    /// The two numbers a group keeps so that a group can be passed over
    /// without being read, and which move with the trees.
    #[test]
    fn the_group_records_what_is_left() {
        let mut agf = Agf::from_bytes(block_of(V4_AGF, 512), 512).unwrap();
        assert_eq!(agf.free_blocks(), 30144);
        assert_eq!(agf.longest_free(), 29528);
        agf.set_free_blocks(30000);
        agf.set_longest_free(20000);
        assert_eq!(agf.free_blocks(), 30000);
        assert_eq!(agf.longest_free(), 20000);
        // And the bytes it was decoded from are the bytes that changed, so that
        // writing it back writes what was set.
        assert_eq!(agf, Agf::from_bytes(agf.as_bytes().to_vec(), 512).unwrap());
    }

    /// Anything that is not a group header has to be refused rather than read.
    #[test]
    fn other_blocks_are_not_group_headers() {
        let mut bytes = block_of(V4_AGF, 512);
        bytes[0] = 0x42; // the magic of a block bitmap
        assert!(Agf::from_bytes(bytes.clone(), 512).is_err());
        assert!(Agf::from_bytes(vec![0u8; 64], 512).is_err());
    }
}
