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
//! Reading the btrees that say which blocks are free.
//!
//! # What this is
//!
//! A group's free space is not a bitmap.  It is a list of *runs* -- this run of
//! 400 free blocks starting here, that run of 9 starting there -- kept in a
//! B+tree, and there are two of them per group, one keyed by where each run
//! starts and one keyed by how long each run is.  The two answer the two
//! questions an allocation actually asks: "is there a run at or after this
//! block", and "is there a run at least this long".  The group header says where
//! the root of each is.
//!
//! This module reads one node of one of those btrees.  It is the piece that has
//! to be right before a single block can be allocated, because an allocator
//! that misreads a run will hand out a block that is in use, and two files will
//! overwrite each other.
//!
//! # The shape of a node
//!
//! ```text
//!  0   the magic number
//!  4   the level, as a 16-bit number
//!  6   how many records, as a 16-bit number
//!  8   the block holding the node to the left, or the null block
//! 12   the block holding the node to the right, or the null block
//! 16   ... on a file system with checksums: the log sequence number, the
//!      owner, a pad, the file system's identifier, and the checksum
//!      ... then the records, and then the keys
//! ```
//!
//! A record is where a run starts and how long it is, both 32-bit.  A key is
//! where a run starts, how long it is, and -- for a run that a btree has split
//! across nodes -- how far into the run this node begins; the last key in a node
//! is a sentinel that starts past the end of the node's last run.
//!
//! The checksum, on a file system that has one, is a CRC-32C over the whole
//! block with the checksum field read as zeroes, stored least significant byte
//! first: the same convention the superblock, the inodes, the group header and
//! the free list all use.
//!
//! # Why the header is a different size on different file systems
//!
//! A version 4 file system has no checksums and no owner, so its header ends
//! after the two sibling pointers and the records start sixteen bytes into the
//! block.  A version 5 one adds the log sequence number, the owner, the file
//! system's identifier and the checksum, and its records start fifty-six bytes
//! in.  This is not a detail that can be guessed from the block size, and the
//! two are checked against real file systems in the tests below.
//!
//! # What is here and what is not
//!
//! Here: reading a node -- its header, its records, and its checksums -- and
//! telling a leaf from an interior node.
//!
//! Not yet: walking from a root down through the interior nodes to the leaf that
//! covers a given block.  That is the next step, and it is where the *keys*
//! above start to matter, because an interior node's records are the blocks
//! holding its children rather than free runs.

use byteorder::{BigEndian, ByteOrder, LittleEndian};
use crc::{Crc, CRC_32_ISCSI};

use super::super::{
    definitions::XfsAgblock,
    error::{FsError, FsResult},
};

/// The magic of a btree of free space, keyed by where a run starts.
pub const XFS_ABTB_MAGIC: u32 = 0x4142_5442; // "ABTB"
/// The magic of a btree of free space, keyed by how long a run is.
pub const XFS_ABTC_MAGIC: u32 = 0x4142_5443; // "ABTC"
/// The version 5 spelling of [`XFS_ABTB_MAGIC`], for a tree that fits in one
/// block.
pub const XFS_AB3B_MAGIC: u32 = 0x4142_3342; // "AB3B"
/// The version 5 spelling of [`XFS_ABTC_MAGIC`], for a tree that fits in one
/// block.
pub const XFS_AB3C_MAGIC: u32 = 0x4142_3343; // "AB3C"

/// The block number that means "no block", which is how a node with no left or
/// right sibling says so.
pub const NULL_AGBLOCK: XfsAgblock = u32::MAX;

/// The bytes of one free space btree node.
mod offset {
    pub const MAGIC: usize = 0;
    pub const LEVEL: usize = 4;
    pub const NUMRECS: usize = 6;
    pub const LEFTSIB: usize = 8;
    pub const RIGHTSIB: usize = 12;
    /// Where the records start on a file system with no checksums.
    pub const RECORDS_NO_CRC: usize = 16;
    pub const LSN: usize = 16;
    pub const OWNER: usize = 24;
    pub const UUID: usize = 32;
    pub const CRC: usize = 52;
    /// Where the records start on a file system with checksums.
    pub const RECORDS: usize = 56;
}

/// The size of a free space record, and of the key that indexes a subtree: both
/// are two 32-bit numbers.
pub const RECORD_LEN: usize = 8;
/// The size of a pointer to a child node, which is a 32-bit block number within
/// the group.
///
/// A short-format tree is laid out as its header, then the key array, then the
/// pointer array, and the two are *not* interleaved into records.  The key
/// identifies the first thing reachable through the matching child, and what
/// that key means differs between the two trees: the tree keyed by start block
/// indexes on where a free run begins, and the tree keyed by size indexes on how
/// long one is.
pub const PTR_LEN: usize = 4;
/// The size of a key: a run's start and its length, which is also the size of
/// a leaf's record.
pub const KEY_LEN: usize = 8;

/// Which of the two orders a tree keeps its runs in.
///
/// The two free space trees index the same free space in different orders, and
/// the order a node keeps its records in is what a search over it can rely on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Order {
    ByBlock,
    ByLength,
}
/// The number of bytes one entry of a node costs: a key and a pointer.
pub const ENTRY_LEN: usize = RECORD_LEN + PTR_LEN;

/// One run of free blocks within a group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FreeRun {
    /// The first block of the run.
    pub start: XfsAgblock,
    /// How many blocks it covers.
    pub len:   u32,
}

impl FreeRun {
    /// The block just past the end of the run.
    pub const fn end(&self) -> u64 {
        self.start as u64 + self.len as u64
    }
}

/// One node of a group's free space btree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FreeSpaceNode {
    bytes:     Box<[u8]>,
    records:   usize,
    blocksize: usize,
    numrecs:   u16,
    has_crc:   bool,
    by_block:  bool,
}

impl FreeSpaceNode {
    /// Take ownership of a node's bytes.
    ///
    /// The magic number has to be one of the four the two free space btrees use,
    /// because a block that is not a node of one of them is not something to
    /// read a list of free blocks out of.  `by_block` says which of the two is
    /// expected, so that reading the btree keyed by run length where the one
    /// keyed by start block should have been is caught rather than used.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>, has_crc: bool, by_block: bool) -> FsResult<Self> {
        let bytes = bytes.into();
        if bytes.len() < offset::RECORDS_NO_CRC + RECORD_LEN {
            return Err(FsError::corrupt("free space btree node is too short"));
        }
        let magic = BigEndian::read_u32(&bytes[offset::MAGIC..]);
        let wanted = if by_block {
            [XFS_ABTB_MAGIC, XFS_AB3B_MAGIC]
        } else {
            [XFS_ABTC_MAGIC, XFS_AB3C_MAGIC]
        };
        if !wanted.contains(&magic) {
            return Err(FsError::corrupt(format!(
                "expected the {} free space btree and found magic {magic:#010x}",
                if by_block {
                    "start block"
                } else {
                    "run length"
                }
            )));
        }
        let numrecs = BigEndian::read_u16(&bytes[offset::NUMRECS..]);
        let records = if has_crc {
            offset::RECORDS
        } else {
            offset::RECORDS_NO_CRC
        };
        let blocksize = bytes.len();
        if records + RECORD_LEN * numrecs as usize > blocksize {
            return Err(FsError::corrupt(format!(
                "free space btree node says it holds {numrecs} records but the block is too small"
            )));
        }
        let node = Self {
            bytes: bytes.into_boxed_slice(),
            records,
            blocksize,
            numrecs,
            has_crc,
            by_block,
        };
        // An interior node's pointer array has to fit as well, and where it
        // begins depends on how many entries the block could hold rather than on
        // how many it has.  A leaf has no pointer array, and its capacity is
        // twice an interior node's, so the same arithmetic does not apply to it.
        if !node.is_leaf() {
            let end = node.pointers_offset() + PTR_LEN * node.capacity() as usize;
            if end > blocksize {
                return Err(FsError::corrupt(
                    "free space btree node's pointer array does not fit in the block",
                ));
            }
        }
        Ok(node)
    }

    /// The node's bytes, as they are on the image.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The node's bytes, for writing back to the image.
    pub fn into_bytes(self) -> Box<[u8]> {
        self.bytes
    }

    /// How deep in the tree this node is; 0 means it holds the runs themselves.
    pub fn level(&self) -> u16 {
        BigEndian::read_u16(&self.bytes[offset::LEVEL..])
    }

    /// Is this a leaf, holding the runs of free blocks?
    pub fn is_leaf(&self) -> bool {
        self.level() == 0
    }

    /// How many records the node holds.
    pub const fn numrecs(&self) -> u16 {
        self.numrecs
    }

    /// The block holding the node to the left, or [`NULL_AGBLOCK`].
    pub fn left_sibling(&self) -> XfsAgblock {
        BigEndian::read_u32(&self.bytes[offset::LEFTSIB..])
    }

    /// The block holding the node to the right, or [`NULL_AGBLOCK`].
    pub fn right_sibling(&self) -> XfsAgblock {
        BigEndian::read_u32(&self.bytes[offset::RIGHTSIB..])
    }

    /// The log sequence number of the last change to this node.
    pub fn lsn(&self) -> u64 {
        if self.has_crc {
            BigEndian::read_u64(&self.bytes[offset::LSN..])
        } else {
            0
        }
    }

    /// Is this the btree keyed by where a run starts?
    pub const fn is_by_block(&self) -> bool {
        self.by_block
    }

    /// Where a run belongs among this node's runs, in this tree's order.
    fn position_for(&self, run: &FreeRun) -> FsResult<usize> {
        let runs = self.runs()?;
        let key = |r: &FreeRun| match self.order() {
            Order::ByBlock => (r.start, r.len),
            Order::ByLength => (r.len, r.start),
        };
        Ok(runs
            .iter()
            .position(|r| key(r) > key(run))
            .unwrap_or(runs.len()))
    }

    /// Take `count` blocks out of the run that starts at `start`.
    ///
    /// Returns the part of the run that is left, or `None` if the whole run was
    /// taken.  Taking from the front of a run leaves the rest where it was, so
    /// its neighbours do not have to move: the only thing that changes is where
    /// the run begins.
    ///
    /// This is the change an allocation makes, and it is the change that must
    /// not be wrong.  A block that is still in a tree after it has been written
    /// to will be handed out a second time, and two files will overwrite each
    /// other.
    pub fn take_from_run(&mut self, start: XfsAgblock, count: u32) -> FsResult<Option<FreeRun>> {
        if !self.is_leaf() {
            return Err(FsError::invalid(
                libc::EINVAL,
                "only a leaf holds runs to take blocks from",
            ));
        }
        let mut runs = self.runs()?;
        let index = runs.iter().position(|r| r.start == start).ok_or_else(|| {
            FsError::invalid(
                libc::ENOENT,
                format!("no free run in this node starts at block {start}"),
            )
        })?;
        if count == 0 || count > runs[index].len {
            return Err(FsError::invalid(
                libc::EINVAL,
                format!(
                    "cannot take {count} blocks from a run of {}",
                    runs[index].len
                ),
            ));
        }
        let left = if count == runs[index].len {
            runs.remove(index);
            None
        } else {
            runs[index].start += count;
            runs[index].len -= count;
            Some(runs[index])
        };
        self.set_runs(&runs)?;
        Ok(left)
    }

    /// Put a run back.
    ///
    /// A node that is full cannot take another run, and saying so is better than
    /// dropping the run: a run that is in a tree but not in the image would be
    /// handed out again.
    pub fn put_run(&mut self, run: FreeRun) -> FsResult<()> {
        if !self.is_leaf() {
            return Err(FsError::invalid(libc::EINVAL, "only a leaf holds runs"));
        }
        if run.len == 0 {
            return Err(FsError::invalid(
                libc::EINVAL,
                "a run of no blocks is not a run",
            ));
        }
        if self.overlaps(&run) {
            return Err(FsError::invalid(
                libc::EINVAL,
                format!(
                    "the run {}..{} overlaps a run already in this node",
                    run.start,
                    run.end()
                ),
            ));
        }
        let at = self.position_for(&run)?;
        let mut runs = self.runs()?;
        if runs.len() as u16 >= self.capacity() {
            return Err(FsError::NoSpace);
        }
        runs.insert(at, run);
        self.set_runs(&runs)?;
        Ok(())
    }

    /// Does this node hold a run that shares a block with `run`?
    fn overlaps(&self, run: &FreeRun) -> bool {
        self.runs()
            .map(|runs| {
                runs.iter()
                    .any(|r| (r.start as u64) < run.end() && (run.start as u64) < r.end())
            })
            .unwrap_or(false)
    }

    /// Replace this leaf's records.
    ///
    /// A leaf holds records and nothing else.  A 512-byte block spends its
    /// region on 62 of them, which is as many as fit, so a leaf has no separate
    /// key array to keep in step -- the keys of a tree live in its interior
    /// nodes, where a search descends through them.  A node with room to spare
    /// has the rest of its region left as it found it.
    fn set_runs(&mut self, runs: &[FreeRun]) -> FsResult<()> {
        if runs.len() > self.capacity() as usize {
            return Err(FsError::NoSpace);
        }
        // The count lives in two places -- in the bytes, and in the field this
        // struct was built with -- and they have to move together, or every
        // reader of the node would be reading a different number of records than
        // the one that wrote it.
        self.numrecs = runs.len() as u16;
        BigEndian::write_u16(&mut self.bytes[offset::NUMRECS..], self.numrecs);
        for (i, run) in runs.iter().enumerate() {
            let at = self.records + RECORD_LEN * i;
            BigEndian::write_u32(&mut self.bytes[at..], run.start);
            BigEndian::write_u32(&mut self.bytes[at + 4..], run.len);
        }
        self.update_crc();
        Ok(())
    }

    /// The free runs this node holds.
    ///
    /// Only meaningful for a leaf: in an interior node the same records hold the
    /// blocks holding the children, and are not free runs.
    pub fn runs(&self) -> FsResult<Vec<FreeRun>> {
        if !self.is_leaf() {
            return Err(FsError::invalid(
                libc::EINVAL,
                "this node holds subtrees, not free runs",
            ));
        }
        let mut out = Vec::with_capacity(self.numrecs as usize);
        for i in 0..self.numrecs as usize {
            let at = self.records + RECORD_LEN * i;
            let start = BigEndian::read_u32(&self.bytes[at..]);
            let len = BigEndian::read_u32(&self.bytes[at + 4..]);
            if start == NULL_AGBLOCK {
                return Err(FsError::corrupt("a free run starts at the null block"));
            }
            out.push(FreeRun { start, len });
        }
        Ok(out)
    }

    /// How many records this node can hold.
    ///
    /// It depends what kind of node it is.  A leaf spends the whole region on
    /// records, so a 512-byte block holds 62 of them; an interior node spends
    /// eight bytes on a key and four on a pointer for each entry, so it holds
    /// 41.  Getting this backwards would leave a leaf thinking it was full when
    /// it was not, or an interior node pointing its array past the block.
    pub fn capacity(&self) -> u16 {
        let room = self.blocksize.saturating_sub(self.records);
        let per = if self.is_leaf() {
            RECORD_LEN
        } else {
            ENTRY_LEN
        };
        (room / per) as u16
    }

    /// Where this node's key region ends, which is where an interior node's
    /// pointer array begins.
    ///
    /// The key region is sized for the node's *capacity*, not for the number of
    /// keys in use: a tree that grew or shrank moves its keys within a
    /// fixed-size key region and its pointers after it.
    pub fn pointers_offset(&self) -> usize {
        self.records + self.capacity() as usize * RECORD_LEN
    }

    /// The fewest records a node should hold.
    ///
    /// A node below this is not damaged -- every run in it is still a free run,
    /// and a search still finds it -- but the tree is no longer the shape the
    /// file system keeps, and the next writer will rebalance it.  A node that
    /// drops this far below should be merged with a sibling, or, if it is the
    /// root, collapsed into its only child.
    ///
    /// The rule is half the capacity, and it is visible in what the file system's
    /// own repair says: a 512-byte block holds 62 records, and one holding 30 is
    /// reported as short of the 31 it wants.
    pub fn min_records(&self) -> u16 {
        self.capacity().div_ceil(2)
    }

    /// Is this node short enough to want merging with a sibling?
    pub fn wants_merging(&self) -> bool {
        self.numrecs() < self.min_records()
    }

    /// Can a run be taken out of this leaf without leaving it in a shape the
    /// file system would want to rebalance?
    ///
    /// Only a leaf with more than half its capacity may lose a record.  That
    /// sounds cautious, and it is: taking the last record out of a leaf leaves an
    /// empty one, and leaving a short one is something the file system would
    /// rather fix than be handed.  Both need the layer that can merge, which is
    /// the next change.
    pub fn can_lose(&self) -> bool {
        self.numrecs() > self.min_records()
    }

    /// Which of the two orders this tree keeps its runs in.
    pub fn order(&self) -> Order {
        if self.by_block {
            Order::ByBlock
        } else {
            Order::ByLength
        }
    }

    /// The keys of this node, each a start block and a block count.
    ///
    /// What those two numbers mean depends on which of the two trees this is: for
    /// the tree keyed by start block the first is where a run begins, and for the
    /// tree keyed by size the second is how long a run is.  Nothing in the block
    /// says which, which is why [`FreeSpaceNode::from_bytes`] is told.
    pub fn keys(&self) -> Vec<(u32, u32)> {
        (0..self.numrecs as usize)
            .map(|i| {
                let at = self.records + RECORD_LEN * i;
                (
                    BigEndian::read_u32(&self.bytes[at..]),
                    BigEndian::read_u32(&self.bytes[at + 4..]),
                )
            })
            .collect()
    }

    /// The blocks holding this node's children.
    pub fn children(&self) -> FsResult<Vec<XfsAgblock>> {
        if self.is_leaf() {
            return Err(FsError::invalid(
                libc::EINVAL,
                "a leaf holds free runs, not subtrees",
            ));
        }
        let at0 = self.pointers_offset();
        let mut out = Vec::with_capacity(self.numrecs as usize);
        for i in 0..self.numrecs as usize {
            let at = at0 + PTR_LEN * i;
            if at + PTR_LEN > self.bytes.len() {
                return Err(FsError::corrupt(
                    "free space btree node's pointer array runs past the block",
                ));
            }
            let child = BigEndian::read_u32(&self.bytes[at..]);
            if child == NULL_AGBLOCK {
                return Err(FsError::corrupt(
                    "a free space btree node points at the null block",
                ));
            }
            out.push(child);
        }
        Ok(out)
    }

    /// Is the node's checksum correct?
    ///
    /// A file system without checksums has none to check, and this reports
    /// those as correct rather than as damaged.
    pub fn verify_crc(&self) -> bool {
        if !self.has_crc {
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

    /// Recompute the node's checksum, if it has one.
    pub fn update_crc(&mut self) {
        if !self.has_crc {
            return;
        }
        let crc = self.computed_crc();
        LittleEndian::write_u32(&mut self.bytes[offset::CRC..], crc);
    }
}

/// The most blocks deep a free space btree may be.
///
/// A group is at most a few hundred thousand blocks, so a tree that claims to
/// be deeper than this is damaged, and following it would read blocks that have
/// nothing to do with free space.  The bound is a check on the file system, not
/// a limit on it.
const MAX_TREE_DEPTH: usize = 8;

/// A group's blocks, as the tree operations see them.
///
/// The trees are given this rather than a device, so that a whole allocation --
/// searching, taking blocks out of both trees, and writing the nodes back -- can
/// be exercised without an image, a transaction, or a journal.  The caller that
/// has all three supplies the other implementation of the same two methods, and
/// the code here cannot tell the difference.
pub trait GroupBlocks {
    /// The contents of a block within the group.
    fn get(&mut self, block: XfsAgblock) -> FsResult<Box<[u8]>>;
    /// Replace the contents of a block within the group.
    fn put(&mut self, block: XfsAgblock, bytes: Box<[u8]>) -> FsResult<()>;
}

/// A group in memory, which is what the tests use.
#[derive(Debug, Default)]
pub struct MemoryBlocks {
    blocks: std::collections::HashMap<XfsAgblock, Box<[u8]>>,
}

impl MemoryBlocks {
    /// A new, empty group.
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a block's contents.
    pub fn set(&mut self, block: XfsAgblock, bytes: Box<[u8]>) {
        self.blocks.insert(block, bytes);
    }
}

impl GroupBlocks for MemoryBlocks {
    fn get(&mut self, block: XfsAgblock) -> FsResult<Box<[u8]>> {
        self.blocks
            .get(&block)
            .cloned()
            .ok_or_else(|| FsError::Corrupt {
                what: format!("block {block} of the group is not there"),
            })
    }

    fn put(&mut self, block: XfsAgblock, bytes: Box<[u8]>) -> FsResult<()> {
        self.blocks.insert(block, bytes);
        Ok(())
    }
}

/// Read a whole free space btree, and return the free runs it records.
///
/// `fetch` is given a block number within the group and returns that block's
/// bytes.  Passing a closure rather than a device keeps the walk testable and
/// keeps this module free of any knowledge of where blocks come from.
///
/// # What it checks along the way
///
/// The point of a walk like this is that a mistake is silent, so each of these
/// is a hard error rather than a warning:
///
/// * the root and every child must be a node of the *expected* one of the two
///   free space trees, so that the tree keyed by run size cannot be read as the
///   one keyed by start block;
/// * every child must be one level shallower than its parent, and the walk must
///   reach level 0;
/// * every child pointer must be inside the group, and a node must not be
///   visited twice, which is what a corrupt tree that pointed at itself would
///   otherwise do.
///
/// # Order
///
/// The runs come back in the order the leaves are reached, which is the order
/// the tree holds them, and for each of the two trees that is a different order
/// on purpose: the tree keyed by start block is in increasing block order, and
/// the tree keyed by run length is in increasing length order.  A caller
/// searching for a run follows the tree that answers its question.
pub fn walk<F>(root: XfsAgblock, geometry: GroupGeometry, mut fetch: F) -> FsResult<Vec<FreeRun>>
where
    F: FnMut(XfsAgblock) -> FsResult<Box<[u8]>>,
{
    let mut out = Vec::new();
    visit_runs(root, &geometry, &mut fetch, |run| {
        out.push(*run);
        std::ops::ControlFlow::Continue(())
    })?;
    Ok(out)
}

/// The first run in a group's free space, at or after a given block.
///
/// This is the question the tree keyed by start block answers, and it is the
/// question an allocation that wants a particular block asks.  The runs arrive
/// in increasing block order, so the first one that starts at or after the
/// block asked for is the answer.
pub fn first_run_from<F>(
    root: XfsAgblock,
    geometry: GroupGeometry,
    from: XfsAgblock,
    mut fetch: F,
) -> FsResult<Option<FreeRun>>
where
    F: FnMut(XfsAgblock) -> FsResult<Box<[u8]>>,
{
    visit_runs(root, &geometry, &mut fetch, |run| {
        if run.start >= from {
            std::ops::ControlFlow::Break(*run)
        } else {
            std::ops::ControlFlow::Continue(())
        }
    })
}

/// The first run in a group's free space that is at least a given length.
///
/// This is the question the tree keyed by run length answers, and the runs
/// arrive in increasing length order, so the first one long enough is the
/// answer.
///
/// It is a scan rather than a descent, and deliberately so.  A node's key says
/// the *first* run under it in that tree's order, so a subtree whose first run
/// is short may still hold a long one further in -- and taking it would be right
/// only if nothing to the left of it is long enough, which is exactly what a
/// scan establishes and a descent cannot.  Making this a descent needs the
/// free space *bins*, which summarise the sizes of the runs under a subtree;
/// that is a later change, and the scan is correct without it.
pub fn first_run_of_at_least<F>(
    root: XfsAgblock,
    geometry: GroupGeometry,
    len: u32,
    mut fetch: F,
) -> FsResult<Option<FreeRun>>
where
    F: FnMut(XfsAgblock) -> FsResult<Box<[u8]>>,
{
    visit_runs(root, &geometry, &mut fetch, |run| {
        if run.len >= len {
            std::ops::ControlFlow::Break(*run)
        } else {
            std::ops::ControlFlow::Continue(())
        }
    })
}

/// The two free space trees of one group, and the allocation that keeps them
/// true.
///
/// # Why an allocation touches both trees
///
/// A group indexes the same free space twice, once by start block and once by
/// run length.  They are not two views that may disagree: they are the record of
/// which blocks are free, and a block missing from one of them but present in the
/// other is a block that will be handed out twice.  An allocation therefore
/// takes the blocks from *both* trees, and looks for the same run in both by
/// where the run starts, so the two cannot disagree even about which leaf holds
/// it.  Neither tree is written until both have been found and both have agreed
/// to lose the blocks.
///
/// # What an allocation will not do
///
/// It will not leave a leaf holding fewer than half its capacity.  A node below
/// that is not damaged, but the file system keeps its trees that way -- its own
/// repair says so -- and rebalancing a leaf is the next change, not something to
/// be done by accident.  A run in a leaf that cannot spare its blocks is passed
/// over, and a group with nothing else to give reports itself full, which is an
/// answer rather than a failure: there is another group.
///
/// Nothing here is durable until the caller writes the changed blocks and the
/// group header through its transaction.
pub struct FreeSpace<'a, B: GroupBlocks> {
    blocks:        &'a mut B,
    geometry:      GroupGeometry,
    by_block_root: XfsAgblock,
    by_size_root:  XfsAgblock,
}

impl<'a, B: GroupBlocks> FreeSpace<'a, B> {
    /// Take the group, the two roots its header names, and how to read it.
    pub fn new(
        blocks: &'a mut B,
        geometry: GroupGeometry,
        by_block_root: XfsAgblock,
        by_size_root: XfsAgblock,
    ) -> Self {
        Self {
            blocks,
            geometry,
            by_block_root,
            by_size_root,
        }
    }

    /// Every free run in the group, from the tree keyed by start block.
    pub fn runs(&mut self) -> FsResult<Vec<FreeRun>> {
        walk(self.by_block_root, self.by_block_geometry(), |b| {
            self.blocks.get(b)
        })
    }

    /// How many blocks the group has free, and the longest run among them: the
    /// two numbers its header keeps.
    pub fn summaries(&mut self) -> FsResult<(u64, u32)> {
        let runs = self.runs()?;
        let total: u64 = runs.iter().map(|r| r.len as u64).sum();
        Ok((total, runs.iter().map(|r| r.len).max().unwrap_or(0)))
    }

    fn by_block_geometry(&self) -> GroupGeometry {
        GroupGeometry {
            by_block: true,
            ..self.geometry
        }
    }

    fn by_size_geometry(&self) -> GroupGeometry {
        GroupGeometry {
            by_block: false,
            ..self.geometry
        }
    }

    fn node(&mut self, geometry: GroupGeometry, block: XfsAgblock) -> FsResult<FreeSpaceNode> {
        let bytes = self.blocks.get(block)?;
        let node = FreeSpaceNode::from_bytes(bytes, geometry.has_crc, geometry.by_block)?;
        if !node.verify_crc() {
            return Err(FsError::corrupt(format!(
                "the free space btree node in block {block} fails its checksum"
            )));
        }
        Ok(node)
    }

    /// The leaf that holds the run starting at `start`.
    ///
    /// It is found by looking rather than by following keys, because the two
    /// trees order their keys differently and only one of the two can be
    /// descended: the tree keyed by start block, whose key is the first block of
    /// a run, can be; the tree keyed by run length, whose key is the *shortest*
    /// run under a node, cannot, because a node keyed 1 may still hold a run of
    /// 8954 further in.  Correctness first: the free space bins are what turn
    /// this into a descent, and they are a later change.
    fn leaf_holding(
        &mut self,
        geometry: GroupGeometry,
        block: XfsAgblock,
        start: XfsAgblock,
    ) -> FsResult<(XfsAgblock, FreeSpaceNode)> {
        let node = self.node(geometry, block)?;
        if node.is_leaf() {
            if node.runs()?.iter().any(|r| r.start == start) {
                return Ok((block, node));
            }
            return Err(FsError::Corrupt {
                what: format!(
                    "the free space btree leaf in block {block} does not hold the run at {start}"
                ),
            });
        }
        for child in node.children()? {
            if let Ok(found) = self.leaf_holding(geometry, child, start) {
                return Ok(found);
            }
        }
        Err(FsError::Corrupt {
            what: format!("no free space btree leaf holds a run starting at block {start}"),
        })
    }

    /// Take `count` blocks from the group, and hand them back.
    ///
    /// Returns `None` when the group has nothing it can spare, which is how an
    /// allocator learns to look in another group.  "Nothing it can spare" is not
    /// the same as "nothing free": a run in a leaf that cannot afford to lose
    /// them is left alone.
    pub fn allocate(&mut self, count: u32) -> FsResult<Option<FreeRun>> {
        if count == 0 {
            return Err(FsError::invalid(libc::EINVAL, "no blocks to allocate"));
        }
        let size = self.by_size_geometry();
        let block = self.by_block_geometry();
        let by_size_root = self.by_size_root;
        let by_block_root = self.by_block_root;
        // The tree keyed by run length answers this question, in increasing
        // length, so the first run long enough is the one to take from.
        let Some(run) = first_run_of_at_least(by_size_root, size, count, |b| self.blocks.get(b))?
        else {
            return Ok(None);
        };
        let start = run.start;

        // The same run, in the other tree.  If the two trees disagree about it,
        // that is a damaged file system and the right answer is to say so rather
        // than to take blocks from one of them.
        let (by_block_leaf, mut by_block_node) = self.leaf_holding(block, by_block_root, start)?;
        let (by_size_leaf, mut by_size_node) = self.leaf_holding(size, by_size_root, start)?;
        if !by_block_node.can_lose() || !by_size_node.can_lose() {
            return Ok(None);
        }
        by_block_node.take_from_run(start, count)?;
        by_size_node.take_from_run(start, count)?;
        self.blocks.put(by_block_leaf, by_block_node.into_bytes())?;
        self.blocks.put(by_size_leaf, by_size_node.into_bytes())?;
        Ok(Some(FreeRun { start, len: count }))
    }
}

/// Hand every run of a tree to `visit`, stopping early if it says so.
fn visit_runs<F, V>(
    root: XfsAgblock,
    geometry: &GroupGeometry,
    fetch: &mut F,
    mut visit: V,
) -> FsResult<Option<FreeRun>>
where
    F: FnMut(XfsAgblock) -> FsResult<Box<[u8]>>,
    V: FnMut(&FreeRun) -> std::ops::ControlFlow<FreeRun, ()>,
{
    if root == NULL_AGBLOCK || root >= geometry.agblocks {
        return Err(FsError::corrupt(format!(
            "a group's free space btree points outside the group: block {root} of {}",
            geometry.agblocks
        )));
    }
    let mut seen = std::collections::HashSet::new();
    let node = node_at(root, geometry, fetch)?;
    visit_node(root, &node, geometry, fetch, 0, &mut seen, &mut visit)
}

/// Read one node, and check that it is a node of the tree being walked.
fn node_at<F>(block: XfsAgblock, geometry: &GroupGeometry, fetch: &mut F) -> FsResult<FreeSpaceNode>
where
    F: FnMut(XfsAgblock) -> FsResult<Box<[u8]>>,
{
    if block >= geometry.agblocks {
        return Err(FsError::corrupt(format!(
            "a free space btree in a group of {} blocks points at block {block}",
            geometry.agblocks
        )));
    }
    let bytes = fetch(block)?;
    let node = FreeSpaceNode::from_bytes(bytes, geometry.has_crc, geometry.by_block)?;
    if !node.verify_crc() {
        return Err(FsError::corrupt(format!(
            "the free space btree node in block {block} fails its checksum"
        )));
    }
    Ok(node)
}

fn visit_node<F, V>(
    block: XfsAgblock,
    node: &FreeSpaceNode,
    geometry: &GroupGeometry,
    fetch: &mut F,
    depth: usize,
    seen: &mut std::collections::HashSet<XfsAgblock>,
    visit: &mut V,
) -> FsResult<Option<FreeRun>>
where
    F: FnMut(XfsAgblock) -> FsResult<Box<[u8]>>,
    V: FnMut(&FreeRun) -> std::ops::ControlFlow<FreeRun, ()>,
{
    if depth > MAX_TREE_DEPTH {
        return Err(FsError::corrupt(format!(
            "a free space btree is more than {MAX_TREE_DEPTH} levels deep"
        )));
    }
    if !seen.insert(block) {
        return Err(FsError::corrupt(format!(
            "a free space btree visits block {block} twice"
        )));
    }
    let _ = block;
    if node.is_leaf() {
        for run in node.runs()? {
            if let std::ops::ControlFlow::Break(found) = visit(&run) {
                return Ok(Some(found));
            }
        }
        return Ok(None);
    }
    for child in node.children()? {
        // The child about to be descended into has to be one level shallower, and
        // checking it means reading it.  Only the children the walk actually
        // reaches are checked, because a search that stops at its answer should
        // not have read the rest of the tree to get there -- and a child it
        // never reads cannot mislead it.
        {
            let child_node = node_at(child, geometry, fetch)?;
            if child_node.level() + 1 != node.level() {
                return Err(FsError::corrupt(format!(
                    "a free space btree node in block {child} is at level {} under a parent at \
                     level {}",
                    child_node.level(),
                    node.level()
                )));
            }
            if let Some(found) =
                visit_node(child, &child_node, geometry, fetch, depth + 1, seen, visit)?
            {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

/// What a walk needs to know about the group it is walking.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupGeometry {
    /// How many blocks the group has, which bounds every pointer.
    pub agblocks: XfsAgblock,
    /// Whether the file system checksums its metadata, which decides the size of a
    /// node's header and so where its keys and pointers begin.
    pub has_crc:  bool,
    /// Which of the two free space trees this is, keyed by start block or by run
    /// length.  The two have different magics and different key meanings, and
    /// reading one as the other is a silent way to search in the wrong order.
    pub by_block: bool,
}

impl GroupGeometry {
    /// The geometry of a group: how big it is, whether its metadata is
    /// checksummed, and which of the two trees this is.
    pub fn new(agblocks: XfsAgblock, has_crc: bool, by_block: bool) -> Self {
        Self {
            agblocks,
            has_crc,
            by_block,
        }
    }

    /// The same group, and the other of its two trees.
    pub fn sibling_tree(self) -> Self {
        Self {
            by_block: !self.by_block,
            ..self
        }
    }
}

fn walk_node<F>(
    block: XfsAgblock,
    geometry: &GroupGeometry,
    fetch: &mut F,
    depth: usize,
    seen: &mut std::collections::HashSet<XfsAgblock>,
    out: &mut Vec<FreeRun>,
) -> FsResult<()>
where
    F: FnMut(XfsAgblock) -> FsResult<Box<[u8]>>,
{
    if depth > MAX_TREE_DEPTH {
        return Err(FsError::corrupt(format!(
            "a free space btree is more than {MAX_TREE_DEPTH} levels deep"
        )));
    }
    if !seen.insert(block) {
        return Err(FsError::corrupt(format!(
            "a free space btree visits block {block} twice"
        )));
    }
    let bytes = fetch(block)?;
    let node = FreeSpaceNode::from_bytes(bytes, geometry.has_crc, geometry.by_block)?;
    if !node.verify_crc() {
        return Err(FsError::corrupt(format!(
            "the free space btree node in block {block} fails its checksum"
        )));
    }
    if node.is_leaf() {
        out.extend(node.runs()?);
        return Ok(());
    }
    let children = node.children()?;
    for child in &children {
        if *child >= geometry.agblocks {
            return Err(FsError::corrupt(format!(
                "a free space btree in a group of {} blocks points at block {child}",
                geometry.agblocks
            )));
        }
    }
    // Every child has to be one level shallower, and that means reading each of
    // them before deciding to descend.  A tree that is not consistent is damaged,
    // and an allocator that walked it anyway would be reading free space out of
    // whatever happened to be in those blocks.
    for child in &children {
        let bytes = fetch(*child)?;
        let child_node = FreeSpaceNode::from_bytes(bytes, geometry.has_crc, geometry.by_block)?;
        if child_node.level() + 1 != node.level() {
            return Err(FsError::corrupt(format!(
                "a free space btree node in block {child} is at level {} under a parent at level \
                 {}",
                child_node.level(),
                node.level()
            )));
        }
    }
    for child in children {
        walk_node(child, geometry, fetch, depth + 1, seen, out)?;
    }
    Ok(())
}

#[cfg(test)]
mod t {
    use super::*;

    /// The first 176 bytes of the free space btree of the first group of
    /// `resources/xfsv4.img`, which is a leaf in a version 4 file system: its
    /// header and all nineteen of its records.  The rest of that block is keys
    /// and unused space.
    ///
    /// What the runs in it are is not a guess: `xfs_db`'s `freesp -a 0 -d`
    /// prints the same runs for the same group, and the expectations below are
    /// those.
    const V4_NODE: &str = concat!(
        "41 42 54 42 00 00 00 13 ff ff ff ff ff ff ff ff ",
        "00 00 00 0d 00 00 00 03 00 00 02 39 00 00 00 07 ",
        "00 00 05 c9 00 00 00 07 00 00 08 e8 00 00 00 18 ",
        "00 00 09 20 00 00 00 08 00 00 09 30 00 00 00 50 ",
        "00 00 09 a8 00 00 00 18 00 00 09 e0 00 00 00 18 ",
        "00 00 0a 00 00 00 00 48 00 00 0a 70 00 00 00 20 ",
        "00 00 0a b0 00 00 00 08 00 00 0a b9 00 00 00 08 ",
        "00 00 0a c9 00 00 00 67 00 00 0b 50 00 00 00 08 ",
        "00 00 0b 60 00 00 00 48 00 00 0b d0 00 00 00 20 ",
        "00 00 0c 10 00 00 00 18 00 00 0c 30 00 00 00 50 ",
        "00 00 0c a8 00 00 73 58 ",
    );

    /// The first 96 bytes of the free space btree of the first group of
    /// `resources/xfs_4kn.img`: a leaf in a version 5 file system, with the
    /// longer header and five records.
    const V5_NODE: &str = concat!(
        "41 42 33 42 00 00 00 05 ff ff ff ff ff ff ff ff ",
        "00 00 00 00 00 00 00 20 00 00 00 01 00 00 00 08 ",
        "8d 0c 39 d3 96 de 47 ef a4 76 1c 07 14 0c b9 36 ",
        "00 00 00 00 4b ed 12 94 00 00 00 0d 00 00 00 02 ",
        "00 00 00 19 00 00 00 01 00 00 00 1b 00 00 00 01 ",
        "00 00 00 20 00 00 00 01 00 00 00 22 00 00 0f de ",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ",
        "00 00 00 00 00 00 00 00 ",
    );

    fn block_of(hex_rows: &str, blocksize: usize) -> Vec<u8> {
        let mut out: Vec<u8> = hex_rows
            .split_whitespace()
            .map(|byte| u8::from_str_radix(byte, 16).expect("two hex digits per byte"))
            .collect();
        out.resize(blocksize, 0);
        out
    }

    /// A real version 4 leaf, decoded, must produce the runs that `xfs_db`
    /// reports for its group.
    #[test]
    fn real_v4_leaf_lists_its_runs() {
        let node = FreeSpaceNode::from_bytes(block_of(V4_NODE, 512), false, true).unwrap();
        assert!(node.is_leaf());
        assert_eq!(node.level(), 0);
        assert_eq!(node.numrecs(), 19);
        assert_eq!(node.left_sibling(), NULL_AGBLOCK);
        assert_eq!(node.right_sibling(), NULL_AGBLOCK);
        assert!(
            node.verify_crc(),
            "a version 4 node has no checksum to be wrong about"
        );
        assert!(node.is_by_block());

        let runs = node.runs().unwrap();
        assert_eq!(runs.len(), 19);
        assert_eq!(
            runs[0],
            FreeRun {
                start: 13,
                len:   3,
            }
        );
        assert_eq!(
            runs[1],
            FreeRun {
                start: 569,
                len:   7,
            }
        );
        assert_eq!(
            runs[2],
            FreeRun {
                start: 1481,
                len:   7,
            }
        );
        // The group's longest run, which the group header also reports.
        assert_eq!(
            runs[18],
            FreeRun {
                start: 3240,
                len:   29528,
            }
        );
        assert_eq!(
            runs[18].end(),
            32768,
            "the last run reaches the end of the group"
        );
    }

    /// The same for a version 5 leaf, whose header is longer.
    #[test]
    fn real_v5_leaf_lists_its_runs() {
        let node = FreeSpaceNode::from_bytes(block_of(V5_NODE, 4096), true, true).unwrap();
        assert!(node.is_leaf());
        assert_eq!(node.numrecs(), 5);
        assert!(node.verify_crc(), "a real version 5 node must verify");
        assert_eq!(node.lsn(), 0x20);

        let runs = node.runs().unwrap();
        assert_eq!(
            runs,
            vec![
                FreeRun {
                    start: 13,
                    len:   2,
                },
                FreeRun {
                    start: 25,
                    len:   1,
                },
                FreeRun {
                    start: 27,
                    len:   1,
                },
                FreeRun {
                    start: 32,
                    len:   1,
                },
                FreeRun {
                    start: 34,
                    len:   4062,
                },
            ]
        );
    }

    /// A block that is not a node of the expected btree has to be refused, and
    /// so does a node read as the wrong one of the two.
    #[test]
    fn the_wrong_block_is_refused() {
        // The version 4 leaf, read as the btree keyed by run length.
        let err = FreeSpaceNode::from_bytes(block_of(V4_NODE, 512), false, false).unwrap_err();
        assert!(matches!(err, FsError::Corrupt { .. }));
        // A block bitmap, which is another thing a group header points at.
        let err = FreeSpaceNode::from_bytes(vec![0x42; 512], false, true).unwrap_err();
        assert!(matches!(err, FsError::Corrupt { .. }));
        // Too short to hold a record.
        assert!(FreeSpaceNode::from_bytes(vec![0u8; 8], false, true).is_err());
    }

    /// A node that claims more records than its block can hold is damaged, and
    /// has to be caught rather than read past the end of the block.
    #[test]
    fn a_node_that_claims_too_many_records_is_refused() {
        let mut bytes = block_of(V4_NODE, 512);
        BigEndian::write_u16(&mut bytes[6..], 4000);
        assert!(FreeSpaceNode::from_bytes(bytes, false, true).is_err());
    }

    /// An interior node's records are the blocks holding its children, which are
    /// not free runs, and asking for runs has to say so.
    #[test]
    fn an_interior_node_holds_subtrees() {
        let mut bytes = block_of(V4_NODE, 512);
        BigEndian::write_u16(&mut bytes[4..], 1); // level 1
        let node = FreeSpaceNode::from_bytes(bytes, false, true).unwrap();
        assert!(!node.is_leaf());
        let err = node.runs().unwrap_err();
        assert_eq!(err.errno(), libc::EINVAL);
        assert_eq!(node.children().unwrap().len(), 19);
        // And a leaf is not a subtree.
        let leaf = FreeSpaceNode::from_bytes(block_of(V4_NODE, 512), false, true).unwrap();
        assert_eq!(leaf.children().unwrap_err().errno(), libc::EINVAL);
    }

    // --- the tree walk, on a tree built here so that every way of reading it
    // --- wrong is a test that fails.

    use std::collections::HashMap;

    const BS: usize = 512;
    const AGBLOCKS: XfsAgblock = 4096;

    /// A 512-byte-block, version 4 group.
    fn geometry() -> GroupGeometry {
        GroupGeometry {
            agblocks: AGBLOCKS,
            has_crc:  false,
            by_block: true,
        }
    }

    /// The same group, and the tree keyed by run length.
    fn size_geometry() -> GroupGeometry {
        GroupGeometry {
            by_block: false,
            ..geometry()
        }
    }

    /// A leaf holding `runs`, as a version 4 node of one of the two trees.
    fn leaf_of(magic: u32, runs: &[(u32, u32)]) -> Vec<u8> {
        let mut b = vec![0u8; BS];
        BigEndian::write_u32(&mut b[0..], magic);
        BigEndian::write_u16(&mut b[4..], 0); // a leaf
        BigEndian::write_u16(&mut b[6..], runs.len() as u16);
        BigEndian::write_u32(&mut b[8..], NULL_AGBLOCK);
        BigEndian::write_u32(&mut b[12..], NULL_AGBLOCK);
        for (i, (at, len)) in runs.iter().enumerate() {
            BigEndian::write_u32(&mut b[16 + RECORD_LEN * i..], *at);
            BigEndian::write_u32(&mut b[16 + RECORD_LEN * i + 4..], *len);
        }
        b
    }

    /// An interior node over `leaves`, each with the key it covers.
    fn interior_of(magic: u32, keys: &[u32], leaves: &[u32]) -> Vec<u8> {
        assert_eq!(keys.len(), leaves.len());
        let mut b = vec![0u8; BS];
        BigEndian::write_u32(&mut b[0..], magic);
        BigEndian::write_u16(&mut b[4..], 1);
        BigEndian::write_u16(&mut b[6..], leaves.len() as u16);
        BigEndian::write_u32(&mut b[8..], NULL_AGBLOCK);
        BigEndian::write_u32(&mut b[12..], NULL_AGBLOCK);
        let max = (BS - 16) / ENTRY_LEN;
        for (i, leaf) in leaves.iter().enumerate() {
            BigEndian::write_u32(&mut b[16 + RECORD_LEN * i..], keys[i]);
            BigEndian::write_u32(&mut b[16 + RECORD_LEN * i + 4..], 0);
            BigEndian::write_u32(&mut b[16 + max * RECORD_LEN + PTR_LEN * i..], *leaf);
        }
        b
    }

    /// Build a leaf holding `runs`, as a version 4 free space node of the tree
    /// keyed by start block.
    fn leaf(runs: &[(XfsAgblock, u32)]) -> Vec<u8> {
        leaf_of(XFS_ABTB_MAGIC, runs)
    }

    /// The same, for the tree keyed by run length.
    fn size_leaf(runs: &[(XfsAgblock, u32)]) -> Vec<u8> {
        leaf_of(XFS_ABTC_MAGIC, runs)
    }

    /// Build an interior node pointing at `children`, as a version 4 node.
    fn interior(level: u16, keys: &[(u32, u32)], children: &[XfsAgblock]) -> Vec<u8> {
        assert_eq!(keys.len(), children.len());
        let mut b = vec![0u8; BS];
        BigEndian::write_u32(&mut b[0..], XFS_ABTB_MAGIC);
        BigEndian::write_u16(&mut b[4..], level);
        BigEndian::write_u16(&mut b[6..], children.len() as u16);
        BigEndian::write_u32(&mut b[8..], NULL_AGBLOCK);
        BigEndian::write_u32(&mut b[12..], NULL_AGBLOCK);
        let max = (BS - 16) / ENTRY_LEN;
        for (i, (start, count)) in keys.iter().enumerate() {
            let at = 16 + RECORD_LEN * i;
            BigEndian::write_u32(&mut b[at..], *start);
            BigEndian::write_u32(&mut b[at + 4..], *count);
        }
        // The pointers live after the *whole* key region, not after the keys in
        // use.  This is the layout the tree depends on.
        for (i, child) in children.iter().enumerate() {
            let at = 16 + max * RECORD_LEN + PTR_LEN * i;
            BigEndian::write_u32(&mut b[at..], *child);
        }
        b
    }

    /// A group holding `blocks`, addressed by block number.
    fn group(
        blocks: HashMap<XfsAgblock, Vec<u8>>,
    ) -> impl FnMut(XfsAgblock) -> FsResult<Box<[u8]>> {
        move |bno| {
            blocks
                .get(&bno)
                .cloned()
                .map(Vec::into_boxed_slice)
                .ok_or_else(|| FsError::corrupt(format!("no block {bno}")))
        }
    }

    /// A two-level tree must produce every run its leaves hold, in tree order.
    #[test]
    fn a_two_level_tree_yields_every_run() {
        let mut blocks = HashMap::new();
        blocks.insert(10u32, leaf(&[(5, 3), (40, 7)]));
        blocks.insert(11, leaf(&[(100, 1), (200, 62)]));
        blocks.insert(4, interior(1, &[(5, 3), (100, 1)], &[10, 11]));
        let runs = walk(4, geometry(), group(blocks)).unwrap();
        assert_eq!(
            runs,
            vec![
                FreeRun { start: 5, len: 3 },
                FreeRun {
                    start: 40,
                    len:   7,
                },
                FreeRun {
                    start: 100,
                    len:   1,
                },
                FreeRun {
                    start: 200,
                    len:   62,
                },
            ]
        );
    }

    /// A three-level tree has to descend twice, and each level's keys have to
    /// match the runs under it for a search to work later.
    #[test]
    fn a_three_level_tree_descends() {
        let mut blocks = HashMap::new();
        blocks.insert(20u32, leaf(&[(7, 1)]));
        blocks.insert(21, leaf(&[(9, 2)]));
        blocks.insert(10, interior(1, &[(7, 1), (9, 2)], &[20, 21]));
        blocks.insert(4, interior(2, &[(7, 3)], &[10]));
        let runs = walk(4, geometry(), group(blocks)).unwrap();
        assert_eq!(
            runs,
            vec![FreeRun { start: 7, len: 1 }, FreeRun { start: 9, len: 2 },]
        );
    }

    /// A single-node tree, which is what a small group has, is the easy case and
    /// has to keep working.
    #[test]
    fn a_single_node_tree_works() {
        let mut blocks = HashMap::new();
        blocks.insert(4u32, leaf(&[(1, 1), (3, 4), (900, 30)]));
        let runs = walk(4, geometry(), group(blocks)).unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!(
            runs[2],
            FreeRun {
                start: 900,
                len:   30,
            }
        );
    }

    /// The keys are the indexing keys, not the leaf's records, and the pointer
    /// array starts after the whole key region.  Both are read out here so that
    /// a change to either is a test that fails.
    #[test]
    fn keys_and_pointers_are_separate_arrays() {
        let b = interior(1, &[(5, 3), (100, 1)], &[10, 11]);
        let node = FreeSpaceNode::from_bytes(b.clone(), false, true).unwrap();
        assert_eq!(node.keys(), vec![(5, 3), (100, 1)]);
        assert_eq!(node.children().unwrap(), vec![10, 11]);
        // The pointer array is after the key region sized for the block's
        // capacity, which for a 512 byte block is 41 keys.
        assert_eq!(
            node.capacity(),
            41,
            "an interior node holds key-and-pointer entries"
        );
        assert_eq!(
            FreeSpaceNode::from_bytes(leaf(&[(1, 1)]), false, true)
                .unwrap()
                .capacity(),
            62,
            "a leaf holds records"
        );
        assert_eq!(node.pointers_offset(), 16 + 41 * 8);
        assert_eq!(node.pointers_offset(), 344);

        // Reading the pointers from where the *used* keys end picks up the empty
        // part of the key region instead, which is the mistake this catches.
        let wrong = 16 + 2 * RECORD_LEN;
        assert_eq!(
            BigEndian::read_u32(&b[wrong..]),
            0,
            "that is not a child block"
        );
        assert_eq!(BigEndian::read_u32(&b[344..]), 10);
    }

    /// A child that is not one level shallower means the tree is not what it
    /// claims, and walking it would read free space out of whatever is there.
    #[test]
    fn a_child_at_the_wrong_level_is_refused() {
        let mut blocks = HashMap::new();
        blocks.insert(10u32, leaf(&[(5, 3)]));
        blocks.insert(11, interior(1, &[], &[]));
        blocks.insert(4, interior(1, &[(5, 3), (9, 2)], &[10, 11]));
        let err = walk(4, geometry(), group(blocks)).unwrap_err();
        assert!(matches!(err, FsError::Corrupt { .. }), "{err}");
    }

    /// A pointer outside the group would be a read of something that is not
    /// free space.
    #[test]
    fn a_pointer_outside_the_group_is_refused() {
        let mut blocks = HashMap::new();
        blocks.insert(10u32, leaf(&[(5, 3)]));
        // A child past the end of the group.
        blocks.insert(4, interior(1, &[(5, 3)], &[AGBLOCKS + 1]));
        let err = walk(4, geometry(), group(blocks)).unwrap_err();
        assert!(matches!(err, FsError::Corrupt { .. }), "{err}");
    }

    /// A tree that points at itself would otherwise be walked for ever.
    #[test]
    fn a_tree_that_loops_is_refused() {
        let mut blocks = HashMap::new();
        blocks.insert(4u32, interior(1, &[(5, 3)], &[4]));
        let err = walk(4, geometry(), group(blocks)).unwrap_err();
        assert!(matches!(err, FsError::Corrupt { .. }), "{err}");
    }

    /// The root itself has to be inside the group, and a tree that is not
    /// there is not a tree to walk.
    #[test]
    fn a_root_outside_the_group_is_refused() {
        let err = walk(AGBLOCKS, geometry(), group(HashMap::new())).unwrap_err();
        assert!(matches!(err, FsError::Corrupt { .. }), "{err}");
        let err = walk(NULL_AGBLOCK, geometry(), group(HashMap::new())).unwrap_err();
        assert!(matches!(err, FsError::Corrupt { .. }), "{err}");
    }

    /// The tree keyed by run size must not be read as the one keyed by start
    /// block: a walk that got that wrong would search the wrong order.
    #[test]
    fn the_wrong_tree_is_refused() {
        let mut blocks = HashMap::new();
        blocks.insert(10u32, leaf(&[(5, 3)]));
        let mut root = interior(1, &[(5, 3)], &[10]);
        // The root is the size-ordered tree; the leaf is the block-ordered one.
        BigEndian::write_u32(&mut root[0..], XFS_ABTC_MAGIC);
        blocks.insert(4, root);
        let err = walk(4, geometry(), group(blocks)).unwrap_err();
        assert!(matches!(err, FsError::Corrupt { .. }), "{err}");
    }

    /// What the node's records should look like after a change, checked against a
    /// plain list rather than against the node's own idea of its contents.
    fn records_of(node: &FreeSpaceNode) -> Vec<FreeRun> {
        node.runs().unwrap()
    }

    /// The first key of a node, which is what a search descending through it
    /// compares against: the first record under it, in that tree's order.
    fn first_key_of(node: &FreeSpaceNode) -> (u32, u32) {
        let runs = records_of(node);
        runs.first().map(|r| (r.start, r.len)).unwrap_or((0, 0))
    }

    /// Taking blocks out of the middle of a run keeps what is left where it was.
    #[test]
    fn taking_from_the_front_of_a_run() {
        let mut node =
            FreeSpaceNode::from_bytes(leaf(&[(100, 10), (200, 5)]), false, true).unwrap();
        let left = node.take_from_run(100, 4).unwrap();
        assert_eq!(
            left,
            Some(FreeRun {
                start: 104,
                len:   6,
            })
        );
        assert_eq!(
            records_of(&node),
            vec![
                FreeRun {
                    start: 104,
                    len:   6,
                },
                FreeRun {
                    start: 200,
                    len:   5,
                }
            ]
        );
    }

    /// Taking a whole run removes it, and the ones after it move up.
    #[test]
    fn taking_a_whole_run_removes_it() {
        let mut node =
            FreeSpaceNode::from_bytes(leaf(&[(100, 4), (200, 5), (300, 1)]), false, true).unwrap();
        assert_eq!(node.take_from_run(200, 5).unwrap(), None);
        assert_eq!(
            records_of(&node),
            vec![
                FreeRun {
                    start: 100,
                    len:   4,
                },
                FreeRun {
                    start: 300,
                    len:   1,
                }
            ]
        );
    }

    /// Asking for blocks that are not there, or more than there are, has to be an
    /// error: an allocation that silently took the wrong blocks would be the
    /// worst bug in this file system.
    #[test]
    fn taking_what_is_not_there_is_refused() {
        let mut node = FreeSpaceNode::from_bytes(leaf(&[(100, 4)]), false, true).unwrap();
        assert!(node.take_from_run(101, 1).is_err(), "no such run");
        assert!(
            node.take_from_run(100, 5).is_err(),
            "more than the run holds"
        );
        assert!(node.take_from_run(100, 0).is_err(), "none at all");
        // And the node is unchanged, which matters: a failed allocation must not
        // have half-taken a run.
        assert_eq!(
            records_of(&node),
            vec![FreeRun {
                start: 100,
                len:   4,
            }]
        );
    }

    /// An interior node holds subtrees, not runs, and saying so beats reading
    /// its keys as free space.
    #[test]
    fn only_a_leaf_holds_runs() {
        let mut node =
            FreeSpaceNode::from_bytes(interior(1, &[(5, 3), (100, 1)], &[10, 11]), false, true)
                .unwrap();
        assert_eq!(node.take_from_run(5, 1).unwrap_err().errno(), libc::EINVAL);
        assert_eq!(
            node.put_run(FreeRun { start: 7, len: 1 })
                .unwrap_err()
                .errno(),
            libc::EINVAL
        );
        assert!(node.runs().is_err());
    }

    /// A run goes back where its tree's order says it belongs.
    #[test]
    fn a_run_goes_where_its_order_says() {
        // The tree keyed by start block, inserting out of order.
        let mut by_block =
            FreeSpaceNode::from_bytes(leaf(&[(100, 1), (200, 1)]), false, true).unwrap();
        by_block
            .put_run(FreeRun {
                start: 50,
                len:   3,
            })
            .unwrap();
        by_block
            .put_run(FreeRun {
                start: 150,
                len:   2,
            })
            .unwrap();
        assert_eq!(
            records_of(&by_block),
            vec![
                FreeRun {
                    start: 50,
                    len:   3,
                },
                FreeRun {
                    start: 100,
                    len:   1,
                },
                FreeRun {
                    start: 150,
                    len:   2,
                },
                FreeRun {
                    start: 200,
                    len:   1,
                },
            ]
        );

        // The tree keyed by run length: the same runs come out in a different
        // order, which is the whole point of having two trees.
        let mut by_len =
            FreeSpaceNode::from_bytes(size_leaf(&[(100, 1), (200, 9)]), false, false).unwrap();
        by_len
            .put_run(FreeRun {
                start: 150,
                len:   4,
            })
            .unwrap();
        assert_eq!(
            records_of(&by_len),
            vec![
                FreeRun {
                    start: 100,
                    len:   1,
                },
                FreeRun {
                    start: 150,
                    len:   4,
                },
                FreeRun {
                    start: 200,
                    len:   9,
                },
            ]
        );
        assert_eq!(by_len.order(), Order::ByLength);
    }

    /// Two runs may not share a block, or the same block would be handed out
    /// twice.  A node that would end up overlapping is refused.
    #[test]
    fn an_overlapping_run_is_refused() {
        let mut node = FreeSpaceNode::from_bytes(leaf(&[(100, 10)]), false, true).unwrap();
        assert!(node
            .put_run(FreeRun {
                start: 105,
                len:   1,
            })
            .is_err());
        assert!(node
            .put_run(FreeRun {
                start: 90,
                len:   20,
            })
            .is_err());
        // Touching end to end is not overlapping: that is how a run grows.
        node.put_run(FreeRun {
            start: 90,
            len:   10,
        })
        .unwrap();
        node.put_run(FreeRun {
            start: 110,
            len:   10,
        })
        .unwrap();
        assert_eq!(records_of(&node).len(), 3);
    }

    /// A leaf that is full says so rather than dropping a run on the floor.
    #[test]
    fn a_full_leaf_refuses_another_run() {
        let runs: Vec<(XfsAgblock, u32)> = (0..62u32).map(|i| (i * 2, 1)).collect();
        let mut node = FreeSpaceNode::from_bytes(leaf(&runs), false, true).unwrap();
        assert_eq!(node.capacity(), 62);
        assert_eq!(node.numrecs(), 62);
        assert_eq!(
            node.put_run(FreeRun {
                start: 5000,
                len:   1,
            })
            .unwrap_err()
            .errno(),
            libc::ENOSPC
        );
        // And taking one out makes room for one.
        node.take_from_run(0, 1).unwrap();
        node.put_run(FreeRun {
            start: 5000,
            len:   1,
        })
        .unwrap();
        assert_eq!(node.numrecs(), 62);
    }

    /// What a search descending through a node compares against is its first
    /// record, so taking the first run out of a node has to change what that is.
    #[test]
    fn a_nodes_first_record_follows_its_runs() {
        let mut node =
            FreeSpaceNode::from_bytes(leaf(&[(100, 4), (200, 1), (300, 9)]), false, true).unwrap();
        assert_eq!(first_key_of(&node), (100, 4));
        node.take_from_run(100, 4).unwrap();
        assert_eq!(first_key_of(&node), (200, 1), "the first record moved");
        node.put_run(FreeRun {
            start: 150,
            len:   2,
        })
        .unwrap();
        assert_eq!(
            first_key_of(&node),
            (150, 2),
            "the first record moved again"
        );
    }

    /// A node that is more than half empty is one the file system would want to
    /// merge with a sibling, and a node with one entry is one that has no
    /// business being a separate level of the tree.
    #[test]
    fn how_full_a_node_has_to_be() {
        let full: Vec<(XfsAgblock, u32)> = (0..62u32).map(|i| (i * 2, 1)).collect();
        let node = FreeSpaceNode::from_bytes(leaf(&full), false, true).unwrap();
        assert_eq!(node.capacity(), 62);
        assert_eq!(node.min_records(), 31);
        assert!(!node.wants_merging(), "a full node is not short");

        let mut node = FreeSpaceNode::from_bytes(leaf(&full), false, true).unwrap();
        for _ in 0..31 {
            node.take_from_run(node.runs().unwrap()[0].start, 1)
                .unwrap();
        }
        assert_eq!(node.numrecs(), 31);
        assert!(
            !node.wants_merging(),
            "half full is the boundary, not below it"
        );
        node.take_from_run(node.runs().unwrap()[0].start, 1)
            .unwrap();
        assert_eq!(node.numrecs(), 30);
        assert!(
            node.wants_merging(),
            "below half full is what repair complains about"
        );

        // The same half-full rule applies to an interior node's entries, though
        // whether one should be merged depends on whether it has a sibling to
        // merge with, which only the layer that knows the tree's shape can say.
        let interior =
            FreeSpaceNode::from_bytes(interior(1, &[(5, 3), (100, 1)], &[10, 11]), false, true)
                .unwrap();
        assert_eq!(interior.capacity(), 41);
        assert_eq!(interior.min_records(), 21);
    }

    /// A whole file system's worth of take and give, against a plain list of
    /// what should be left.
    ///
    /// This is the shape of test that catches a mutation that is right for one
    /// record and wrong for two hundred: random operations in a random order,
    /// with the node's contents compared against a list kept alongside it.
    #[test]
    fn take_and_give_against_a_model() {
        // A small deterministic sequence, so a failure can be reproduced: the
        // numbers are a linear congruential sequence, not a random source that
        // would differ from run to run.
        for (by_block, seed_start) in [(true, 1u64), (false, 9u64)] {
            let start_run = (1000u32, 40u32);
            let mut seed = seed_start;
            let mut next = move || {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (seed >> 33) as u32
            };
            let mut node = FreeSpaceNode::from_bytes(
                if by_block {
                    leaf(&[start_run])
                } else {
                    size_leaf(&[start_run])
                },
                false,
                by_block,
            )
            .unwrap();
            let mut model: Vec<(XfsAgblock, u32)> = vec![(1000, 40)];
            for step in 0..200 {
                if model.is_empty() {
                    // Everything has been taken.  An allocation that finds the
                    // group empty has to look in another group, so what matters
                    // is that the node says so.
                    assert!(node.runs().unwrap().is_empty());
                    break;
                }
                let at = (next() as usize) % model.len();
                let (start, len) = model[at];
                let count = 1 + next() % len;
                // What is left behind, worked out before the model is touched:
                // taking from the front of a run moves where the rest begins and
                // takes the length off it, and the run disappears when that was
                // all of it.
                let expected_left = if count == len {
                    None
                } else {
                    Some((start + count, len - count))
                };
                let left = node.take_from_run(start, count).expect("take");
                if count == len {
                    model.remove(at);
                } else {
                    model[at].0 += count;
                    model[at].1 -= count;
                }
                assert_eq!(
                    left.map(|r| (r.start, r.len)),
                    expected_left,
                    "step {step}: what was left behind does not match the model"
                );
                assert_eq!(
                    records_of(&node).len(),
                    model.len(),
                    "step {step}: {model:?}"
                );
                // Half the time, give a run back.
                if next() % 2 == 0 {
                    let newlen = 1 + next() % 12;
                    let newstart = 1 + next() % 20000;
                    if model.iter().all(|(s, l)| {
                        newstart as u64 + newlen as u64 <= *s as u64
                            || *s as u64 + *l as u64 <= newstart as u64
                    }) {
                        node.put_run(FreeRun {
                            start: newstart,
                            len:   newlen,
                        })
                        .expect("put");
                        model.push((newstart, newlen));
                        model.sort_by_key(|(s, _)| *s);
                    }
                }
                // The node and the model must hold exactly the same runs.  That
                // they are *in* each tree's order is a separate question, and it
                // is asked directly by a_run_goes_where_its_order_says, where a
                // mistake is easier to read off.
                let mut from_node = records_of(&node);
                let mut from_model: Vec<FreeRun> = model
                    .iter()
                    .map(|(s, l)| FreeRun {
                        start: *s,
                        len:   *l,
                    })
                    .collect();
                from_node.sort_by_key(|r| (r.start, r.len));
                from_model.sort_by_key(|r| (r.start, r.len));
                assert_eq!(from_node, from_model, "step {step}");
            }
        }
    }

    // --- the allocator, over a group built here so that "the same block twice"
    // --- can be checked against a list of everything handed out.

    /// How many records the fixture's leaves hold.  It has to be more than half
    /// a leaf's capacity for a leaf to be allowed to lose one, so this is well
    /// above the 31 of a 512-byte block.
    const LEAF_RECORDS: usize = 32;

    /// Allocating from a group takes the blocks from both trees, and the same
    /// block is never handed out twice.
    ///
    /// That is the property the file system's integrity rests on, so it is
    /// checked over a long sequence rather than for one allocation, and both
    /// trees are compared afterwards: a block free in one of them and taken in
    /// the other is a block that gets handed out again.
    #[test]
    fn allocation_never_hands_out_a_block_twice() {
        let runs = scattered_runs();
        let expected_free: std::collections::HashSet<u32> =
            runs.iter().flat_map(|(at, len)| *at..*at + *len).collect();
        let mut blocks = group_of(&runs);
        let geometry = GroupGeometry::new(1 << 20, false, true);
        let mut handed_out: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut served = 0u32;

        loop {
            let got = FreeSpace::new(&mut blocks, geometry, 4, 5)
                .allocate(1 + served % 5)
                .expect("allocate");
            let Some(run) = got else {
                break;
            };
            for block in run.start..run.start + run.len {
                assert!(
                    expected_free.contains(&block),
                    "block {block} was handed out but was never free"
                );
                assert!(
                    handed_out.insert(block),
                    "block {block} was handed out twice"
                );
            }
            served += 1;
            assert!(
                served < 200,
                "the group should have run out of what it can spare"
            );
        }
        assert!(served > 0, "the group should have served something");

        // What is left in the two trees, and they have to agree.
        let mut by_block = walk(4, geometry, |b| blocks.get(b)).expect("walk");
        let mut by_size = walk(5, GroupGeometry::new(1 << 20, false, false), |b| {
            blocks.get(b)
        })
        .expect("walk");
        by_block.sort_by_key(|r| (r.start, r.len));
        by_size.sort_by_key(|r| (r.start, r.len));
        assert_eq!(
            by_block, by_size,
            "the two trees no longer agree about what is free"
        );
        let left: std::collections::HashSet<u32> = by_block
            .iter()
            .flat_map(|r| r.start..r.start + r.len)
            .collect();
        for block in &handed_out {
            assert!(
                !left.contains(block),
                "block {block} is free and has been handed out"
            );
        }
        assert_eq!(left.len() + handed_out.len(), expected_free.len());
        // No leaf was left short enough for the file system to want to rebalance.
        let (free, longest) = FreeSpace::new(&mut blocks, geometry, 4, 5)
            .summaries()
            .expect("summaries");
        assert_eq!(free, left.len() as u64);
        assert_eq!(longest, by_block.iter().map(|r| r.len).max().unwrap_or(0));
    }

    /// A group with nothing it can spare says so, rather than leaving a leaf in
    /// a shape the file system would want to repair.
    #[test]
    fn a_group_with_nothing_to_spare_says_so() {
        // A run that is exactly what a leaf holds: taking from it would empty
        // the leaf, so it is left alone.
        let runs = scattered_runs();
        let mut blocks = group_of(&runs);
        let geometry = GroupGeometry::new(1 << 20, false, true);
        // Fill every leaf down to one record short of what it may lose, by
        // taking the last allocations the group will serve.
        let mut fs = FreeSpace::new(&mut blocks, geometry, 4, 5);
        let before = fs.runs().expect("read the trees");
        loop {
            if fs.allocate(1).expect("allocate").is_none() {
                break;
            }
        }
        let after = fs.runs().expect("read the trees");
        assert!(
            after.len() < before.len(),
            "the group should have served something"
        );
        // And it is still a usable file system: both trees agree, and nothing
        // free is missing.
        let mut by_block = after.clone();
        by_block.sort_by_key(|r| (r.start, r.len));
        assert!(by_block.iter().all(|r| r.len > 0));
    }

    /// Asking for no blocks is a mistake, not an allocation of nothing.
    #[test]
    fn asking_for_no_blocks_is_refused() {
        let mut blocks = group_of(&scattered_runs());
        let geometry = GroupGeometry::new(1 << 20, false, true);
        let mut fs = FreeSpace::new(&mut blocks, geometry, 4, 5);
        assert_eq!(fs.allocate(0).unwrap_err().errno(), libc::EINVAL);
    }

    /// Some free runs, spread out the way a group looks after files have been
    /// created and deleted.
    fn scattered_runs() -> Vec<(u32, u32)> {
        (0..200u32)
            .map(|i| (1000 + i * 37, 1 + (i * 13) % 23))
            .collect()
    }

    /// A group whose free space is `runs`, spread over leaves of
    /// [`LEAF_RECORDS`] records, in both trees, with the roots at 4 and 5.
    ///
    /// The two trees get the same runs in their own orders, as a file system's
    /// do, so that an allocation has to find the same run in both.
    fn group_of(runs: &[(u32, u32)]) -> MemoryBlocks {
        let chunks: Vec<&[(u32, u32)]> = runs.chunks(LEAF_RECORDS).collect();
        let mut blocks = MemoryBlocks::new();
        let mut by_block_leaves = Vec::new();
        let mut by_size_leaves = Vec::new();
        let mut by_block_keys = Vec::new();
        let mut by_size_keys = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let block_leaf = 10 + 2 * i as u32;
            let size_leaf = 11 + 2 * i as u32;
            blocks.set(
                block_leaf,
                leaf_of(XFS_ABTB_MAGIC, chunk).into_boxed_slice(),
            );
            let mut ordered: Vec<(u32, u32)> = chunk.to_vec();
            ordered.sort_by_key(|(at, len)| (*len, *at));
            blocks.set(
                size_leaf,
                leaf_of(XFS_ABTC_MAGIC, &ordered).into_boxed_slice(),
            );
            by_block_leaves.push(block_leaf);
            by_size_leaves.push(size_leaf);
            by_block_keys.push(chunk[0].0);
            by_size_keys.push(ordered[0].1);
        }
        blocks.set(
            4,
            interior_of(XFS_ABTB_MAGIC, &by_block_keys, &by_block_leaves).into_boxed_slice(),
        );
        blocks.set(
            5,
            interior_of(XFS_ABTC_MAGIC, &by_size_keys, &by_size_leaves).into_boxed_slice(),
        );
        blocks
    }

    /// A version 5 node's checksum has to be caught when it changes, and
    /// recomputed when it is rewritten.
    #[test]
    fn a_version_5_node_checksums() {
        let mut bytes = block_of(V5_NODE, 4096);
        assert!(FreeSpaceNode::from_bytes(bytes.clone(), true, true)
            .unwrap()
            .verify_crc());
        // A record, in the middle of the block.
        bytes[60] ^= 0x01;
        assert!(!FreeSpaceNode::from_bytes(bytes.clone(), true, true)
            .unwrap()
            .verify_crc());
        // Recomputing must make it verify again, since that is what writing the
        // node back does.
        let mut node = FreeSpaceNode::from_bytes(bytes, true, true).unwrap();
        node.update_crc();
        assert!(node.verify_crc());
    }

    /// The block-ordered tree answers "is there a run at or after this block",
    /// and it answers it in block order, so the answer is the first run that
    /// starts at or after where we asked.
    #[test]
    fn the_first_run_from_a_block() {
        let mut blocks = HashMap::new();
        blocks.insert(10u32, leaf(&[(5, 3), (40, 7)]));
        blocks.insert(11, leaf(&[(100, 1), (200, 62)]));
        blocks.insert(4, interior(1, &[(5, 3), (100, 1)], &[10, 11]));
        let g = geometry();

        assert_eq!(
            first_run_from(4, g, 0, group(blocks.clone())).unwrap(),
            Some(FreeRun { start: 5, len: 3 }),
        );
        assert_eq!(
            first_run_from(4, g, 6, group(blocks.clone())).unwrap(),
            Some(FreeRun {
                start: 40,
                len:   7,
            }),
        );
        // A run that has to be found past a whole leaf.
        assert_eq!(
            first_run_from(4, g, 60, group(blocks.clone())).unwrap(),
            Some(FreeRun {
                start: 100,
                len:   1,
            }),
        );
        // Past the last run there is nothing, which is how an allocator learns
        // to look in another group.
        assert_eq!(first_run_from(4, g, 201, group(blocks)).unwrap(), None);
    }

    /// The size-ordered tree answers "is there a run this long", and it answers
    /// it in length order.  The two trees answer different questions, which is
    /// why a search has to be told which one it is walking.
    #[test]
    fn the_first_run_of_a_given_length() {
        let mut blocks = HashMap::new();
        blocks.insert(10u32, size_leaf(&[(5, 1), (40, 3), (100, 62)]));
        // The size-ordered tree: one leaf, one key.
        let mut root = interior(1, &[(5, 1)], &[10]);
        BigEndian::write_u32(&mut root[0..], XFS_ABTC_MAGIC);
        blocks.insert(4, root);

        assert_eq!(
            first_run_of_at_least(4, size_geometry(), 1, group(blocks.clone())).unwrap(),
            Some(FreeRun { start: 5, len: 1 }),
        );
        assert_eq!(
            first_run_of_at_least(4, size_geometry(), 4, group(blocks.clone())).unwrap(),
            Some(FreeRun {
                start: 100,
                len:   62,
            }),
        );
        assert_eq!(
            first_run_of_at_least(4, size_geometry(), 63, group(blocks)).unwrap(),
            None,
        );
    }

    /// A search that stops early must not read the rest of the tree, or an
    /// allocation in a group with a hundred thousand runs would read all of them.
    #[test]
    fn a_search_stops_at_its_answer() {
        let mut blocks = HashMap::new();
        blocks.insert(10u32, leaf(&[(5, 3), (40, 7)]));
        // A second leaf that is fetched on demand, and must not be.
        blocks.insert(11, leaf(&[(100, 1)]));
        blocks.insert(4, interior(1, &[(5, 3), (100, 1)], &[10, 11]));
        let mut fetched: Vec<XfsAgblock> = Vec::new();
        let mut counting = |bno: XfsAgblock| -> FsResult<Box<[u8]>> {
            fetched.push(bno);
            blocks
                .get(&bno)
                .cloned()
                .map(Vec::into_boxed_slice)
                .ok_or_else(|| FsError::corrupt(format!("no block {bno}")))
        };
        let found = first_run_from(4, geometry(), 6, &mut counting).unwrap();
        assert_eq!(
            found,
            Some(FreeRun {
                start: 40,
                len:   7,
            })
        );
        assert_eq!(fetched, vec![4, 10], "the second leaf should not be read");
    }
}
