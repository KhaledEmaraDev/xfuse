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
//! B+tree for a file's extent map (BMBT).
//!
//! A file's data fork holds either a list of extents (in the inode) or a B+tree
//! (the BMBT) rooted in the inode and spilling into blocks.  This module
//! provides the on-disk block structures, reading, and bounded construction.
//!
//! # Algorithm reference
//!
//! See [`docs/xfs-algorithms.md`] sections 5--6 (File extent maps, BMBT
//! algorithms) for the algorithm map with DOCUMENTED/MEASURED/IMPLEMENTED/
//! HYPOTHESIS labels.
use std::{
    cell::RefCell,
    collections::{btree_map::Entry, BTreeMap},
    io::{prelude::*, SeekFrom},
    marker::PhantomData,
};

use bincode_next::{
    de::{read::Reader, Decoder},
    error::DecodeError,
    Decode,
};
use num_traits::{PrimInt, Unsigned};

use super::{
    alloc::allocator::allocate_in_group,
    block_device::BlockDevice,
    bmbt_rec::{BmbtRec, Bmx},
    definitions::{XfsFileoff, XfsFsblock, XfsIno, XFS_BMAP_CRC_MAGIC, XFS_BMAP_MAGIC},
    error::FsError,
    inode::{encode_extent, offset, RawDinode, EXTENT_REC_SIZE},
    sb::Sb,
    transaction::Transaction,
    utils::{decode, decode_from, Uuid},
    volume::SUPERBLOCK,
};
use byteorder::{BigEndian, ByteOrder};
use std::sync::Arc;

/// Trait for readers that can provide access to the underlying block device.
pub trait DeviceReader: bincode_next::de::read::Reader + BufRead + Seek {
    fn device(&self) -> Arc<BlockDevice>;
}

#[derive(Clone, Copy, Debug)]
pub struct BtreeBlockHdr<T: PrimInt + Unsigned> {
    bb_magic: u32,
    pub bb_level: u16,
    pub bb_numrecs: u16,
    //_bb_leftsib: T,
    //_bb_rightsib: T,
    _phantom: PhantomData<T>,
    // Below fields are for V5 file systems only
    //_bb_blkno: u64,
    //_bb_lsn: u64,
    //_bb_uuid: Uuid,
    //_bb_owner: u64,
    //_bb_crc: u32,
    //_bb_pad: u32,
}

impl<T: Decode<Ctx> + PrimInt + Unsigned, Ctx> Decode<Ctx> for BtreeBlockHdr<T> {
    fn decode<D: Decoder<Context = Ctx>>(decoder: &mut D) -> Result<Self, DecodeError> {
        let bb_magic: u32 = Decode::decode(decoder)?;
        let bb_level = Decode::decode(decoder)?;
        let bb_numrecs = Decode::decode(decoder)?;
        let _bb_leftsib: T = Decode::decode(decoder)?;
        let _bb_rightsib: T = Decode::decode(decoder)?;
        // A magic this is not, a file system identifier that is not this file
        // system's, and a level that does not match the depth it was reached at
        // are all faults in an image somebody else wrote.  They were `panic!` and
        // `assert_eq!`, which is a crash rather than a diagnosis.
        match bb_magic {
            XFS_BMAP_MAGIC => {}
            XFS_BMAP_CRC_MAGIC => {
                let _bb_blkno: u64 = Decode::decode(decoder)?;
                let _bb_lsn: u64 = Decode::decode(decoder)?;
                let bb_uuid: Uuid = Decode::decode(decoder)?;
                let _bb_owner: u64 = Decode::decode(decoder)?;
                let _bb_crc: u32 = Decode::decode(decoder)?;
                let _bb_pad: u32 = Decode::decode(decoder)?;
                if let Some(super_block) = SUPERBLOCK.get() {
                    if bb_uuid != super_block.sb_uuid {
                        return Err(DecodeError::OtherString(
                            "a b-tree block belongs to another file system".into(),
                        ));
                    }
                }
            }
            other => {
                return Err(DecodeError::OtherString(format!(
                    "a b-tree block carries the unexpected magic {other:#x}"
                )));
            }
        };
        Ok(BtreeBlockHdr {
            bb_magic,
            bb_level,
            bb_numrecs,
            _phantom: PhantomData,
        })
    }
}

/// The magic a b-map block carries, and the one a version 5 block carries.
///
/// `BMAP`, read out of three real leaves in `xfsv4.img` -- and, for what it is
/// worth, the same value `xfs_db`'s own type table calls `bmapbta`/`bmapbtd`.
pub const BMBT_MAGIC: u32 = XFS_BMAP_MAGIC;

/// Bytes from the start of a non-CRC b-map block to its first record.
///
/// A long-form header without a checksum: a magic, a level and a record count,
/// then a left and a right sibling of eight bytes each, which is
/// `XFS_BTREE_LBLOCK_LEN` in the published XFS documentation (XFS Algorithms and Data Structures, and the XFS pages at docs.kernel.org).
///
/// This is the length a b-map block had before `xfs_repair` added a checksum to
/// it, and it is *not* the length any image here has.  See
/// [`BMBT_CRC_HEADER_LEN`], which is the one that is measured.
pub const BMBT_HEADER_LEN: usize = 24;

/// Bytes from the start of a checksummed b-map block to its first record.
///
/// `XFS_BTREE_LBLOCK_CRC_LEN`: the eight bytes of magic, level and count, then a
/// long-form header carrying a block number, an LSN, the file system's UUID, the
/// owning inode, the checksum and four bytes of padding.
///
/// This is 72, and reading the records from offset 24 instead -- which is what
/// this used to do -- reads them from the middle of the header.  Nothing there
/// looks like a record, which is why it went unnoticed: the values it produced
/// were small, orderly and wrong.
///
/// Which of the two lengths a block has is decided by its magic, not guessed:
/// see [`BmbtLeafBlock::from_bytes`].
pub const BMBT_CRC_HEADER_LEN: usize = 72;

/// Bytes per record in a b-map leaf.
///
/// Sixteen: two eight-byte words.  The stride is the record size, but the
/// record is *not* four four-byte fields, which is what reading it as one
/// amounts to.  See [`BmbtLeafRecord`].
pub const BMBT_RECORD_LEN: usize = 16;

/// Where in a b-map record the extent flag sits, and its width.
///
/// `l0:63`, one bit, and it is one only for an extent that is not an ordinary
/// mapped run.
pub const BMBT_EXNTFLAG_BITLEN: u32 = 1;

/// Where in a b-map record the file offset sits, and its width.
///
/// `l0:9-62`, fifty-four bits, counted in blocks.
pub const BMBT_STARTOFF_BITLEN: u32 = 54;

/// Where in a b-map record the block count sits, and its width.
///
/// `l1:0-20`, twenty-one bits, counted in blocks.  It is twenty-one because that
/// is as long as an extent may be: the length was a signed field once, and the
/// sign bit is still spent.
pub const BMBT_BLOCKCOUNT_BITLEN: u32 = 21;

/// The five values in a long-form checksummed b-tree header that are not counts.
///
/// They are gathered into one value because they are one question -- "which block
/// is this, in what file system, owned by whom, and next to what" -- and a writer
/// that took eight arguments to ask it would be a writer whose callers could not
/// remember the order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BtreeLblockHdr {
    /// The block's **own** number.
    pub blkno: XfsFsblock,
    /// The log sequence number; see [`BmbtLeafBlock::to_bytes`].
    pub lsn: u64,
    pub leftsib: XfsFsblock,
    pub rightsib: XfsFsblock,
    /// The inode whose fork the tree belongs to.
    pub owner: XfsIno,
    /// The file system's identifier.
    pub uuid: [u8; 16],
}

impl BtreeLblockHdr {
    /// A header for a tree with one leaf and no log sequence number: the two
    /// siblings are null and the rest is what the block is.
    #[allow(dead_code)]
    pub const fn for_single_leaf(blkno: XfsFsblock, owner: XfsIno, uuid: [u8; 16]) -> Self {
        Self {
            blkno,
            lsn: 0,
            leftsib: BMBT_NULL_PTR,
            rightsib: BMBT_NULL_PTR,
            owner,
            uuid,
        }
    }
}

/// The sibling pointer a b-tree node has when it has no sibling.
///
/// `NULLAGINO` widened to the eight bytes the long form uses, which is every bit
/// set -- not zero, which is a block number.
#[allow(dead_code)] // The conversion writes a single-leaf tree, which is this.
pub const BMBT_NULL_PTR: XfsFsblock = u64::MAX;

/// A CRC-32C over a whole block, with the checksum's own four bytes read as zeroes.
///
/// Little significant byte first, at `at`, which is where every checksummed
/// structure in this file system keeps it.
fn crc32c_without_its_own_field(bytes: &[u8], at: usize) -> u32 {
    let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISCSI);
    let mut buf = bytes.to_vec();
    buf[at..at + 4].fill(0);
    crc.checksum(&buf)
}

/// Where in a b-map record the low half of the start block sits, and its width.
///
/// `l0:0-8`, nine bits.
pub const BMBT_STARTBLOCK_LOW_BITLEN: u32 = 9;

/// Where in a b-map record the high half of the start block sits, and its width.
///
/// `l1:21-63`, forty-three bits.  Nine and forty-three make the fifty-two bits
/// the start block has between the two words.
pub const BMBT_STARTBLOCK_HIGH_BITLEN: u32 = 43;

/// How far the stored start block is from the block number the rest of this
/// program uses.
///
/// `l0:0-8` together with `l1:21-63` make a fifty-two-bit start block, and in
/// every leaf of `xfs4096.img` that fifty-two-bit field is the file system block
/// number scaled up by 512: a record holding start block 17833 holds
/// 17833 * 512, and 17833 * 512 >> 9 is 17833 again.  Read unshifted it reads as
/// a block number 512 times too large, which lands outside the image.
pub const BMBT_STARTBLOCK_SCALE_SHIFT: u32 = 9;

/// One extent as a b-map leaf records it: two eight-byte words.
///
/// The fields are not byte-aligned within them.  the published XFS documentation (XFS Algorithms and Data Structures, and the XFS pages at docs.kernel.org) says where they
/// are, and this is that layout rather than a guess:
///
/// ```text
///   l0:63      is an extent flag (one means the extent is not an ordinary run)
///   l0:9-62    are the startoff, in blocks
///   l0:0-8     and l1:21-63 together are the start block
///   l1:0-20    are the block count, in blocks
/// ```
///
/// The start block straddles the two words because the record is squeezed: an
/// offset wants fifty-four bits, a block count twenty-one and a flag one, and
/// fifty-four plus fifty-two plus twenty-one plus one is 128, which is exactly the
/// record.  Nothing is spare, so the fields interleave rather than sit beside
/// each other.
///
/// [`BmbtLeafRecord::as_extent`] is the only way to read one, and it is the only
/// thing in this file that turns a record into a block number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BmbtLeafRecord {
    pub l0: u64,
    pub l1: u64,
}

impl BmbtLeafRecord {
    /// This record's extent flag: is this an ordinary mapped run?
    pub const fn extent_flag(&self) -> bool {
        self.l0 >> (64 - BMBT_EXNTFLAG_BITLEN) != 0
    }

    /// This record's file offset, in blocks.
    pub const fn startoff(&self) -> u64 {
        (self.l0 >> BMBT_STARTBLOCK_LOW_BITLEN) & ((1 << BMBT_STARTOFF_BITLEN) - 1)
    }

    /// This record's block count, in blocks.
    pub const fn blockcount(&self) -> u64 {
        self.l1 & ((1 << BMBT_BLOCKCOUNT_BITLEN) - 1)
    }

    /// This record's start block, as the rest of this program counts blocks.
    ///
    /// The stored field is the block number scaled by
    /// [`BMBT_STARTBLOCK_SCALE_SHIFT`] bits; this undoes that.
    pub const fn startblock(&self) -> u64 {
        let low = self.l0 & ((1 << BMBT_STARTBLOCK_LOW_BITLEN) - 1);
        let high = (self.l1 >> BMBT_BLOCKCOUNT_BITLEN) & ((1 << BMBT_STARTBLOCK_HIGH_BITLEN) - 1);
        ((high << BMBT_STARTBLOCK_LOW_BITLEN) | low) >> BMBT_STARTBLOCK_SCALE_SHIFT
    }

    /// The record that records this extent, which is its inverse.
    ///
    /// The scale and the bit layout are [`startoff`]'s, [`startblock`]'s and
    /// [`blockcount`]'s read backwards, and the one that is easy to get wrong is
    /// the start block: it is written into `l1`'s high bits **already scaled**, so
    /// it is `l1 >> 21` and not a value assembled from both words.  `l0`'s low nine
    /// bits -- nominally the block number's low bits -- are left zero, which is
    /// what every leaf measured here holds, and the round trip in the allocator's
    /// tests is what pins that rather than this comment.
    #[allow(dead_code)] // Used as soon as a data fork can grow into a B+tree.
    pub const fn from_extent(e: &BmbtRec) -> Self {
        let mut l0 =
            (e.br_startoff & ((1 << BMBT_STARTOFF_BITLEN) - 1)) << BMBT_STARTBLOCK_LOW_BITLEN;
        if e.br_flag {
            l0 |= 1 << (64 - BMBT_EXNTFLAG_BITLEN);
        }
        let startblock = e.br_startblock & ((1 << BMBT_STARTBLOCK_HIGH_BITLEN) - 1);
        let l1 = (startblock << BMBT_BLOCKCOUNT_BITLEN)
            | (e.br_blockcount & ((1 << BMBT_BLOCKCOUNT_BITLEN) - 1));
        Self { l0, l1 }
    }

    /// These two records' bytes, big endian, which is how a leaf holds them.
    pub fn to_bytes(self) -> [u8; BMBT_RECORD_LEN] {
        let mut out = [0u8; BMBT_RECORD_LEN];
        out[..8].copy_from_slice(&self.l0.to_be_bytes());
        out[8..].copy_from_slice(&self.l1.to_be_bytes());
        out
    }

    /// These two words read out of a leaf's bytes.
    #[allow(dead_code)] // Used as soon as a data fork can grow into a B+tree.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut l0 = [0u8; 8];
        let mut l1 = [0u8; 8];
        l0.copy_from_slice(&bytes[..8]);
        l1.copy_from_slice(&bytes[8..16]);
        Self {
            l0: u64::from_be_bytes(l0),
            l1: u64::from_be_bytes(l1),
        }
    }

    /// This record as the extent the rest of this program works with.
    pub const fn as_extent(&self) -> BmbtRec {
        BmbtRec {
            br_startoff: self.startoff(),
            br_startblock: self.startblock(),
            br_blockcount: self.blockcount(),
            br_flag: self.extent_flag(),
        }
    }
}

/// A b-map leaf block: the header, and the extents it holds.
#[derive(Debug, Clone)]
pub struct BmbtLeafBlock {
    pub level: u16,
    pub records: Vec<BmbtLeafRecord>,
}

/// A b-map interior node: keys and pointers instead of records.
///
/// The same block header a leaf has, and the same 16-byte stride, but what is in
/// the body is different: **keys first, then pointers, with the padding between
/// them that a node's own layout implies**.  A leaf holds records; this holds one
/// eight-byte key per child and one eight-byte pointer per child, the key being
/// the first file offset in that child.
///
/// It is written as a block in its own right rather than as a second kind of leaf,
/// because that is what it is: a block allocated for the purpose, carrying the
/// owning inode and the file system's UUID, with a checksum.
#[derive(Debug, Clone)]
pub struct BmbtInteriorBlock {
    pub level: u16,
    /// One per child, ascending; `keys[i]` is the first offset in child `i`.
    pub keys: Vec<BmbtKey>,
    /// One per child, in the same order.
    pub ptrs: Vec<XfsBmbtPtr>,
}

impl BmbtInteriorBlock {
    /// The most children one node can hold in a block of this shape.
    pub const fn max_children(sb_blocksize: usize, has_crc: bool) -> usize {
        // Half the body for keys and half for pointers, rounded to the node's own
        // alignment -- the same reasoning as a leaf's occupancy, with two halves
        // instead of one, which is why the answer is roughly half a leaf's records
        // times two and is not simply `body / 16`.
        let body = sb_blocksize.saturating_sub(if has_crc {
            BMBT_CRC_HEADER_LEN
        } else {
            BMBT_HEADER_LEN
        });
        (body / 2) / BmbtKey::SIZE
    }

    /// This node as the bytes of a checksummed block.
    #[allow(dead_code)] // Used as soon as a tree is deep enough to need one.
    pub fn to_bytes(&self, hdr: &BtreeLblockHdr, sb_blocksize: usize) -> Vec<u8> {
        let mut b = vec![0u8; sb_blocksize];
        put_header(
            &mut b,
            XFS_BMAP_CRC_MAGIC,
            self.level,
            self.keys.len(),
            hdr,
            sb_blocksize,
        );
        let room = Self::max_children(sb_blocksize, true);
        let mut at = BMBT_CRC_HEADER_LEN;
        for k in &self.keys {
            b[at..at + 8].copy_from_slice(&k.br_startoff.to_be_bytes());
            at += BmbtKey::SIZE;
        }
        at = BMBT_CRC_HEADER_LEN + room * BmbtKey::SIZE;
        for p in &self.ptrs {
            b[at..at + 8].copy_from_slice(&p.to_be_bytes());
            at += 8;
        }
        let crc = crc32c_without_its_own_field(&b, 64);
        b[64..68].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// This node as the bytes of a block on a file system without checksums.
    #[allow(dead_code)] // Used as soon as a tree is deep enough to need one.
    pub fn to_bytes_v4(
        &self,
        leftsib: XfsFsblock,
        rightsib: XfsFsblock,
        sb_blocksize: usize,
    ) -> Vec<u8> {
        let mut b = vec![0u8; sb_blocksize];
        put_header(
            &mut b,
            XFS_BMAP_MAGIC,
            self.level,
            self.keys.len(),
            &BtreeLblockHdr {
                blkno: 0,
                lsn: 0,
                leftsib,
                rightsib,
                owner: 0,
                uuid: [0; 16],
            },
            sb_blocksize,
        );
        let room = Self::max_children(sb_blocksize, false);
        let mut at = BMBT_HEADER_LEN;
        for k in &self.keys {
            b[at..at + 8].copy_from_slice(&k.br_startoff.to_be_bytes());
            at += BmbtKey::SIZE;
        }
        at = BMBT_HEADER_LEN + room * BmbtKey::SIZE;
        for p in &self.ptrs {
            b[at..at + 8].copy_from_slice(&p.to_be_bytes());
            at += 8;
        }
        b
    }

    /// Serialize this node for the file system described by `has_crc`.
    pub fn to_bytes_auto(
        &self,
        hdr: &BtreeLblockHdr,
        sb_blocksize: usize,
        has_crc: bool,
    ) -> Vec<u8> {
        if has_crc {
            self.to_bytes(hdr, sb_blocksize)
        } else {
            self.to_bytes_v4(hdr.leftsib, hdr.rightsib, sb_blocksize)
        }
    }

    /// Insert a key-pointer pair into this intermediate node, returning the split result if it overflows.
    /// Returns None if inserted without splitting, or Some((new_node, separator_key)) if split.
    #[allow(dead_code)]
    pub fn insert_key_ptr(
        &mut self,
        key: BmbtKey,
        ptr: XfsBmbtPtr,
        sb_blocksize: usize,
        has_crc: bool,
    ) -> Option<(BmbtInteriorBlock, BmbtKey)> {
        let max = Self::max_children(sb_blocksize, has_crc);
        if self.keys.len() < max {
            // Room for one more - insert in order by key
            let idx = self
                .keys
                .partition_point(|k| k.br_startoff < key.br_startoff);
            self.keys.insert(idx, key);
            self.ptrs.insert(idx, ptr);
            return None;
        }

        // Node is full - need to split
        let mut all_keys = self.keys.clone();
        let mut all_ptrs = self.ptrs.clone();
        let idx = all_keys.partition_point(|k| k.br_startoff < key.br_startoff);
        all_keys.insert(idx, key);
        all_ptrs.insert(idx, ptr);

        // Split in half
        let mid = all_keys.len() / 2;
        let separator_key = all_keys[mid].clone();
        let new_keys = all_keys.split_off(mid);
        let new_ptrs = all_ptrs.split_off(mid);

        let new_node = BmbtInteriorBlock {
            level: self.level,
            keys: new_keys,
            ptrs: new_ptrs,
        };

        self.keys = all_keys;
        self.ptrs = all_ptrs;

        Some((new_node, separator_key))
    }

    /// Remove a key-pointer pair from this intermediate node by key.
    /// Returns true if removal caused the node to fall below minimum occupancy.
    #[allow(dead_code)]
    pub fn remove_key_ptr(
        &mut self,
        key: BmbtKey,
        sb_blocksize: usize,
        has_crc: bool,
    ) -> Result<bool, i32> {
        let min = Self::min_children(sb_blocksize, has_crc);
        let idx = self
            .keys
            .partition_point(|k| k.br_startoff < key.br_startoff);
        if idx >= self.keys.len() || self.keys[idx].br_startoff != key.br_startoff {
            return Err(libc::ENOENT);
        }
        self.keys.remove(idx);
        self.ptrs.remove(idx);
        // Return true if we need to merge/redistribute
        Ok(self.keys.len() < min)
    }

    /// Merge this node with another node (right sibling).
    #[allow(dead_code)]
    pub fn merge_with(&mut self, other: BmbtInteriorBlock) {
        self.keys.extend(other.keys);
        self.ptrs.extend(other.ptrs);
    }

    /// Redistribute key-pointer pairs with a sibling to maintain minimum occupancy.
    #[allow(dead_code)]
    pub fn redistribute_with(
        &mut self,
        other: &mut BmbtInteriorBlock,
        _sb_blocksize: usize,
        _has_crc: bool,
    ) -> bool {
        let total = self.keys.len() + other.keys.len();
        let min = Self::min_children(0, false);
        if total < 2 * min {
            // Can't redistribute while maintaining minimum
            return false;
        }
        // Combine and split evenly
        let mut all_keys = self.keys.clone();
        all_keys.append(&mut other.keys);
        let mut all_ptrs = self.ptrs.clone();
        all_ptrs.append(&mut other.ptrs);
        // Re-sort by key
        let combined: Vec<_> = all_keys.into_iter().zip(all_ptrs).collect();
        let mut combined_sorted = combined;
        combined_sorted.sort_by_key(|(k, _)| k.br_startoff);
        let mid = combined_sorted.len() / 2;
        let (new_self_keys, new_self_ptrs): (Vec<_>, Vec<_>) =
            combined_sorted[..mid].iter().cloned().unzip();
        let (new_other_keys, new_other_ptrs): (Vec<_>, Vec<_>) =
            combined_sorted[mid..].iter().cloned().unzip();
        self.keys = new_self_keys;
        self.ptrs = new_self_ptrs;
        other.keys = new_other_keys;
        other.ptrs = new_other_ptrs;
        true
    }

    /// The minimum number of children this node can have (when not root).
    pub const fn min_children(sb_blocksize: usize, has_crc: bool) -> usize {
        Self::max_children(sb_blocksize, has_crc) / 2
    }
}

/// Write the header both node writers share.
#[allow(clippy::too_many_arguments)]
fn put_header(
    b: &mut [u8],
    magic: u32,
    level: u16,
    numrecs: usize,
    hdr: &BtreeLblockHdr,
    _sb_blocksize: usize,
) {
    b[0..4].copy_from_slice(&magic.to_be_bytes());
    b[4..6].copy_from_slice(&level.to_be_bytes());
    b[6..8].copy_from_slice(&(numrecs as u16).to_be_bytes());
    b[8..16].copy_from_slice(&hdr.leftsib.to_be_bytes());
    b[16..24].copy_from_slice(&hdr.rightsib.to_be_bytes());
    if hdr.uuid != [0; 16] || hdr.owner != 0 {
        b[24..32].copy_from_slice(&hdr.blkno.to_be_bytes());
        b[32..40].copy_from_slice(&hdr.lsn.to_be_bytes());
        b[40..56].copy_from_slice(&hdr.uuid);
        b[56..64].copy_from_slice(&hdr.owner.to_be_bytes());
    }
}

impl BmbtLeafBlock {
    /// Read a leaf block's bytes.
    ///
    /// The records are [`BMBT_RECORD_LEN`] bytes apart, beginning at
    /// [`BMBT_CRC_HEADER_LEN`] in a checksummed block and [`BMBT_HEADER_LEN`] in
    /// one without.  Which a block is, its magic says: the published XFS documentation (XFS Algorithms and Data Structures, and the XFS pages at docs.kernel.org) gives a
    /// version 5 block [`XFS_BMAP_CRC_MAGIC`] and an older one [`BMBT_MAGIC`],
    /// and the checksummed header is the longer one, so the magic that carries a
    /// checksum is the magic that costs forty-eight more bytes of header.
    ///
    /// Both are needed.  `xfs4096.img` and `xfs1024.img` are version 5 and every
    /// b-map block in them is checksummed; `xfsv4.img` is version 4 and none of
    /// its hundred-odd b-map blocks is.
    ///
    /// A block that is too short for what its header claims, carries a magic this
    /// is not, or has more records than fit, is a **corrupt** structure rather than
    /// a reason to read past the end of a buffer or to panic.
    pub fn from_bytes(bytes: &[u8]) -> crate::libxfuse::error::FsResult<Self> {
        use crate::libxfuse::error::FsError;
        if bytes.len() < BMBT_HEADER_LEN {
            return Err(FsError::Corrupt {
                what: format!(
                    "a b-map block of {} bytes is too short to hold a header",
                    bytes.len()
                ),
            });
        }
        let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let header_len = match magic {
            m if m == XFS_BMAP_CRC_MAGIC => BMBT_CRC_HEADER_LEN,
            m if m == BMBT_MAGIC => BMBT_HEADER_LEN,
            m => {
                return Err(FsError::Corrupt {
                    what: format!("a b-map block carries magic {m:#x}"),
                });
            }
        };
        let level = u16::from_be_bytes([bytes[4], bytes[5]]);
        let numrecs = u16::from_be_bytes([bytes[6], bytes[7]]) as usize;
        let fit = bytes.len().saturating_sub(header_len) / BMBT_RECORD_LEN;
        if numrecs > fit {
            return Err(FsError::Corrupt {
                what: format!("a b-map block claims {numrecs} records and holds room for {fit}"),
            });
        }
        let word = |at: usize| {
            u64::from_be_bytes([
                bytes[at],
                bytes[at + 1],
                bytes[at + 2],
                bytes[at + 3],
                bytes[at + 4],
                bytes[at + 5],
                bytes[at + 6],
                bytes[at + 7],
            ])
        };
        let mut records = Vec::with_capacity(numrecs);
        for i in 0..numrecs {
            let at = header_len + i * BMBT_RECORD_LEN;
            records.push(BmbtLeafRecord {
                l0: word(at),
                l1: word(at + 8),
            });
        }
        Ok(BmbtLeafBlock { level, records })
    }

    /// This leaf as the bytes a block of a checksummed file system holds.
    ///
    /// The header is the **long** form, 72 bytes, and it is worth being explicit
    /// that this is not the header the free space trees use: those are the short
    /// form, 56 bytes, with four-byte siblings and no block number.  Both are in
    /// the published XFS documentation (XFS Algorithms and Data Structures, and the XFS pages at docs.kernel.org) and both are in the same images here, so a node writer copied
    /// from the free space one and given a new magic would be sixteen bytes short
    /// and mis-parsed past the siblings.
    ///
    /// `blkno` is the block's **own** number, `owner` the inode whose fork this
    /// tree belongs to, and both are there so a block that has been copied or
    /// swapped cannot pass as the block it claims to be.
    ///
    /// The siblings are passed in rather than set to null because they are not
    /// always null: a tree with one leaf has none, and every leaf of a longer chain
    /// has two, and a writer that could only say "null" could not build one.  The
    /// null a single-leaf tree wants is [`BMBT_NULL_PTR`].
    ///
    /// The checksum is a CRC-32C over the whole block with its own field read as
    /// zeroes, stored least significant byte first -- the same convention the
    /// superblock, the inodes, the group header and the free list all use.
    ///
    /// `lsn` is the log sequence number, and it is a parameter rather than a zero
    /// because the leaves in these images carry `0x10000016f`: a non-zero value on
    /// a file system whose log nothing here has ever written.  So "unset is zero"
    /// is not a safe assumption for reproducing a block byte for byte, even though
    /// `xfs_repair` reads a zero as perfectly acceptable -- it is the one field of
    /// the header that this code cannot derive, and passing it in is what keeps
    /// that out of the writer.
    #[allow(dead_code)] // Used as soon as a data fork can grow into a B+tree.
    pub fn to_bytes(&self, hdr: &BtreeLblockHdr, sb_blocksize: usize) -> Vec<u8> {
        let (blkno, lsn, leftsib, rightsib) = (hdr.blkno, hdr.lsn, hdr.leftsib, hdr.rightsib);
        let (owner, uuid) = (hdr.owner, hdr.uuid);
        let max = Self::max_records(sb_blocksize, true);
        assert!(
            self.records.len() <= max,
            "b-map leaf overflow: {} records, capacity {}",
            self.records.len(),
            max
        );
        let mut b = vec![0u8; sb_blocksize];
        b[0..4].copy_from_slice(&XFS_BMAP_CRC_MAGIC.to_be_bytes());
        b[4..6].copy_from_slice(&self.level.to_be_bytes());
        b[6..8].copy_from_slice(&(self.records.len() as u16).to_be_bytes());
        b[8..16].copy_from_slice(&leftsib.to_be_bytes());
        b[16..24].copy_from_slice(&rightsib.to_be_bytes());
        b[24..32].copy_from_slice(&blkno.to_be_bytes());
        // The log sequence number.  It is **not** zero in the images here -- see
        // the note on the parameter -- and it is passed in rather than invented,
        // because this code does not go through the log and so has no sequence
        // number of its own to write.
        b[32..40].copy_from_slice(&lsn.to_be_bytes());
        b[40..56].copy_from_slice(&uuid);
        b[56..64].copy_from_slice(&owner.to_be_bytes());
        // 64..68 is the checksum and is computed last; 68..72 is padding.
        for (i, r) in self.records.iter().enumerate() {
            let at = BMBT_CRC_HEADER_LEN + i * BMBT_RECORD_LEN;
            b[at..at + BMBT_RECORD_LEN].copy_from_slice(&r.to_bytes());
        }
        let crc = crc32c_without_its_own_field(&b, 64);
        b[64..68].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// Serialize this leaf for the file system described by `has_crc`.
    pub fn to_bytes_auto(
        &self,
        hdr: &BtreeLblockHdr,
        sb_blocksize: usize,
        has_crc: bool,
    ) -> Vec<u8> {
        if has_crc {
            self.to_bytes(hdr, sb_blocksize)
        } else {
            self.to_bytes_v4(hdr.leftsib, hdr.rightsib, sb_blocksize)
        }
    }

    /// The most records a leaf can hold in one block of this shape.
    ///
    /// Asked before writing, because the alternative is writing past the end of the
    /// block: 31 records fit a 512-byte version 4 leaf and 253 fit a 4096-byte
    /// checksummed one, and a file that outgrows its leaf needs a **second leaf and
    /// an interior node**, which is a different piece of work and has to be refused
    /// rather than discovered as a slice range.
    pub const fn max_records(sb_blocksize: usize, has_crc: bool) -> usize {
        let header = if has_crc {
            BMBT_CRC_HEADER_LEN
        } else {
            BMBT_HEADER_LEN
        };
        sb_blocksize.saturating_sub(header) / BMBT_RECORD_LEN
    }

    /// The fewest records a leaf may hold **when it has a parent**.
    ///
    /// Half the maximum.  A leaf under an interior node has to be at least half
    /// full, so that splitting it in two leaves both leaves are still mostly full
    /// -- a tree that could only hold nearly-empty leaves would deepen on every
    /// insertion.
    ///
    /// This is a real rule and not a style: `xfs_repair` rejects a leaf outside it
    /// by name -- "bad # of bmap records (7, min - 15, max - 30)" -- and then
    /// calls the whole fork bad, which reads like a mapping fault and is not one.
    pub const fn min_records(sb_blocksize: usize, has_crc: bool) -> usize {
        Self::max_records(sb_blocksize, has_crc) / 2
    }

    /// How to divide `count` records among leaves of this shape.
    ///
    /// Not `chunks(max)`, which fills every leaf and leaves a remainder: the
    /// remainder becomes the last leaf, and a last leaf holding less than
    /// [`min_records`] is exactly what `xfs_repair` complains about.  So the
    /// number of leaves is fixed first and the records spread evenly over them,
    /// which keeps every leaf inside the range the format's occupancy rule
    /// describes.
    ///
    /// Returns the number of leaves, or `None` when the records cannot be divided
    /// legally.
    pub fn leaves_for(count: usize, sb_blocksize: usize, has_crc: bool) -> Option<usize> {
        if count == 0 {
            return None;
        }
        let room = Self::max_records(sb_blocksize, has_crc);
        let leaves = count.div_ceil(room);
        let per = count.div_ceil(leaves);
        if per > room || per < Self::min_records(sb_blocksize, has_crc) {
            return None;
        }
        Some(leaves)
    }

    /// Insert a record into this leaf, returning the split result if the leaf overflows.
    /// Returns None if inserted without splitting, or Some((new_leaf, separator_key)) if split.
    #[allow(dead_code)]
    pub fn insert_record(
        &mut self,
        record: BmbtLeafRecord,
        sb_blocksize: usize,
        has_crc: bool,
    ) -> Option<(BmbtLeafBlock, BmbtKey)> {
        let max = Self::max_records(sb_blocksize, has_crc);
        if self.records.len() < max {
            // Room for one more - insert in order by startoff
            let idx = self
                .records
                .partition_point(|r| r.startoff() < record.startoff());

            // Check for coalescing with adjacent records before inserting
            // Coalesce with previous record if adjacent
            if idx > 0 {
                let prev = &self.records[idx - 1];
                let prev_end = prev.startoff() + prev.blockcount();
                let prev_end_block = prev.startblock() + prev.blockcount();

                if prev_end == record.startoff()
                    && prev_end_block == record.startblock()
                    && prev.extent_flag() == record.extent_flag()
                {
                    // Coalesce: extend previous record
                    self.records[idx - 1] = BmbtLeafRecord::from_extent(&BmbtRec {
                        br_startoff: prev.startoff(),
                        br_startblock: prev.startblock(),
                        br_blockcount: prev.blockcount() + record.blockcount(),
                        br_flag: prev.extent_flag(),
                    });
                    return None;
                }
            }

            // Coalesce with next record if adjacent
            if idx < self.records.len() {
                let next = &self.records[idx];
                let record_end = record.startoff() + record.blockcount();
                let record_end_block = record.startblock() + record.blockcount();

                if record_end == next.startoff()
                    && record_end_block == next.startblock()
                    && record.extent_flag() == next.extent_flag()
                {
                    // Coalesce: extend new record to include next
                    let new_record = BmbtLeafRecord::from_extent(&BmbtRec {
                        br_startoff: record.startoff(),
                        br_startblock: record.startblock(),
                        br_blockcount: record.blockcount() + next.blockcount(),
                        br_flag: record.extent_flag(),
                    });
                    self.records.remove(idx);
                    self.records.insert(idx, new_record);
                    return None;
                }
            }

            // No coalescing possible, insert normally
            self.records.insert(idx, record);
            return None;
        }

        // Leaf is full - need to split
        let mut all_records = self.records.clone();
        let idx = all_records.partition_point(|r| r.startoff() < record.startoff());
        all_records.insert(idx, record);

        // Split in half - first half stays, second half goes to new leaf
        let mid = all_records.len() / 2;
        let new_leaf_records = all_records.split_off(mid);

        // The separator key is the first record of the new leaf
        let separator_key = BmbtKey {
            br_startoff: new_leaf_records[0].startoff(),
        };

        let new_leaf = BmbtLeafBlock {
            level: self.level,
            records: new_leaf_records,
        };

        self.records = all_records;

        Some((new_leaf, separator_key))
    }

    /// The separator key for this leaf (first record's startoff).
    #[allow(dead_code)]
    pub fn separator_key(&self) -> BmbtKey {
        BmbtKey {
            br_startoff: self.records[0].startoff(),
        }
    }

    /// Remove a record from this leaf by startoff, returning true if removed.
    /// If the leaf falls below minimum occupancy, returns Ok(true) to indicate
    /// that merge/redistribution is needed.
    #[allow(dead_code)]
    pub fn remove_record(
        &mut self,
        startoff: XfsFileoff,
        sb_blocksize: usize,
        has_crc: bool,
    ) -> Result<bool, i32> {
        let min = Self::min_records(sb_blocksize, has_crc);
        let idx = self.records.partition_point(|r| r.startoff() < startoff);
        if idx >= self.records.len() || self.records[idx].startoff() != startoff {
            return Err(libc::ENOENT);
        }

        // Check if adjacent records can be coalesced after removal
        // If we have a record before and after the removed one, check if they're now adjacent
        if idx > 0 && idx < self.records.len() - 1 {
            let prev = &self.records[idx - 1];
            let next = &self.records[idx + 1];
            let prev_end = prev.startoff() + prev.blockcount();
            let prev_end_block = prev.startblock() + prev.blockcount();

            if prev_end == next.startoff()
                && prev_end_block == next.startblock()
                && prev.extent_flag() == next.extent_flag()
            {
                // Coalesce prev and next
                let merged = BmbtLeafRecord::from_extent(&BmbtRec {
                    br_startoff: prev.startoff(),
                    br_startblock: prev.startblock(),
                    br_blockcount: prev.blockcount() + next.blockcount(),
                    br_flag: prev.extent_flag(),
                });
                self.records.remove(idx); // Remove the target record
                self.records[idx - 1] = merged; // Replace prev with merged
                self.records.remove(idx); // Remove next (now at idx after first removal)
                return Ok(self.records.len() < min);
            }
        }

        self.records.remove(idx);
        // Return true if we need to merge/redistribute
        Ok(self.records.len() < min)
    }

    /// Merge this leaf with another leaf (right sibling), returning the combined records.
    /// Used when a leaf is below minimum and can be merged with a sibling.
    #[allow(dead_code)]
    pub fn merge_with(&mut self, other: BmbtLeafBlock) {
        self.records.extend(other.records);
    }

    /// Redistribute records with a sibling leaf to maintain minimum occupancy.
    /// Returns true if redistribution was successful.
    #[allow(dead_code)]
    pub fn redistribute_with(&mut self, other: &mut BmbtLeafBlock) -> bool {
        let total = self.records.len() + other.records.len();
        if total < 2 * Self::min_records(0, false) {
            // Can't redistribute while maintaining minimum
            return false;
        }
        // Combine and split evenly
        let mut all = self.records.clone();
        all.append(&mut other.records);
        all.sort_by_key(|r| r.startoff());
        let mid = all.len() / 2;
        let new_other = all.split_off(mid);
        self.records = all;
        other.records = new_other;
        true
    }

    /// Coalesce adjacent extents in this leaf.
    /// Merges records that are contiguous in both file offset and filesystem block.
    #[allow(dead_code)]
    pub fn coalesce_adjacent(&mut self) {
        if self.records.len() < 2 {
            return;
        }

        let mut i = 0;
        while i + 1 < self.records.len() {
            let current = &self.records[i];
            let next = &self.records[i + 1];

            let current_end = current.startoff() + current.blockcount();
            let current_end_block = current.startblock() + current.blockcount();

            if current_end == next.startoff()
                && current_end_block == next.startblock()
                && current.extent_flag() == next.extent_flag()
            {
                // Coalesce current and next
                let merged = BmbtLeafRecord::from_extent(&BmbtRec {
                    br_startoff: current.startoff(),
                    br_startblock: current.startblock(),
                    br_blockcount: current.blockcount() + next.blockcount(),
                    br_flag: current.extent_flag(),
                });
                self.records[i] = merged;
                self.records.remove(i + 1);
                // Don't increment i - check if we can merge further
            } else {
                i += 1;
            }
        }
    }

    /// This leaf as the bytes of a block of a file system **without** checksums holds.
    /// This leaf as the bytes a block of a file system **without** checksums holds.
    ///
    /// A version 4 file system has no checksum, no UUID and no log, so its b-tree
    /// blocks carry neither: the header is the magic, the level, the count and the
    /// two siblings, twenty-four bytes, and the records start there.  That is
    /// `XFS_BTREE_LBLOCK_LEN`, and it is the same long form as the checksummed one
    /// with everything after the siblings left out.
    ///
    /// **The magic is not the checksummed one**, which is the mistake this exists to
    /// prevent: a version 5 leaf written into a version 4 file system is rejected by
    /// `xfs_repair` as `bad magic # 0x424d4133`, and the tree it belongs to is then
    /// "bad data fork in inode ...", which reads like a mapping fault and is not
    /// one.
    pub fn to_bytes_v4(
        &self,
        leftsib: XfsFsblock,
        rightsib: XfsFsblock,
        sb_blocksize: usize,
    ) -> Vec<u8> {
        let max = Self::max_records(sb_blocksize, false);
        assert!(
            self.records.len() <= max,
            "b-map leaf overflow: {} records, capacity {}",
            self.records.len(),
            max
        );
        let mut b = vec![0u8; sb_blocksize];
        b[0..4].copy_from_slice(&XFS_BMAP_MAGIC.to_be_bytes());
        b[4..6].copy_from_slice(&self.level.to_be_bytes());
        b[6..8].copy_from_slice(&(self.records.len() as u16).to_be_bytes());
        b[8..16].copy_from_slice(&leftsib.to_be_bytes());
        b[16..24].copy_from_slice(&rightsib.to_be_bytes());
        for (i, r) in self.records.iter().enumerate() {
            let at = BMBT_HEADER_LEN + i * BMBT_RECORD_LEN;
            b[at..at + BMBT_RECORD_LEN].copy_from_slice(&r.to_bytes());
        }
        b
    }

    /// The extents as the rest of this program represents them.
    ///
    /// Each record carries its own block count, so nothing here is assumed about
    /// how long an extent is.
    pub fn extents(&self) -> Bmx {
        let recs: Vec<BmbtRec> = self.records.iter().map(BmbtLeafRecord::as_extent).collect();
        Bmx::new(&recs)
    }
}

#[derive(Debug, Clone, Decode, Default)]
pub struct BmdrBlock {
    pub bb_level: u16,
    pub bb_numrecs: u16,
}

impl BmdrBlock {
    pub const SIZE: usize = 4;
}

#[derive(Debug, Clone, Decode)]
pub struct BmbtKey {
    pub br_startoff: XfsFileoff,
}

impl BmbtKey {
    pub const SIZE: usize = 8;
}

pub type XfsBmbtPtr = XfsFsblock;
pub type XfsBmdrPtr = XfsFsblock;
pub type XfsBmbtLblock = BtreeBlockHdr<u64>;

trait BtreePriv {
    fn keys(&self) -> &[BmbtKey];
    fn level(&self) -> u16;
    fn block_cache(&self) -> &RefCell<BtreeBlockCache>;
    fn ptrs(&self) -> &[XfsBmbtPtr];
}

/// Methods that are common to both BtreeRoot and BtreeIntermediate
#[allow(private_bounds)]
pub trait Btree: BtreePriv {
    /// Return the extent, if any, that contains the given block within the file.
    /// Return its starting position as an FSblock, and its length in file system block units.
    /// If a hole's length extents to EoF, return None for length.
    /// If `filter_unwritten` is true, unwritten extents (br_flag=true) are treated as holes.
    fn map_block<R: bincode_next::de::read::Reader + BufRead + Seek>(
        &self,
        buf_reader: &mut R,
        logical_block: XfsFileoff,
        filter_unwritten: bool,
    ) -> Result<(Option<XfsFsblock>, Option<u64>), i32> {
        let super_block = super::volume::try_superblock().ok_or(libc::ENODEV)?;
        let pp = self
            .keys()
            .partition_point(|k| k.br_startoff <= logical_block);
        // If there's a hole at the start, we should still descend into the leftmost child.
        // BtreeLeaf::get_extent will calculate the hole's size.
        let idx = pp.saturating_sub(1);

        let mut guard = self.block_cache().borrow_mut();
        match &mut *guard {
            BtreeBlockCache::Intermediate(bci) => {
                if self.level() <= 1 {
                    return Err(crate::libxfuse::EUCLEAN);
                }

                let entry = bci.entry(idx);
                match entry {
                    Entry::Vacant(ve) => {
                        let offset = super_block.fsb_to_offset(self.ptrs()[idx]);
                        buf_reader
                            .seek(SeekFrom::Start(offset))
                            .map_err(|e| e.raw_os_error().unwrap())?;
                        let bti: BtreeIntermediate =
                            decode_from(buf_reader.by_ref()).map_err(|_| libc::EDESTADDRREQ)?;
                        ve.insert(bti)
                            .map_block(buf_reader, logical_block, filter_unwritten)
                    }
                    Entry::Occupied(oe) => {
                        let v: &BtreeIntermediate = oe.get();
                        v.map_block(buf_reader, logical_block, filter_unwritten)
                    }
                }
            }
            BtreeBlockCache::Leaf(bcl) => {
                if self.level() > 1 {
                    return Err(crate::libxfuse::EUCLEAN);
                }

                let entry = bcl.entry(idx);
                match entry {
                    Entry::Vacant(ve) => {
                        let offset = super_block.fsb_to_offset(self.ptrs()[idx]);
                        let mut bytes = vec![0u8; super_block.sb_blocksize as usize];
                        buf_reader
                            .seek(SeekFrom::Start(offset))
                            .map_err(|e| e.raw_os_error().unwrap())?;
                        buf_reader
                            .read_exact(&mut bytes)
                            .map_err(|e| e.raw_os_error().unwrap())?;
                        let btl =
                            BtreeLeaf::from_bytes(&bytes).map_err(|_| crate::libxfuse::EUCLEAN)?;
                        Ok(ve.insert(btl).get_extent(logical_block, filter_unwritten))
                    }
                    Entry::Occupied(oe) => {
                        let v: &BtreeLeaf = oe.get();
                        Ok(v.get_extent(logical_block, filter_unwritten))
                    }
                }
            }
        }
    }
}

impl BtreeRoot {
    /// Every extent in the tree, and every block the tree occupies.
    ///
    /// The two go together because they are what an operation that *removes* a
    /// tree needs: the extents to decide what the file still owns, and the node
    /// blocks to give back, because a node that is still charged for is a block
    /// nobody can allocate.
    ///
    /// `map_block` answers "which extent holds *this* block", which is the question
    /// a read asks and the only one this code has asked of a b-tree.  An operation
    /// that changes the tree asks a different one -- "what is in it" -- and there
    /// was no way to get that answer, so every such operation refused a file whose
    /// extents are not in its inode.  Truncating one is refused for exactly that
    /// reason.
    ///
    /// This is what it needs, and what a writer needs before it can rewrite one.
    ///
    /// The walk descends by level rather than by searching, so a child's level
    /// comes from its parent and its own header never has to be parsed to decide
    /// what it is.  Records come out in key order, which for a b-map b-tree is
    /// file-offset order, and that is the order the rest of the program expects an
    /// extent list in.
    ///
    /// **A root that is itself a leaf is refused.**  Such a root keeps its records
    /// in the inode rather than in a block, and this holds only the `xfs_bmdr_block`
    /// and the keys and pointers decoded beside it, so its records are not in hand
    /// here.  No data fork in any image in this repository has one -- every one
    /// measured has an interior root with leaves in blocks -- and returning half a
    /// tree for the rest would describe a file that is not the file, so this is an
    /// error and says so.
    ///
    /// A block that cannot be read, or that is not the level its parent promised,
    /// is likewise an error.  Silently returning the extents of the leaves that
    /// happened to be readable would be a short list, and a short list is a file
    /// that is quietly shorter than it is.
    #[allow(dead_code)] // Used as soon as a b-tree data fork can be truncated.
    pub fn all_extents<R>(&self, buf_reader: &mut R) -> Result<(Bmx, Vec<XfsFsblock>), i32>
    where
        R: bincode_next::de::read::Reader + BufRead + Seek,
    {
        let sb = super::volume::try_superblock().ok_or(libc::ENODEV)?;
        let mut out: Vec<BmbtRec> = Vec::new();
        // Every node below the root.  The root itself is in the inode and so is
        // not here: it costs the file nothing beyond its own inode bytes.
        let mut nodes: Vec<XfsFsblock> = Vec::new();
        let mut stack: Vec<(Vec<XfsBmbtPtr>, u16)> = vec![(self.ptrs.clone(), self.level())];

        while let Some((ptrs, level)) = stack.pop() {
            for ptr in ptrs {
                let offset = sb.fsb_to_offset(ptr);
                let child = level - 1;
                buf_reader
                    .seek(SeekFrom::Start(offset))
                    .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
                nodes.push(ptr);
                if child == 0 {
                    let mut bytes = vec![0u8; sb.sb_blocksize as usize];
                    buf_reader
                        .read_exact(&mut bytes)
                        .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
                    let leaf =
                        BmbtLeafBlock::from_bytes(&bytes).map_err(|_| crate::libxfuse::EUCLEAN)?;
                    if leaf.level != child {
                        return Err(crate::libxfuse::EUCLEAN);
                    }
                    out.extend(leaf.extents().extents().iter().copied());
                } else {
                    let node: BtreeIntermediate =
                        decode_from(buf_reader.by_ref()).map_err(|_| libc::EDESTADDRREQ)?;
                    if node.level() != child {
                        return Err(crate::libxfuse::EUCLEAN);
                    }
                    stack.push((node.ptrs().to_vec(), node.level()));
                }
            }
        }
        Ok((Bmx::new(&out), nodes))
    }

    /// Like [`all_extents`](Self::all_extents), but reads blocks through the
    /// transaction cache so it sees uncommitted writes.
    pub fn all_extents_tx(&self, tx: &mut Transaction<'_>) -> Result<(Bmx, Vec<XfsFsblock>), i32> {
        let _sb = super::volume::try_superblock().ok_or(libc::ENODEV)?;
        let mut out: Vec<BmbtRec> = Vec::new();
        let mut nodes: Vec<XfsFsblock> = Vec::new();
        let mut stack: Vec<(Vec<XfsBmbtPtr>, u16)> = vec![(self.ptrs.clone(), self.level())];

        while let Some((ptrs, level)) = stack.pop() {
            for ptr in ptrs {
                let child = level - 1;
                nodes.push(ptr);
                if child == 0 {
                    let bytes = tx.read_block(ptr).map_err(|e| e.errno())?;
                    let leaf =
                        BmbtLeafBlock::from_bytes(&bytes).map_err(|_| crate::libxfuse::EUCLEAN)?;
                    if leaf.level != child {
                        return Err(crate::libxfuse::EUCLEAN);
                    }
                    out.extend(leaf.extents().extents().iter().copied());
                } else {
                    let bytes = tx.read_block(ptr).map_err(|e| e.errno())?;
                    let node: BtreeIntermediate = decode(&bytes).map_err(|_| libc::EDESTADDRREQ)?.0;
                    if node.level() != child {
                        return Err(crate::libxfuse::EUCLEAN);
                    }
                    stack.push((node.ptrs().to_vec(), node.level()));
                }
            }
        }
        Ok((Bmx::new(&out), nodes))
    }
}

/// Result of inserting into a child node: either no split, or a new node with separator key.
#[allow(dead_code)]
#[derive(Debug)]
enum InsertResult {
    NoSplit,
    SplitLeaf(BmbtLeafBlock, BmbtKey, XfsFsblock),
    SplitIntermediate(BmbtInteriorBlock, BmbtKey, XfsFsblock),
}

impl BtreeRoot {
    /// Insert an extent record into the BMBT, growing the tree as necessary.
    /// This is used when extending a file's data fork.
    #[allow(dead_code)]
    pub fn insert_extent<R: DeviceReader>(
        &mut self,
        buf_reader: &mut R,
        extent: BmbtRec,
        sb: &Sb,
        tx: &mut Transaction<'_>,
    ) -> Result<(), i32> {
        let record = BmbtLeafRecord::from_extent(&extent);

        // Handle root level 0 (root is a leaf in the inode)
        if self.bmdr.bb_level == 0 {
            return self.insert_extent_root_leaf(buf_reader, record, sb, tx);
        }

        // For multi-level tree, descend and insert
        self.insert_extent_recursive(buf_reader, record, sb, tx)
    }

    /// Insert when root is a leaf (level 0). Convert to a tree with level 1 root.
    #[allow(dead_code)]
    fn insert_extent_root_leaf<R: DeviceReader>(
        &mut self,
        buf_reader: &mut R,
        record: BmbtLeafRecord,
        sb: &Sb,
        tx: &mut Transaction<'_>,
    ) -> Result<(), i32> {
        // The root is currently a leaf (records in the inode).
        // This case requires converting the inode from extent list format to BMBT format.
        // We need to:
        // 1. Read current extents from the inode
        // 2. Allocate a new leaf block
        // 3. Write extents to the leaf block
        // 4. Update the inode's root to point to the new leaf (level 1)

        // Get current extents from the inode
        let device = buf_reader.device();
        let extents = self.all_extents_from_inode(device, sb)?;
        if extents.is_empty() {
            // No extents yet - just insert the new record
            return self.insert_extent_root_leaf_empty(buf_reader, record, sb, tx);
        }

        // Insert the new record into the extent list first
        let mut all_extents = extents;
        let new_extent = BmbtLeafRecord::as_extent(&record);
        // Find insertion point
        let insert_idx = all_extents.partition_point(|e| e.br_startoff < new_extent.br_startoff);
        all_extents.insert(insert_idx, new_extent);

        // Check if all extents fit in inode
        let max_inode_extents = self.max_inode_extents(sb);
        if all_extents.len() <= max_inode_extents {
            // Still fits in inode - write back to inode
            return self.write_extents_to_inode(&all_extents, sb, tx);
        }

        // Need to convert to BMBT: allocate leaf block, write extents, update root
        self.convert_to_btree(buf_reader, &all_extents, sb, tx)
    }

    /// Insert when root is empty (level 0, no extents)
    #[allow(dead_code)]
    fn insert_extent_root_leaf_empty<R>(
        &mut self,
        _buf_reader: &mut R,
        record: BmbtLeafRecord,
        sb: &Sb,
        tx: &mut Transaction<'_>,
    ) -> Result<(), i32>
    where
        R: bincode_next::de::read::Reader + BufRead + Seek,
    {
        // Just write the single extent to the inode
        let extents = vec![record.as_extent()];
        self.write_extents_to_inode(&extents, sb, tx)
    }

    /// Maximum number of extents that fit in an inode's data fork
    fn max_inode_extents(&self, sb: &Sb) -> usize {
        let fork_size = self.inode_fork_size(sb);
        fork_size / BMBT_RECORD_LEN
    }

    /// Size of the inode's data fork in bytes
    fn inode_fork_size(&self, sb: &Sb) -> usize {
        // The data fork starts after the inode core and attribute fork
        // For simplicity, use a reasonable estimate based on inode size
        let inode_size = sb.inode_size();
        let core_size = 176; // size of di_core
        let attr_fork_size = if self.bmdr.bb_level == 0 && self.bmdr.bb_numrecs == 0 {
            0
        } else {
            (self.bmdr.bb_numrecs as usize) * BMBT_RECORD_LEN
        };
        inode_size.saturating_sub(core_size + attr_fork_size)
    }

    /// Write extents directly to inode (extent list format)
    fn write_extents_to_inode(
        &mut self,
        extents: &[BmbtRec],
        sb: &Sb,
        tx: &mut Transaction<'_>,
    ) -> Result<(), i32> {
        // Get the inode number from the bmdr owner
        let ino = self.owner.ok_or(libc::ENOTSUP)?;
        let inode_offset = sb.inode_offset(ino);
        let inode_size = sb.inode_size();

        // Read the inode
        let inode_bytes = tx
            .read_bytes(inode_offset, inode_size)
            .map_err(|e| e.errno())?;

        // Parse the inode
        let mut raw_inode =
            RawDinode::from_bytes(inode_bytes).map_err(|_| crate::libxfuse::EUCLEAN)?;

        // Set format to 2 (extent list)
        raw_inode.set_format(2);

        // Write bmdr header (bb_level=0, bb_numrecs=extents.len())
        let start = raw_inode.literal_area_offset();
        let is_nrext64 = raw_inode.nrext64();
        let bytes = raw_inode.as_mut_bytes();
        bytes[start..start + 2].copy_from_slice(&0u16.to_be_bytes()); // bb_level = 0
        bytes[start + 2..start + 4].copy_from_slice(&(extents.len() as u16).to_be_bytes()); // bb_numrecs

        // Write extents
        let extent_start = start + BmdrBlock::SIZE;
        for (i, extent) in extents.iter().enumerate() {
            let at = extent_start + i * EXTENT_REC_SIZE;
            encode_extent(&mut bytes[at..at + EXTENT_REC_SIZE], extent);
        }

        // Update nextents count
        let count = extents.len() as u64;
        if is_nrext64 {
            BigEndian::write_u64(&mut bytes[offset::NEXTENTS..], count);
        } else {
            BigEndian::write_u32(&mut bytes[offset::NEXTENTS32..], count as u32);
        }

        // Finalize the inode (update CRC, etc.)
        raw_inode.finalise();

        // Write the inode back
        tx.write_bytes(inode_offset, raw_inode.as_bytes())
            .map_err(|e| e.errno())?;

        Ok(())
    }

    /// Convert from extent list format to BMBT format
    fn convert_to_btree<R>(
        &mut self,
        _buf_reader: &mut R,
        extents: &[BmbtRec],
        sb: &Sb,
        tx: &mut Transaction<'_>,
    ) -> Result<(), i32>
    where
        R: bincode_next::de::read::Reader + BufRead + Seek,
    {
        // Allocate a new leaf block
        let agno = 0; // TODO: choose AG intelligently
        let run_opt = allocate_in_group(tx, sb, agno, 1).map_err(|e| e.errno())?;
        let run = run_opt.ok_or(libc::ENOSPC)?;
        let leaf_block = (agno as u64 * sb.sb_agblocks as u64) + run.start as u64;

        // Create leaf block with all extents
        let leaf = BmbtLeafBlock {
            level: 0,
            records: extents.iter().map(BmbtLeafRecord::from_extent).collect(),
        };

        // Write leaf block
        let leaf_hdr = BtreeLblockHdr {
            blkno: leaf_block,
            lsn: 0,
            leftsib: BMBT_NULL_PTR,
            rightsib: BMBT_NULL_PTR,
            owner: 0,
            uuid: sb.sb_uuid.as_image_bytes(),
        };
        let leaf_bytes = leaf.to_bytes_auto(&leaf_hdr, sb.sb_blocksize as usize, sb.has_crc());
        tx.write_bytes(sb.fsb_to_offset(leaf_block), &leaf_bytes)
            .map_err(|e| e.errno())?;

        // Update root to point to leaf (level 1)
        self.bmdr.bb_level = 1;
        self.bmdr.bb_numrecs = 1;
        self.keys = vec![BmbtKey {
            br_startoff: leaf.records[0].startoff(),
        }];
        self.ptrs = vec![leaf_block];

        // Note: The inode itself needs to be updated with the new root
        // This requires coordination with the inode write path
        Ok(())
    }

    /// Get all extents from the inode's root (for level 0)
    fn all_extents_from_inode(
        &self,
        device: Arc<BlockDevice>,
        sb: &Sb,
    ) -> Result<Vec<BmbtRec>, i32> {
        // For level 0 root, the extents are stored in the inode's literal area
        // We need to read the inode and extract the extents
        let ino = self.owner.ok_or(libc::ENOTSUP)?;
        let inode_offset = sb.inode_offset(ino);
        let inode_size = sb.inode_size();

        // Read the inode using the provided device
        let mut inode_bytes = vec![0u8; inode_size];
        device
            .read_at(&mut inode_bytes, inode_offset)
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

        // Parse the inode
        let raw_inode = RawDinode::from_bytes(inode_bytes).map_err(|_| crate::libxfuse::EUCLEAN)?;

        // Get the extents from the inode
        raw_inode.core_extents().ok_or(libc::ENOTSUP)
    }

    /// Recursive insertion for multi-level trees (level >= 1)
    #[allow(dead_code)]
    fn insert_extent_recursive<R: DeviceReader>(
        &mut self,
        buf_reader: &mut R,
        record: BmbtLeafRecord,
        sb: &Sb,
        tx: &mut Transaction<'_>,
    ) -> Result<(), i32> {
        let record_startoff = record.startoff();

        // Find the child index
        let child_idx = self
            .keys
            .partition_point(|k| k.br_startoff < record_startoff);

        // Read the child block
        let child_ptr = self.ptrs[child_idx];
        let offset = sb.fsb_to_offset(child_ptr);

        buf_reader
            .seek(SeekFrom::Start(offset))
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

        let mut child_bytes = vec![0u8; sb.sb_blocksize as usize];
        buf_reader
            .read_exact(&mut child_bytes)
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

        // Determine if child is leaf or intermediate
        let child_level = self.bmdr.bb_level - 1;

        let result = if child_level == 0 {
            // Child is a leaf
            let mut leaf =
                BmbtLeafBlock::from_bytes(&child_bytes).map_err(|_| crate::libxfuse::EUCLEAN)?;
            match leaf.insert_record(record, sb.sb_blocksize as usize, sb.has_crc()) {
                None => InsertResult::NoSplit,
                Some((new_leaf, separator_key)) => {
                    // Write the modified leaf back
                    let hdr = BtreeLblockHdr {
                        blkno: child_ptr,
                        lsn: 0,
                        leftsib: BMBT_NULL_PTR,
                        rightsib: BMBT_NULL_PTR,
                        owner: 0,
                        uuid: sb.sb_uuid.as_image_bytes(),
                    };
                    let leaf_bytes =
                        leaf.to_bytes_auto(&hdr, sb.sb_blocksize as usize, sb.has_crc());
                    tx.write_bytes(offset, &leaf_bytes).map_err(|e| e.errno())?;

                    // Allocate a new block for the split leaf
                    let agno = (child_ptr >> sb.sb_agblklog) as u32;
                    let run_opt = allocate_in_group(tx, sb, agno, 1).map_err(|e| e.errno())?;
                    let run = run_opt.ok_or(libc::ENOSPC)?;
                    let new_block = run.start as XfsFsblock;

                    // Write the new leaf
                    let new_hdr = BtreeLblockHdr {
                        blkno: new_block,
                        lsn: 0,
                        leftsib: BMBT_NULL_PTR,
                        rightsib: BMBT_NULL_PTR,
                        owner: 0,
                        uuid: sb.sb_uuid.as_image_bytes(),
                    };
                    let new_leaf_bytes =
                        new_leaf.to_bytes_auto(&new_hdr, sb.sb_blocksize as usize, sb.has_crc());
                    tx.write_bytes(sb.fsb_to_offset(new_block), &new_leaf_bytes)
                        .map_err(|e| e.errno())?;

                    // Update sibling pointers if needed (simplified for now)

                    InsertResult::SplitLeaf(new_leaf, separator_key, new_block)
                }
            }
        } else {
            // Child is an intermediate node
            let mut intermediate: BtreeIntermediate =
                decode_from(buf_reader.by_ref()).map_err(|_| libc::EDESTADDRREQ)?;
            // Recursively insert into intermediate
            match self.insert_extent_intermediate(&mut intermediate, buf_reader, record, sb, tx)? {
                None => InsertResult::NoSplit,
                Some((new_node, separator_key, new_block)) => {
                    InsertResult::SplitIntermediate(new_node, separator_key, new_block)
                }
            }
        };

        // Handle split result
        match result {
            InsertResult::NoSplit => Ok(()),
            InsertResult::SplitLeaf(_, separator_key, new_block) => {
                // Insert the separator key and new block pointer into this node
                self.insert_key_ptr_at(child_idx + 1, separator_key, new_block);
                // Check if root (level 1) needs to split
                if self.bmdr.bb_level == 1 {
                    self.handle_root_split(sb, tx)?;
                }
                Ok(())
            }
            InsertResult::SplitIntermediate(_, separator_key, new_block) => {
                self.insert_key_ptr_at(child_idx + 1, separator_key, new_block);
                // Check if root (level 1) needs to split
                if self.bmdr.bb_level == 1 {
                    self.handle_root_split(sb, tx)?;
                }
                Ok(())
            }
        }
    }

    #[allow(dead_code)]
    fn insert_key_ptr_at(&mut self, idx: usize, key: BmbtKey, ptr: XfsBmbtPtr) {
        if idx <= self.keys.len() {
            self.keys.insert(idx, key);
            self.ptrs.insert(idx, ptr);
            self.bmdr.bb_numrecs = self.keys.len() as u16;
        }
    }

    /// Handle root split when level 1 root overflows (root growth 1→2).
    /// Allocates a new root block, moves current root to a child, creates new root.
    #[allow(dead_code)]
    fn handle_root_split(&mut self, sb: &Sb, tx: &mut Transaction<'_>) -> Result<(), i32> {
        // Check if root (level 1) is full
        let max = BmbtInteriorBlock::max_children(sb.sb_blocksize as usize, sb.has_crc());
        if self.keys.len() <= max {
            return Ok(());
        }

        // Root is full - need to split and grow to level 2
        // 1. Allocate new block for the old root (now a child)
        let agno = 0; // TODO: choose AG intelligently
        let run_opt = allocate_in_group(tx, sb, agno, 1).map_err(|e| e.errno())?;
        let run = run_opt.ok_or(libc::ENOSPC)?;
        let old_root_block = (agno as u64 * sb.sb_agblocks as u64) + run.start as u64;

        // 2. Create intermediate node from current root (now level 1)
        let old_root_intermediate = BmbtInteriorBlock {
            level: 1,
            keys: self.keys.clone(),
            ptrs: self.ptrs.clone(),
        };

        // 3. Write old root to new block
        let old_root_hdr = BtreeLblockHdr {
            blkno: old_root_block,
            lsn: 0,
            leftsib: BMBT_NULL_PTR,
            rightsib: BMBT_NULL_PTR,
            owner: 0,
            uuid: sb.sb_uuid.as_image_bytes(),
        };
        let old_root_bytes = old_root_intermediate.to_bytes_auto(
            &old_root_hdr,
            sb.sb_blocksize as usize,
            sb.has_crc(),
        );
        tx.write_bytes(sb.fsb_to_offset(old_root_block), &old_root_bytes)
            .map_err(|e| e.errno())?;

        // 4. Split the keys/ptrs for new root (level 2)
        let mid = self.keys.len() / 2;
        let new_separator = self.keys[mid].clone();

        let left_keys = self.keys[..mid].to_vec();
        let left_ptrs = self.ptrs[..mid].to_vec();
        let right_keys = self.keys[mid..].to_vec();
        let right_ptrs = self.ptrs[mid..].to_vec();

        // 5. Allocate block for new right sibling
        let run_opt2 = allocate_in_group(tx, sb, agno, 1).map_err(|e| e.errno())?;
        let run2 = run_opt2.ok_or(libc::ENOSPC)?;
        let right_block = (agno as u64 * sb.sb_agblocks as u64) + run2.start as u64;

        // 6. Create right sibling intermediate node
        let right_sibling = BmbtInteriorBlock {
            level: 1,
            keys: right_keys,
            ptrs: right_ptrs,
        };

        let right_hdr = BtreeLblockHdr {
            blkno: right_block,
            lsn: 0,
            leftsib: BMBT_NULL_PTR,
            rightsib: BMBT_NULL_PTR,
            owner: 0,
            uuid: sb.sb_uuid.as_image_bytes(),
        };
        let right_bytes =
            right_sibling.to_bytes_auto(&right_hdr, sb.sb_blocksize as usize, sb.has_crc());
        tx.write_bytes(sb.fsb_to_offset(right_block), &right_bytes)
            .map_err(|e| e.errno())?;

        // 7. Update root to level 2 with left keys and pointers to old root and right sibling
        self.bmdr.bb_level = 2;
        self.keys = left_keys;
        self.ptrs = left_ptrs;
        self.keys.push(new_separator);
        self.ptrs.push(right_block);
        self.bmdr.bb_numrecs = self.keys.len() as u16;

        // 8. Update left sibling (old root) rightsib pointer
        // Note: This would require updating the old root block's rightsib
        // For now, we'll skip this as it's a sibling pointer optimization

        Ok(())
    }

    #[allow(dead_code)]
    fn insert_extent_intermediate<R: DeviceReader>(
        &self,
        intermediate: &mut BtreeIntermediate,
        buf_reader: &mut R,
        record: BmbtLeafRecord,
        sb: &Sb,
        tx: &mut Transaction<'_>,
    ) -> Result<Option<(BmbtInteriorBlock, BmbtKey, XfsFsblock)>, i32> {
        let record_startoff = record.startoff();

        // Find the child index
        let child_idx = intermediate
            .keys
            .partition_point(|k| k.br_startoff < record_startoff);

        // Read the child block
        let child_ptr = intermediate.ptrs[child_idx];
        let offset = sb.fsb_to_offset(child_ptr);

        buf_reader
            .seek(SeekFrom::Start(offset))
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

        let mut child_bytes = vec![0u8; sb.sb_blocksize as usize];
        buf_reader
            .read_exact(&mut child_bytes)
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

        // Determine if child is leaf or intermediate
        let child_level = intermediate.level() - 1;

        let result = if child_level == 0 {
            // Child is a leaf
            let mut leaf =
                BmbtLeafBlock::from_bytes(&child_bytes).map_err(|_| crate::libxfuse::EUCLEAN)?;
            match leaf.insert_record(record, sb.sb_blocksize as usize, sb.has_crc()) {
                None => InsertResult::NoSplit,
                Some((new_leaf, separator_key)) => {
                    // Write the modified leaf back
                    let hdr = BtreeLblockHdr {
                        blkno: child_ptr,
                        lsn: 0,
                        leftsib: BMBT_NULL_PTR,
                        rightsib: BMBT_NULL_PTR,
                        owner: 0,
                        uuid: sb.sb_uuid.as_image_bytes(),
                    };
                    let leaf_bytes =
                        leaf.to_bytes_auto(&hdr, sb.sb_blocksize as usize, sb.has_crc());
                    tx.write_bytes(offset, &leaf_bytes).map_err(|e| e.errno())?;

                    // Allocate a new block for the split leaf
                    let agno = (child_ptr >> sb.sb_agblklog) as u32;
                    let run_opt = allocate_in_group(tx, sb, agno, 1).map_err(|e| e.errno())?;
                    let run = run_opt.ok_or(libc::ENOSPC)?;
                    let new_block = run.start as XfsFsblock;

                    // Write the new leaf
                    let new_hdr = BtreeLblockHdr {
                        blkno: new_block,
                        lsn: 0,
                        leftsib: BMBT_NULL_PTR,
                        rightsib: BMBT_NULL_PTR,
                        owner: 0,
                        uuid: sb.sb_uuid.as_image_bytes(),
                    };
                    let new_leaf_bytes =
                        new_leaf.to_bytes_auto(&new_hdr, sb.sb_blocksize as usize, sb.has_crc());
                    tx.write_bytes(sb.fsb_to_offset(new_block), &new_leaf_bytes)
                        .map_err(|e| e.errno())?;

                    InsertResult::SplitLeaf(new_leaf, separator_key, new_block)
                }
            }
        } else {
            // Child is an intermediate node - recurse deeper
            let mut child_intermediate: BtreeIntermediate =
                decode_from(buf_reader.by_ref()).map_err(|_| libc::EDESTADDRREQ)?;
            match self.insert_extent_intermediate(
                &mut child_intermediate,
                buf_reader,
                record,
                sb,
                tx,
            )? {
                None => InsertResult::NoSplit,
                Some((new_node, separator_key, new_block)) => {
                    InsertResult::SplitIntermediate(new_node, separator_key, new_block)
                }
            }
        };

        // Handle split result
        match result {
            InsertResult::NoSplit => Ok(None),
            InsertResult::SplitLeaf(_, separator_key, new_block) => {
                // Insert the separator key and new block pointer into this intermediate node
                let max = BmbtInteriorBlock::max_children(sb.sb_blocksize as usize, sb.has_crc());
                if intermediate.keys.len() < max {
                    let idx = child_idx + 1;
                    intermediate.keys.insert(idx, separator_key);
                    intermediate.ptrs.insert(idx, new_block);
                    // Write the intermediate node back
                    let hdr = BtreeLblockHdr {
                        blkno: offset,
                        lsn: 0,
                        leftsib: BMBT_NULL_PTR,
                        rightsib: BMBT_NULL_PTR,
                        owner: 0,
                        uuid: sb.sb_uuid.as_image_bytes(),
                    };
                    let node_bytes =
                        intermediate.to_bytes_auto(&hdr, sb.sb_blocksize as usize, sb.has_crc());
                    tx.write_bytes(offset, &node_bytes).map_err(|e| e.errno())?;
                    Ok(None)
                } else {
                    // Intermediate node also needs to split
                    let mut all_keys = intermediate.keys.clone();
                    let mut all_ptrs = intermediate.ptrs.clone();
                    let idx = child_idx + 1;
                    all_keys.insert(idx, separator_key);
                    all_ptrs.insert(idx, new_block);

                    let mid = all_keys.len() / 2;
                    let new_separator = all_keys[mid].clone();
                    let new_keys = all_keys.split_off(mid);
                    let new_ptrs = all_ptrs.split_off(mid);

                    let new_node = BmbtInteriorBlock {
                        level: intermediate.level(),
                        keys: new_keys,
                        ptrs: new_ptrs,
                    };
                    intermediate.keys = all_keys;
                    intermediate.ptrs = all_ptrs;

                    // Allocate block for new intermediate node
                    let agno = (offset >> sb.sb_agblklog) as u32;
                    let run_opt = allocate_in_group(tx, sb, agno, 1).map_err(|e| e.errno())?;
                    let run = run_opt.ok_or(libc::ENOSPC)?;
                    let new_block = run.start as XfsFsblock;

                    // Write the modified intermediate node
                    let hdr = BtreeLblockHdr {
                        blkno: offset,
                        lsn: 0,
                        leftsib: BMBT_NULL_PTR,
                        rightsib: BMBT_NULL_PTR,
                        owner: 0,
                        uuid: sb.sb_uuid.as_image_bytes(),
                    };
                    let node_bytes =
                        intermediate.to_bytes_auto(&hdr, sb.sb_blocksize as usize, sb.has_crc());
                    tx.write_bytes(offset, &node_bytes).map_err(|e| e.errno())?;

                    // Write the new intermediate node
                    let new_hdr = BtreeLblockHdr {
                        blkno: new_block,
                        lsn: 0,
                        leftsib: BMBT_NULL_PTR,
                        rightsib: BMBT_NULL_PTR,
                        owner: 0,
                        uuid: sb.sb_uuid.as_image_bytes(),
                    };
                    let new_node_bytes =
                        new_node.to_bytes_auto(&new_hdr, sb.sb_blocksize as usize, sb.has_crc());
                    tx.write_bytes(sb.fsb_to_offset(new_block), &new_node_bytes)
                        .map_err(|e| e.errno())?;

                    Ok(Some((new_node, new_separator, new_block)))
                }
            }
            InsertResult::SplitIntermediate(new_node, separator_key, new_block) => {
                // This case shouldn't happen from leaf split, but handle for completeness
                Ok(Some((new_node, separator_key, new_block)))
            }
        }
    }

    /// Delete extents in a range [startoff, endoff) from the BMBT.
    /// Used for PUNCH_HOLE and truncate.
    #[allow(dead_code)]
    pub fn delete_extents_in_range<R>(
        &mut self,
        buf_reader: &mut R,
        startoff: XfsFileoff,
        endoff: XfsFileoff,
        sb: &Sb,
        tx: &mut Transaction<'_>,
    ) -> Result<Vec<(XfsFsblock, u32)>, i32>
    where
        R: bincode_next::de::read::Reader + BufRead + Seek,
    {
        // For root level 0, use the inode's extent list
        if self.bmdr.bb_level == 0 {
            return Err(libc::ENOSYS);
        }

        // For multi-level tree, descend and delete
        let freed = self.delete_extents_recursive(buf_reader, startoff, endoff, sb, tx)?;

        // After deletion, check if root can be collapsed
        self.check_root_collapse(buf_reader, sb, tx)?;

        Ok(freed)
    }

    /// Recursive deletion for multi-level trees (level >= 1)
    #[allow(dead_code)]
    fn delete_extents_recursive<R>(
        &mut self,
        buf_reader: &mut R,
        startoff: XfsFileoff,
        endoff: XfsFileoff,
        sb: &Sb,
        tx: &mut Transaction<'_>,
    ) -> Result<Vec<(XfsFsblock, u32)>, i32>
    where
        R: bincode_next::de::read::Reader + BufRead + Seek,
    {
        let mut freed_blocks = Vec::new();

        // Find all child indices that overlap with [startoff, endoff)
        let mut affected_children = Vec::new();
        for (idx, key) in self.keys.iter().enumerate() {
            if idx + 1 < self.keys.len() {
                let next_key = self.keys[idx + 1].br_startoff;
                if key.br_startoff < endoff && next_key > startoff {
                    affected_children.push(idx);
                }
            } else {
                // Last child - extends to infinity
                if key.br_startoff < endoff {
                    affected_children.push(idx);
                }
            }
        }

        for &child_idx in &affected_children {
            let child_ptr = self.ptrs[child_idx];
            let offset = sb.fsb_to_offset(child_ptr);

            buf_reader
                .seek(SeekFrom::Start(offset))
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

            let mut child_bytes = vec![0u8; sb.sb_blocksize as usize];
            buf_reader
                .read_exact(&mut child_bytes)
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

            let child_level = self.bmdr.bb_level - 1;

            if child_level == 0 {
                // Child is a leaf
                let mut leaf = BmbtLeafBlock::from_bytes(&child_bytes)
                    .map_err(|_| crate::libxfuse::EUCLEAN)?;

                // Find and remove records in range
                let mut records_to_remove = Vec::new();
                for (idx, record) in leaf.records.iter().enumerate() {
                    let rec_start = record.startoff();
                    let rec_end = rec_start + record.blockcount();
                    if rec_start < endoff && rec_end > startoff {
                        // This record overlaps with the range
                        records_to_remove.push(idx);
                    }
                }

                // For now, handle partial overlaps by splitting records
                // A full implementation would also handle merge/redistribute
                let mut need_merge = false;
                for &idx in records_to_remove.iter().rev() {
                    let record = leaf.records[idx];
                    let rec_start = record.startoff();
                    let rec_end = rec_start + record.blockcount();

                    if rec_start >= startoff && rec_end <= endoff {
                        // Record fully in range - remove it
                        leaf.records.remove(idx);
                        freed_blocks.push((record.startblock(), record.blockcount() as u32));
                    } else if rec_start < startoff && rec_end > endoff {
                        // Record spans the entire hole - split into two
                        let left_extent = BmbtRec {
                            br_startoff: rec_start,
                            br_startblock: record.startblock(),
                            br_blockcount: startoff - rec_start,
                            br_flag: record.extent_flag(),
                        };
                        let right_extent = BmbtRec {
                            br_startoff: endoff,
                            br_startblock: record.startblock() + (endoff - rec_start),
                            br_blockcount: rec_end - endoff,
                            br_flag: record.extent_flag(),
                        };
                        leaf.records.remove(idx);
                        leaf.records
                            .insert(idx, BmbtLeafRecord::from_extent(&left_extent));
                        leaf.records
                            .insert(idx + 1, BmbtLeafRecord::from_extent(&right_extent));

                        let freed_len = endoff - startoff;
                        freed_blocks.push((
                            record.startblock() + (startoff - rec_start),
                            freed_len as u32,
                        ));
                    } else if rec_start < startoff && rec_end > startoff {
                        // Straddles startoff - trim front
                        let kept_blocks = startoff - rec_start;
                        let trimmed_extent = BmbtRec {
                            br_startoff: rec_start,
                            br_startblock: record.startblock(),
                            br_blockcount: kept_blocks,
                            br_flag: record.extent_flag(),
                        };
                        leaf.records.remove(idx);
                        leaf.records
                            .insert(idx, BmbtLeafRecord::from_extent(&trimmed_extent));

                        let freed_len = rec_end - startoff;
                        freed_blocks.push((record.startblock() + kept_blocks, freed_len as u32));
                    } else if rec_start < endoff && rec_end > endoff {
                        // Straddles endoff - trim end
                        let kept_blocks = endoff - rec_start;
                        let trimmed_extent = BmbtRec {
                            br_startoff: rec_start,
                            br_startblock: record.startblock(),
                            br_blockcount: kept_blocks,
                            br_flag: record.extent_flag(),
                        };
                        leaf.records.remove(idx);
                        leaf.records
                            .insert(idx, BmbtLeafRecord::from_extent(&trimmed_extent));

                        let freed_len = rec_end - endoff;
                        freed_blocks.push((record.startblock() + kept_blocks, freed_len as u32));
                    }
                    // Fully inside case already handled above
                }

                let min = BmbtLeafBlock::min_records(sb.sb_blocksize as usize, sb.has_crc());
                if leaf.records.len() < min {
                    need_merge = true;
                }

                // Coalesce adjacent extents after modifications
                leaf.coalesce_adjacent();

                // Write leaf back
                let hdr = BtreeLblockHdr {
                    blkno: child_ptr,
                    lsn: 0,
                    leftsib: BMBT_NULL_PTR,
                    rightsib: BMBT_NULL_PTR,
                    owner: 0,
                    uuid: sb.sb_uuid.as_image_bytes(),
                };
                let leaf_bytes = leaf.to_bytes_auto(&hdr, sb.sb_blocksize as usize, sb.has_crc());
                tx.write_bytes(offset, &leaf_bytes).map_err(|e| e.errno())?;

                if need_merge {
                    // Try to merge/redistribute with sibling
                    self.handle_leaf_underflow(child_idx, child_ptr, &leaf, sb, tx, buf_reader)?;
                }
            } else {
                // Child is an intermediate node - recurse
                let mut intermediate: BtreeIntermediate =
                    decode_from(buf_reader.by_ref()).map_err(|_| libc::EDESTADDRREQ)?;
                let child_freed = self.delete_extents_intermediate(
                    &mut intermediate,
                    buf_reader,
                    startoff,
                    endoff,
                    sb,
                    tx,
                    child_ptr,
                )?;
                freed_blocks.extend(child_freed);

                // Check if intermediate needs merge/redistribute
                let min = BmbtInteriorBlock::min_children(sb.sb_blocksize as usize, sb.has_crc());
                if intermediate.keys.len() < min {
                    self.handle_intermediate_underflow(
                        child_idx,
                        child_ptr,
                        &intermediate,
                        sb,
                        tx,
                        buf_reader,
                    )?;
                }
            }
        }

        Ok(freed_blocks)
    }

    /// Check if the BMBT root can be collapsed (level 2→1 or 1→0).
    /// This happens when the root has only one child and that child can be collapsed.
    #[allow(dead_code)]
    fn check_root_collapse<R>(
        &mut self,
        buf_reader: &mut R,
        sb: &Sb,
        tx: &mut Transaction<'_>,
    ) -> Result<(), i32>
    where
        R: bincode_next::de::read::Reader + BufRead + Seek,
    {
        // Root collapse level 2→1: level 2 root with one child that is an intermediate node
        if self.bmdr.bb_level == 2 && self.keys.len() == 1 && self.ptrs.len() == 1 {
            let child_ptr = self.ptrs[0];
            let offset = sb.fsb_to_offset(child_ptr);

            buf_reader
                .seek(SeekFrom::Start(offset))
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

            let mut child_bytes = vec![0u8; sb.sb_blocksize as usize];
            buf_reader
                .read_exact(&mut child_bytes)
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

            // Verify it's an intermediate node (level 1)
            let mut intermediate = BtreeIntermediate::from_bytes(&child_bytes)
                .map_err(|_| crate::libxfuse::EUCLEAN)?;
            if intermediate.level() == 1
                && intermediate.keys.len() == 1
                && intermediate.ptrs.len() == 1
            {
                // The intermediate has one child - check if it's a leaf
                let leaf_ptr = intermediate.ptrs[0];
                let leaf_offset = sb.fsb_to_offset(leaf_ptr);

                buf_reader
                    .seek(SeekFrom::Start(leaf_offset))
                    .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

                let mut leaf_bytes = vec![0u8; sb.sb_blocksize as usize];
                buf_reader
                    .read_exact(&mut leaf_bytes)
                    .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

                let leaf =
                    BmbtLeafBlock::from_bytes(&leaf_bytes).map_err(|_| crate::libxfuse::EUCLEAN)?;
                if leaf.level == 0 {
                    // Check if leaf records fit in intermediate node
                    let max_keys =
                        BmbtInteriorBlock::max_children(sb.sb_blocksize as usize, sb.has_crc());
                    if leaf.records.len() <= max_keys {
                        // Collapse: move leaf records to intermediate node
                        let new_keys: Vec<BmbtKey> = leaf
                            .records
                            .iter()
                            .map(|r| BmbtKey {
                                br_startoff: r.startoff(),
                            })
                            .collect();
                        let new_ptrs: Vec<XfsFsblock> =
                            leaf.records.iter().map(|r| r.startblock()).collect();

                        // Update intermediate node
                        intermediate.hdr.bb_level = 0;
                        intermediate.hdr.bb_numrecs = leaf.records.len() as u16;
                        intermediate.keys = new_keys;
                        intermediate.ptrs = new_ptrs;

                        // Write updated intermediate (now leaf)
                        let new_hdr = BtreeLblockHdr {
                            blkno: child_ptr,
                            lsn: 0,
                            leftsib: BMBT_NULL_PTR,
                            rightsib: BMBT_NULL_PTR,
                            owner: 0,
                            uuid: sb.sb_uuid.as_image_bytes(),
                        };
                        tx.write_bytes(
                            sb.fsb_to_offset(child_ptr),
                            &intermediate.to_bytes_auto(
                                &new_hdr,
                                sb.sb_blocksize as usize,
                                sb.has_crc(),
                            ),
                        )
                        .map_err(|e| e.errno())?;

                        // Free the leaf block
                        let agno = (leaf_ptr >> sb.sb_agblklog) as u32;
                        let mask = u64::from(1u32 << sb.sb_agblklog) - 1;
                        let start = (leaf_ptr & mask) as u32;
                        crate::libxfuse::alloc::allocator::free_in_group(
                            tx,
                            sb,
                            agno,
                            crate::libxfuse::alloc::free_space::FreeRun { start, len: 1 },
                        )
                        .map_err(|e| e.errno())?;

                        // Update root to level 1 (now points to leaf)
                        self.bmdr.bb_level = 1;
                        self.bmdr.bb_numrecs = 1;
                        self.keys = vec![BmbtKey {
                            br_startoff: intermediate.keys[0].br_startoff,
                        }];
                        self.ptrs = vec![child_ptr];
                    }
                }
            }
        }

        // Root collapse level 1→0: level 1 root with one child that is a leaf
        if self.bmdr.bb_level == 1 && self.keys.len() == 1 && self.ptrs.len() == 1 {
            let child_ptr = self.ptrs[0];
            let offset = sb.fsb_to_offset(child_ptr);

            buf_reader
                .seek(SeekFrom::Start(offset))
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

            let mut child_bytes = vec![0u8; sb.sb_blocksize as usize];
            buf_reader
                .read_exact(&mut child_bytes)
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

            // Verify it's a leaf (level 0)
            let leaf =
                BmbtLeafBlock::from_bytes(&child_bytes).map_err(|_| crate::libxfuse::EUCLEAN)?;
            if leaf.level == 0 {
                // Check if all records fit in inode
                let max_extents = self.max_inode_extents(sb);
                if leaf.records.len() <= max_extents {
                    // Collapse: move records to inode, free leaf block
                    let extents: Vec<BmbtRec> =
                        leaf.records.iter().map(|r| r.as_extent()).collect();

                    // Write extents to inode
                    self.write_extents_to_inode(&extents, sb, tx)?;

                    // Free the leaf block
                    let agno = (child_ptr >> sb.sb_agblklog) as u32;
                    let mask = u64::from(1u32 << sb.sb_agblklog) - 1;
                    let start = (child_ptr & mask) as u32;
                    crate::libxfuse::alloc::allocator::free_in_group(
                        tx,
                        sb,
                        agno,
                        crate::libxfuse::alloc::free_space::FreeRun { start, len: 1 },
                    )
                    .map_err(|e| e.errno())?;

                    // Update root to level 0
                    self.bmdr.bb_level = 0;
                    self.bmdr.bb_numrecs = 0;
                    self.keys.clear();
                    self.ptrs.clear();
                }
            }
        }
        Ok(())
    }

    /// Handle leaf underflow after deletion by trying to redistribute or merge with sibling.
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    fn handle_leaf_underflow<R>(
        &mut self,
        child_idx: usize,
        child_ptr: XfsFsblock,
        leaf: &BmbtLeafBlock,
        sb: &Sb,
        tx: &mut Transaction<'_>,
        buf_reader: &mut R,
    ) -> Result<(), i32>
    where
        R: bincode_next::de::read::Reader + BufRead + Seek,
    {
        // Try right sibling first
        if child_idx + 1 < self.ptrs.len() {
            let right_ptr = self.ptrs[child_idx + 1];
            let right_offset = sb.fsb_to_offset(right_ptr);

            buf_reader
                .seek(SeekFrom::Start(right_offset))
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
            let mut right_bytes = vec![0u8; sb.sb_blocksize as usize];
            buf_reader
                .read_exact(&mut right_bytes)
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

            let mut right_leaf =
                BmbtLeafBlock::from_bytes(&right_bytes).map_err(|_| crate::libxfuse::EUCLEAN)?;

            // Try redistribute
            let mut leaf_mut = leaf.clone();
            if leaf_mut.redistribute_with(&mut right_leaf) {
                // Write both leaves back
                let left_hdr = BtreeLblockHdr {
                    blkno: child_ptr,
                    lsn: 0,
                    leftsib: BMBT_NULL_PTR,
                    rightsib: BMBT_NULL_PTR,
                    owner: 0,
                    uuid: sb.sb_uuid.as_image_bytes(),
                };
                let right_hdr = BtreeLblockHdr {
                    blkno: right_ptr,
                    lsn: 0,
                    leftsib: BMBT_NULL_PTR,
                    rightsib: BMBT_NULL_PTR,
                    owner: 0,
                    uuid: sb.sb_uuid.as_image_bytes(),
                };
                tx.write_bytes(
                    sb.fsb_to_offset(child_ptr),
                    &leaf_mut.to_bytes_auto(&left_hdr, sb.sb_blocksize as usize, sb.has_crc()),
                )
                .map_err(|e| e.errno())?;
                tx.write_bytes(
                    right_offset,
                    &right_leaf.to_bytes_auto(&right_hdr, sb.sb_blocksize as usize, sb.has_crc()),
                )
                .map_err(|e| e.errno())?;

                // Update separator key in parent
                let new_separator = BmbtKey {
                    br_startoff: right_leaf.records[0].startoff(),
                };
                self.keys[child_idx + 1] = new_separator;
                return Ok(());
            }

            // Redistribute failed, try merge (merge right into left)
            leaf_mut.merge_with(right_leaf);

            // Write merged leaf
            let left_hdr = BtreeLblockHdr {
                blkno: child_ptr,
                lsn: 0,
                leftsib: BMBT_NULL_PTR,
                rightsib: BMBT_NULL_PTR,
                owner: 0,
                uuid: sb.sb_uuid.as_image_bytes(),
            };
            tx.write_bytes(
                sb.fsb_to_offset(child_ptr),
                &leaf_mut.to_bytes_auto(&left_hdr, sb.sb_blocksize as usize, sb.has_crc()),
            )
            .map_err(|e| e.errno())?;

            // Remove the right sibling from parent
            self.keys.remove(child_idx + 1);
            self.ptrs.remove(child_idx + 1);
            self.bmdr.bb_numrecs = self.keys.len() as u16;

            // Free the right sibling block
            let agno = (right_ptr >> sb.sb_agblklog) as u32;
            let mask = u64::from(1u32 << sb.sb_agblklog) - 1;
            let start = (right_ptr & mask) as u32;
            crate::libxfuse::alloc::allocator::free_in_group(
                tx,
                sb,
                agno,
                crate::libxfuse::alloc::free_space::FreeRun { start, len: 1 },
            )
            .map_err(|e| e.errno())?;

            return Ok(());
        }

        // Try left sibling
        if child_idx > 0 {
            let left_ptr = self.ptrs[child_idx - 1];
            let left_offset = sb.fsb_to_offset(left_ptr);

            buf_reader
                .seek(SeekFrom::Start(left_offset))
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
            let mut left_bytes = vec![0u8; sb.sb_blocksize as usize];
            buf_reader
                .read_exact(&mut left_bytes)
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

            let left_leaf =
                BmbtLeafBlock::from_bytes(&left_bytes).map_err(|_| crate::libxfuse::EUCLEAN)?;

            // Try redistribute
            let mut left_leaf_mut = left_leaf.clone();
            let mut leaf_mut = leaf.clone();
            if left_leaf_mut.redistribute_with(&mut leaf_mut) {
                // Write both leaves back
                let left_hdr = BtreeLblockHdr {
                    blkno: left_ptr,
                    lsn: 0,
                    leftsib: BMBT_NULL_PTR,
                    rightsib: BMBT_NULL_PTR,
                    owner: 0,
                    uuid: sb.sb_uuid.as_image_bytes(),
                };
                let right_hdr = BtreeLblockHdr {
                    blkno: child_ptr,
                    lsn: 0,
                    leftsib: BMBT_NULL_PTR,
                    rightsib: BMBT_NULL_PTR,
                    owner: 0,
                    uuid: sb.sb_uuid.as_image_bytes(),
                };
                tx.write_bytes(
                    left_offset,
                    &left_leaf_mut.to_bytes_auto(&left_hdr, sb.sb_blocksize as usize, sb.has_crc()),
                )
                .map_err(|e| e.errno())?;
                tx.write_bytes(
                    sb.fsb_to_offset(child_ptr),
                    &leaf_mut.to_bytes_auto(&right_hdr, sb.sb_blocksize as usize, sb.has_crc()),
                )
                .map_err(|e| e.errno())?;

                // Update separator key in parent
                let new_separator = BmbtKey {
                    br_startoff: leaf_mut.records[0].startoff(),
                };
                self.keys[child_idx] = new_separator;
                return Ok(());
            }

            // Redistribute failed, try merge (merge current into left)
            left_leaf_mut.merge_with(leaf_mut);

            // Write merged leaf
            let left_hdr = BtreeLblockHdr {
                blkno: left_ptr,
                lsn: 0,
                leftsib: BMBT_NULL_PTR,
                rightsib: BMBT_NULL_PTR,
                owner: 0,
                uuid: sb.sb_uuid.as_image_bytes(),
            };
            tx.write_bytes(
                left_offset,
                &left_leaf_mut.to_bytes_auto(&left_hdr, sb.sb_blocksize as usize, sb.has_crc()),
            )
            .map_err(|e| e.errno())?;

            // Remove current leaf from parent
            self.keys.remove(child_idx);
            self.ptrs.remove(child_idx);
            self.bmdr.bb_numrecs = self.keys.len() as u16;

            // Free the current leaf block
            let agno = (child_ptr >> sb.sb_agblklog) as u32;
            let mask = u64::from(1u32 << sb.sb_agblklog) - 1;
            let start = (child_ptr & mask) as u32;
            crate::libxfuse::alloc::allocator::free_in_group(
                tx,
                sb,
                agno,
                crate::libxfuse::alloc::free_space::FreeRun { start, len: 1 },
            )
            .map_err(|e| e.errno())?;

            return Ok(());
        }

        // No siblings to merge with - just leave as is
        Ok(())
    }

    /// Handle intermediate node underflow after deletion by trying to redistribute or merge with sibling.
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    fn handle_intermediate_underflow<R>(
        &mut self,
        child_idx: usize,
        child_ptr: XfsFsblock,
        intermediate: &BtreeIntermediate,
        sb: &Sb,
        tx: &mut Transaction<'_>,
        buf_reader: &mut R,
    ) -> Result<(), i32>
    where
        R: bincode_next::de::read::Reader + BufRead + Seek,
    {
        // Try right sibling first
        if child_idx + 1 < self.ptrs.len() {
            let right_ptr = self.ptrs[child_idx + 1];
            let right_offset = sb.fsb_to_offset(right_ptr);

            buf_reader
                .seek(SeekFrom::Start(right_offset))
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
            let mut right_bytes = vec![0u8; sb.sb_blocksize as usize];
            buf_reader
                .read_exact(&mut right_bytes)
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

            let mut right_intermediate = BtreeIntermediate::from_bytes(&right_bytes)
                .map_err(|_| crate::libxfuse::EUCLEAN)?;

            // Try redistribute
            let mut intermediate_mut = intermediate.clone();
            if intermediate_mut.redistribute_with(&mut right_intermediate) {
                // Write both intermediates back
                let left_hdr = BtreeLblockHdr {
                    blkno: child_ptr,
                    lsn: 0,
                    leftsib: BMBT_NULL_PTR,
                    rightsib: BMBT_NULL_PTR,
                    owner: 0,
                    uuid: sb.sb_uuid.as_image_bytes(),
                };
                let right_hdr = BtreeLblockHdr {
                    blkno: right_ptr,
                    lsn: 0,
                    leftsib: BMBT_NULL_PTR,
                    rightsib: BMBT_NULL_PTR,
                    owner: 0,
                    uuid: sb.sb_uuid.as_image_bytes(),
                };
                tx.write_bytes(
                    sb.fsb_to_offset(child_ptr),
                    &intermediate_mut.to_bytes_auto(
                        &left_hdr,
                        sb.sb_blocksize as usize,
                        sb.has_crc(),
                    ),
                )
                .map_err(|e| e.errno())?;
                tx.write_bytes(
                    right_offset,
                    &right_intermediate.to_bytes_auto(
                        &right_hdr,
                        sb.sb_blocksize as usize,
                        sb.has_crc(),
                    ),
                )
                .map_err(|e| e.errno())?;

                // Update separator key in parent
                let new_separator = BmbtKey {
                    br_startoff: right_intermediate.keys[0].br_startoff,
                };
                self.keys[child_idx + 1] = new_separator;
                return Ok(());
            }

            // Redistribute failed, try merge (merge right into left)
            intermediate_mut.merge_with(right_intermediate);

            // Write merged intermediate
            let left_hdr = BtreeLblockHdr {
                blkno: child_ptr,
                lsn: 0,
                leftsib: BMBT_NULL_PTR,
                rightsib: BMBT_NULL_PTR,
                owner: 0,
                uuid: sb.sb_uuid.as_image_bytes(),
            };
            tx.write_bytes(
                sb.fsb_to_offset(child_ptr),
                &intermediate_mut.to_bytes_auto(&left_hdr, sb.sb_blocksize as usize, sb.has_crc()),
            )
            .map_err(|e| e.errno())?;

            // Remove the right sibling from parent
            self.keys.remove(child_idx + 1);
            self.ptrs.remove(child_idx + 1);
            self.bmdr.bb_numrecs = self.keys.len() as u16;

            // Free the right sibling block
            let agno = (right_ptr >> sb.sb_agblklog) as u32;
            let mask = u64::from(1u32 << sb.sb_agblklog) - 1;
            let start = (right_ptr & mask) as u32;
            crate::libxfuse::alloc::allocator::free_in_group(
                tx,
                sb,
                agno,
                crate::libxfuse::alloc::free_space::FreeRun { start, len: 1 },
            )
            .map_err(|e| e.errno())?;

            return Ok(());
        }

        // Try left sibling
        if child_idx > 0 {
            let left_ptr = self.ptrs[child_idx - 1];
            let left_offset = sb.fsb_to_offset(left_ptr);

            buf_reader
                .seek(SeekFrom::Start(left_offset))
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
            let mut left_bytes = vec![0u8; sb.sb_blocksize as usize];
            buf_reader
                .read_exact(&mut left_bytes)
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

            let left_intermediate =
                BtreeIntermediate::from_bytes(&left_bytes).map_err(|_| crate::libxfuse::EUCLEAN)?;

            // Try redistribute
            let mut left_intermediate_mut = left_intermediate.clone();
            let mut intermediate_mut = intermediate.clone();
            if left_intermediate_mut.redistribute_with(&mut intermediate_mut) {
                // Write both intermediates back
                let left_hdr = BtreeLblockHdr {
                    blkno: left_ptr,
                    lsn: 0,
                    leftsib: BMBT_NULL_PTR,
                    rightsib: BMBT_NULL_PTR,
                    owner: 0,
                    uuid: sb.sb_uuid.as_image_bytes(),
                };
                let right_hdr = BtreeLblockHdr {
                    blkno: child_ptr,
                    lsn: 0,
                    leftsib: BMBT_NULL_PTR,
                    rightsib: BMBT_NULL_PTR,
                    owner: 0,
                    uuid: sb.sb_uuid.as_image_bytes(),
                };
                tx.write_bytes(
                    left_offset,
                    &left_intermediate_mut.to_bytes_auto(
                        &left_hdr,
                        sb.sb_blocksize as usize,
                        sb.has_crc(),
                    ),
                )
                .map_err(|e| e.errno())?;
                tx.write_bytes(
                    sb.fsb_to_offset(child_ptr),
                    &intermediate_mut.to_bytes_auto(
                        &right_hdr,
                        sb.sb_blocksize as usize,
                        sb.has_crc(),
                    ),
                )
                .map_err(|e| e.errno())?;

                // Update separator key in parent
                let new_separator = BmbtKey {
                    br_startoff: intermediate_mut.keys[0].br_startoff,
                };
                self.keys[child_idx] = new_separator;
                return Ok(());
            }

            // Redistribute failed, try merge (merge current into left)
            left_intermediate_mut.merge_with(intermediate_mut);

            // Write merged intermediate
            let left_hdr = BtreeLblockHdr {
                blkno: left_ptr,
                lsn: 0,
                leftsib: BMBT_NULL_PTR,
                rightsib: BMBT_NULL_PTR,
                owner: 0,
                uuid: sb.sb_uuid.as_image_bytes(),
            };
            tx.write_bytes(
                left_offset,
                &left_intermediate_mut.to_bytes_auto(
                    &left_hdr,
                    sb.sb_blocksize as usize,
                    sb.has_crc(),
                ),
            )
            .map_err(|e| e.errno())?;

            // Remove current intermediate from parent
            self.keys.remove(child_idx);
            self.ptrs.remove(child_idx);
            self.bmdr.bb_numrecs = self.keys.len() as u16;

            // Free the current intermediate block
            let agno = (child_ptr >> sb.sb_agblklog) as u32;
            let mask = u64::from(1u32 << sb.sb_agblklog) - 1;
            let start = (child_ptr & mask) as u32;
            crate::libxfuse::alloc::allocator::free_in_group(
                tx,
                sb,
                agno,
                crate::libxfuse::alloc::free_space::FreeRun { start, len: 1 },
            )
            .map_err(|e| e.errno())?;

            return Ok(());
        }

        // No siblings to merge with - just leave as is
        Ok(())
    }

    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    fn delete_extents_intermediate<R>(
        &self,
        intermediate: &mut BtreeIntermediate,
        buf_reader: &mut R,
        startoff: XfsFileoff,
        endoff: XfsFileoff,
        sb: &Sb,
        tx: &mut Transaction<'_>,
        _child_ptr: XfsFsblock,
    ) -> Result<Vec<(XfsFsblock, u32)>, i32>
    where
        R: bincode_next::de::read::Reader + BufRead + Seek,
    {
        let mut freed_blocks = Vec::new();

        // Find all child indices that overlap with [startoff, endoff)
        let mut affected_children = Vec::new();
        for (idx, key) in intermediate.keys.iter().enumerate() {
            if idx + 1 < intermediate.keys.len() {
                let next_key = intermediate.keys[idx + 1].br_startoff;
                if key.br_startoff < endoff && next_key > startoff {
                    affected_children.push(idx);
                }
            } else {
                if key.br_startoff < endoff {
                    affected_children.push(idx);
                }
            }
        }

        for &child_idx in &affected_children {
            let child_ptr = intermediate.ptrs[child_idx];
            let offset = sb.fsb_to_offset(child_ptr);

            buf_reader
                .seek(SeekFrom::Start(offset))
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

            let mut child_bytes = vec![0u8; sb.sb_blocksize as usize];
            buf_reader
                .read_exact(&mut child_bytes)
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

            let child_level = intermediate.level() - 1;

            if child_level == 0 {
                // Child is a leaf
                let mut leaf = BmbtLeafBlock::from_bytes(&child_bytes)
                    .map_err(|_| crate::libxfuse::EUCLEAN)?;

                let mut records_to_remove = Vec::new();
                for (idx, record) in leaf.records.iter().enumerate() {
                    let rec_start = record.startoff();
                    let rec_end = rec_start + record.blockcount();
                    if rec_start < endoff && rec_end > startoff {
                        records_to_remove.push(idx);
                    }
                }

                let mut need_merge = false;
                for &idx in records_to_remove.iter().rev() {
                    let record = leaf.records[idx];
                    let rec_start = record.startoff();
                    let rec_end = rec_start + record.blockcount();

                    if rec_start >= startoff && rec_end <= endoff {
                        leaf.records.remove(idx);
                        freed_blocks.push((record.startblock(), record.blockcount() as u32));
                    }
                }

                let min = BmbtLeafBlock::min_records(sb.sb_blocksize as usize, sb.has_crc());
                if leaf.records.len() < min {
                    need_merge = true;
                }

                // Write leaf back
                let hdr = BtreeLblockHdr {
                    blkno: child_ptr,
                    lsn: 0,
                    leftsib: BMBT_NULL_PTR,
                    rightsib: BMBT_NULL_PTR,
                    owner: 0,
                    uuid: sb.sb_uuid.as_image_bytes(),
                };
                let leaf_bytes = leaf.to_bytes_auto(&hdr, sb.sb_blocksize as usize, sb.has_crc());
                tx.write_bytes(offset, &leaf_bytes).map_err(|e| e.errno())?;

                if need_merge {
                    // Need to merge with sibling
                }
            } else {
                // Recurse deeper
                let mut child_intermediate: BtreeIntermediate =
                    decode_from(buf_reader.by_ref()).map_err(|_| libc::EDESTADDRREQ)?;
                let child_freed = self.delete_extents_intermediate(
                    &mut child_intermediate,
                    buf_reader,
                    startoff,
                    endoff,
                    sb,
                    tx,
                    child_ptr,
                )?;
                freed_blocks.extend(child_freed);
            }
        }

        Ok(freed_blocks)
    }
}

#[derive(Debug, Clone)]
enum BtreeBlockCache {
    Intermediate(BTreeMap<usize, BtreeIntermediate>),
    Leaf(BTreeMap<usize, BtreeLeaf>),
}

impl BtreeBlockCache {
    fn new(level: u16) -> Self {
        if level > 1 {
            BtreeBlockCache::Intermediate(Default::default())
        } else {
            BtreeBlockCache::Leaf(Default::default())
        }
    }
}

/// A root BTree in an extent list.
///
/// This is actually part of the inode, not a separate disk block.  Note that root and intermediate
/// nodes are stored differently on disk: root btrees are stored in the inode, whereas intermediate
/// btrees are stored in a BMA3 block.
#[derive(Debug)]
pub struct BtreeRoot {
    pub bmdr: BmdrBlock,
    pub keys: Vec<BmbtKey>,
    pub ptrs: Vec<XfsBmdrPtr>,
    /// The inode number that owns this b-tree root (only set for the actual root)
    pub owner: Option<XfsIno>,
    /// A cache of the object's extents, indexed by block number
    blocks: RefCell<BtreeBlockCache>,
}

impl BtreeRoot {
    pub fn lseek<R>(&self, buf_reader: &mut R, offset: u64, whence: i32) -> Result<u64, i32>
    where
        R: BufRead + Reader + Seek,
    {
        let sb = super::volume::try_superblock().ok_or(libc::ENODEV)?;

        let mut dblock = offset >> sb.sb_blocklog;
        match self.map_block(buf_reader.by_ref(), dblock, true)? {
            (None, Some(len)) => {
                // A hole, followed by data
                if whence == libc::SEEK_HOLE {
                    Ok(offset)
                } else {
                    // It should be impossible to have two hole extents in a row.  But
                    // double-check.
                    debug_assert!(self
                        .map_block(buf_reader.by_ref(), dblock + len, true)
                        .unwrap()
                        .0
                        .is_some());
                    Ok(offset + (len << sb.sb_blocklog))
                }
            }
            (Some(_), None) => {
                unreachable!(
                    "Btree::map_block should never return None for the length of a data region"
                );
            }
            (Some(_), Some(len)) => {
                // In a data region
                if whence == libc::SEEK_HOLE {
                    // Scan for the next hole
                    dblock += len;
                    loop {
                        match self.map_block(buf_reader.by_ref(), dblock, true)? {
                            (Some(_fsblock), Some(len)) => {
                                dblock += len;
                            }
                            (Some(_fsblock), None) => {
                                unreachable!(
                                    "Btree::map_block should never return None for the length of \
                                     a data region"
                                );
                            }
                            (None, _) => {
                                return Ok(dblock << sb.sb_blocklog);
                            }
                        }
                    }
                } else {
                    Ok(offset)
                }
            }
            (None, None) => {
                // A hole that extends to EOF
                if whence == libc::SEEK_HOLE {
                    Ok(offset)
                } else {
                    Err(libc::ENXIO)
                }
            }
        }
    }

    pub fn new(
        bmdr: BmdrBlock,
        keys: Vec<BmbtKey>,
        ptrs: Vec<XfsBmdrPtr>,
        owner: Option<XfsIno>,
    ) -> Self {
        let blocks = RefCell::new(BtreeBlockCache::new(bmdr.bb_level));
        Self {
            bmdr,
            keys,
            ptrs,
            owner,
            blocks,
        }
    }

    /// Set the owner (inode number) for this B-tree root.
    pub fn set_owner(&mut self, owner: XfsIno) {
        self.owner = Some(owner);
    }
}

impl BtreePriv for BtreeRoot {
    fn block_cache(&self) -> &RefCell<BtreeBlockCache> {
        &self.blocks
    }

    fn keys(&self) -> &[BmbtKey] {
        &self.keys
    }

    fn level(&self) -> u16 {
        self.bmdr.bb_level
    }

    fn ptrs(&self) -> &[XfsBmdrPtr] {
        &self.ptrs
    }
}

impl Btree for BtreeRoot {}

/// Child node in a BMBT tree - either a leaf or an intermediate node.
#[allow(dead_code)]
enum BtreeChild {
    Leaf(BmbtLeafBlock),
    Intermediate(BtreeIntermediate),
}

/// An intermediate Btree.
#[derive(Debug, Clone)]
struct BtreeIntermediate {
    hdr: XfsBmbtLblock,
    keys: Vec<BmbtKey>,
    ptrs: Vec<XfsBmbtPtr>,
    /// A cache of the object's extents, indexed by block number
    blocks: RefCell<BtreeBlockCache>,
}

impl BtreePriv for BtreeIntermediate {
    fn block_cache(&self) -> &RefCell<BtreeBlockCache> {
        &self.blocks
    }

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

impl Btree for BtreeIntermediate {}

impl BtreeIntermediate {
    /// Serialize this intermediate node to bytes.
    #[allow(dead_code)]
    pub fn to_bytes(&self, hdr: &BtreeLblockHdr, sb_blocksize: usize) -> Vec<u8> {
        let mut b = vec![0u8; sb_blocksize];
        let room = BmbtInteriorBlock::max_children(sb_blocksize, true);
        put_header(
            &mut b,
            XFS_BMAP_CRC_MAGIC,
            self.hdr.bb_level,
            self.keys.len(),
            hdr,
            sb_blocksize,
        );
        let mut at = BMBT_CRC_HEADER_LEN;
        for k in &self.keys {
            b[at..at + 8].copy_from_slice(&k.br_startoff.to_be_bytes());
            at += BmbtKey::SIZE;
        }
        at = BMBT_CRC_HEADER_LEN + room * BmbtKey::SIZE;
        for p in &self.ptrs {
            b[at..at + 8].copy_from_slice(&p.to_be_bytes());
            at += 8;
        }
        let crc = crc32c_without_its_own_field(&b, 64);
        b[64..68].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// This node as the bytes of a block on a file system without checksums.
    #[allow(dead_code)]
    pub fn to_bytes_v4(
        &self,
        leftsib: XfsFsblock,
        rightsib: XfsFsblock,
        sb_blocksize: usize,
    ) -> Vec<u8> {
        let mut b = vec![0u8; sb_blocksize];
        put_header(
            &mut b,
            XFS_BMAP_MAGIC,
            self.hdr.bb_level,
            self.keys.len(),
            &BtreeLblockHdr {
                blkno: 0,
                lsn: 0,
                leftsib,
                rightsib,
                owner: 0,
                uuid: [0; 16],
            },
            sb_blocksize,
        );
        let room = BmbtInteriorBlock::max_children(sb_blocksize, false);
        let mut at = BMBT_HEADER_LEN;
        for k in &self.keys {
            b[at..at + 8].copy_from_slice(&k.br_startoff.to_be_bytes());
            at += BmbtKey::SIZE;
        }
        at = BMBT_HEADER_LEN + room * BmbtKey::SIZE;
        for p in &self.ptrs {
            b[at..at + 8].copy_from_slice(&p.to_be_bytes());
            at += 8;
        }
        b
    }

    /// Serialize this node for the file system described by `has_crc`.
    pub fn to_bytes_auto(
        &self,
        hdr: &BtreeLblockHdr,
        sb_blocksize: usize,
        has_crc: bool,
    ) -> Vec<u8> {
        if has_crc {
            self.to_bytes(hdr, sb_blocksize)
        } else {
            self.to_bytes_v4(hdr.leftsib, hdr.rightsib, sb_blocksize)
        }
    }

    /// Deserialize an intermediate node from bytes.
    #[allow(dead_code)]
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, FsError> {
        let config = bincode_next::config::standard()
            .with_big_endian()
            .with_fixed_int_encoding();
        let reader = bincode_next::de::read::SliceReader::new(bytes);
        let mut decoder = bincode_next::de::DecoderImpl::new(reader, config, ());

        let hdr = XfsBmbtLblock::decode(&mut decoder)
            .map_err(|e| FsError::corrupt(format!("intermediate node header: {e:?}")))?;

        // Check magic
        if hdr.bb_magic != XFS_BMAP_CRC_MAGIC && hdr.bb_magic != XFS_BMAP_MAGIC {
            return Err(FsError::corrupt(format!(
                "bad intermediate node magic 0x{:x}",
                hdr.bb_magic
            )));
        }

        let has_crc = hdr.bb_magic == XFS_BMAP_CRC_MAGIC;
        let max_children = BmbtInteriorBlock::max_children(bytes.len(), has_crc);

        let mut keys = Vec::with_capacity(hdr.bb_numrecs as usize);
        for _ in 0..hdr.bb_numrecs {
            keys.push(
                BmbtKey::decode(&mut decoder)
                    .map_err(|e| FsError::corrupt(format!("intermediate node key: {e:?}")))?,
            );
        }

        // Skip padding to pointers
        let header_len = if has_crc {
            BMBT_CRC_HEADER_LEN
        } else {
            BMBT_HEADER_LEN
        };
        let keys_end = header_len + max_children * BmbtKey::SIZE;
        let ptrs_start = keys_end;

        // We need to seek to the pointers position
        // Since we're using SliceReader, we need to manually skip
        // For simplicity, let's decode the rest manually
        let mut ptrs = Vec::with_capacity(hdr.bb_numrecs as usize);
        for i in 0..hdr.bb_numrecs as usize {
            let offset = ptrs_start + i * 8;
            if offset + 8 <= bytes.len() {
                let ptr = u64::from_be_bytes(bytes[offset..offset + 8].try_into().unwrap());
                ptrs.push(ptr);
            }
        }

        Ok(Self {
            hdr,
            keys,
            ptrs,
            blocks: RefCell::new(BtreeBlockCache::new(hdr.bb_level)),
        })
    }

    /// Merge this node with another node (right sibling).
    #[allow(dead_code)]
    pub fn merge_with(&mut self, other: BtreeIntermediate) {
        self.keys.extend(other.keys);
        self.ptrs.extend(other.ptrs);
    }

    /// Redistribute key-pointer pairs with a sibling to maintain minimum occupancy.
    #[allow(dead_code)]
    pub fn redistribute_with(&mut self, other: &mut BtreeIntermediate) -> bool {
        let total = self.keys.len() + other.keys.len();
        if total < 2 * BmbtInteriorBlock::min_children(0, false) {
            // Can't redistribute while maintaining minimum
            return false;
        }
        // Combine and split evenly
        let mut all_keys = self.keys.clone();
        all_keys.append(&mut other.keys);
        let mut all_ptrs = self.ptrs.clone();
        all_ptrs.append(&mut other.ptrs);
        // Re-sort by key
        let combined: Vec<_> = all_keys.into_iter().zip(all_ptrs).collect();
        let mut combined_sorted = combined;
        combined_sorted.sort_by_key(|(k, _)| k.br_startoff);
        let mid = combined_sorted.len() / 2;
        let (new_self_keys, new_self_ptrs): (Vec<_>, Vec<_>) =
            combined_sorted[..mid].iter().cloned().unzip();
        let (new_other_keys, new_other_ptrs): (Vec<_>, Vec<_>) =
            combined_sorted[mid..].iter().cloned().unzip();
        self.keys = new_self_keys;
        self.ptrs = new_self_ptrs;
        other.keys = new_other_keys;
        other.ptrs = new_other_ptrs;
        true
    }

    /// Insert an extent record into this intermediate node's subtree.
    /// Returns Ok(()) on success, or Err if the subtree needs to be split and the
    /// separator key needs to be propagated up.
    #[allow(dead_code)]
    pub fn insert_extent<R>(
        &mut self,
        buf_reader: &mut R,
        extent: BmbtRec,
        _sb: &Sb,
    ) -> Result<Option<(BmbtKey, XfsBmbtPtr)>, i32>
    where
        R: bincode_next::de::read::Reader + BufRead + Seek,
    {
        // Traverse down to find the appropriate child
        let record = BmbtLeafRecord::from_extent(&extent);
        let record_startoff = record.startoff();
        let child_idx = self
            .keys
            .partition_point(|k| k.br_startoff < record_startoff);

        // Recursively insert into the child
        let child_ptr = self.ptrs[child_idx];
        let offset = super::volume::try_superblock()
            .ok_or(libc::ENODEV)?
            .fsb_to_offset(child_ptr);

        buf_reader
            .seek(SeekFrom::Start(offset))
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;

        let _child = if self.level() == 1 {
            // Child is a leaf
            let mut bytes = vec![
                0u8;
                super::volume::try_superblock()
                    .ok_or(libc::ENODEV)?
                    .sb_blocksize as usize
            ];
            buf_reader
                .read_exact(&mut bytes)
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
            BtreeChild::Leaf(
                BmbtLeafBlock::from_bytes(&bytes).map_err(|_| crate::libxfuse::EUCLEAN)?,
            )
        } else {
            // Child is an intermediate node
            let mut bytes = vec![
                0u8;
                super::volume::try_superblock()
                    .ok_or(libc::ENODEV)?
                    .sb_blocksize as usize
            ];
            buf_reader
                .read_exact(&mut bytes)
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
            let child: BtreeIntermediate =
                decode_from(buf_reader.by_ref()).map_err(|_| libc::EDESTADDRREQ)?;
            BtreeChild::Intermediate(child)
        };

        // For now, this is a placeholder
        Err(libc::ENOSYS)
    }
}

impl<Ctx> Decode<Ctx> for BtreeIntermediate {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let blocksize = super::volume::try_superblock()
            .ok_or_else(|| DecodeError::OtherString(super::volume::NO_IMAGE.to_string()))?
            .sb_blocksize as usize;
        let mut raw = vec![0u8; blocksize];
        decoder.reader().read(&mut raw)?;
        let (hdr, mut ofs) = decode::<XfsBmbtLblock>(&raw)?;
        assert!(hdr.bb_level > 0);

        let mut keys = Vec::with_capacity(usize::from(hdr.bb_numrecs));
        for _ in 0..hdr.bb_numrecs {
            let (key, keylen) = decode(&raw[ofs..])?;
            ofs += keylen;
            keys.push(key);
        }

        // The XFS Algorithms & Data Structures document section
        // 16.2 says that the pointers start at offset 0x808 within the block.  But for V5 file
        // systems it looks to me like they really start at offset 0x820.
        ofs = match hdr.bb_magic {
            XFS_BMAP_MAGIC => blocksize / 2 + 0x08,
            XFS_BMAP_CRC_MAGIC => blocksize / 2 + 0x20,
            _ => unreachable!(),
        };
        let mut ptrs = Vec::with_capacity(usize::from(hdr.bb_numrecs));
        for _ in 0..hdr.bb_numrecs {
            let (ptr, ptrlen) = decode(&raw[ofs..])?;
            ofs += ptrlen;
            ptrs.push(ptr);
        }

        let blocks = RefCell::new(BtreeBlockCache::new(hdr.bb_level));
        Ok(Self {
            hdr,
            keys,
            ptrs,
            blocks,
        })
    }
}

/// A Leaf Btree.
#[derive(Debug, Clone)]
struct BtreeLeaf {
    bmx: Bmx,
}

impl BtreeLeaf {
    /// Return the extent, if any, that contains the given block within the file.
    /// Return its starting position as an FSblock, and its length in file system block units.
    /// If a hole's length extends to EoF, return None for length.
    /// If `filter_unwritten` is true, unwritten extents (br_flag=true) are treated as holes.
    pub fn get_extent(
        &self,
        dblock: XfsFileoff,
        filter_unwritten: bool,
    ) -> (Option<XfsFsblock>, Option<u64>) {
        if filter_unwritten {
            // Filter out unwritten extents for lseek purposes
            let written_extents: Vec<BmbtRec> = self
                .bmx
                .extents()
                .iter()
                .filter(|e| !e.br_flag)
                .cloned()
                .collect();
            let bmx = Bmx::from(written_extents);
            bmx.get_extent(dblock)
        } else {
            self.bmx.get_extent(dblock)
        }
    }
}

impl BtreeLeaf {
    /// Read a leaf block's bytes.
    ///
    /// This used to decode the header with `XfsBmbtLblock` and then each record
    /// with `BmbtRec`, which is wrong in two ways and had never been run:
    ///
    /// * `BmbtRec`'s decoder is the *inode's* packed form.  Inside a block a record
    ///   is three four-byte fields at a sixteen-byte stride, and decoding sixteen
    ///   bytes as one packed integer reads three fields as one nonsense number.
    /// * `assert_eq!(hdr.bb_level, 0)` is a panic on the content of an image, and
    ///   this program reads images it did not write.
    ///
    /// Both are now errors, and the layout is the measured one.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        match BmbtLeafBlock::from_bytes(bytes) {
            Ok(block) if block.level == 0 => Ok(Self {
                bmx: block.extents(),
            }),
            Ok(block) => Err(DecodeError::OtherString(format!(
                "a b-map leaf claims to be {} levels down",
                block.level
            ))),
            Err(e) => Err(DecodeError::OtherString(format!("{e:?}"))),
        }
    }
}
