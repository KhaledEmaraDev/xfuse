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
//! The b-tree that indexes which inode numbers a group has used.
//!
//! Where the free space btrees in [`super::free_space`] answer "which blocks are
//! free", this one answers "which inode numbers are in use", and it answers it
//! about *blocks* of inodes rather than about single inodes.  A part-used block
//! of inodes is in the tree; the free inodes inside it are free.
//!
//! That distinction is the whole reason the free inode count in the
//! [group inode header](super::agi) cannot be answered from this tree: the tree
//! says which ranges are used, and says nothing about the gaps between them.
//!
//! # Algorithm reference
//!
//! See [`docs/xfs-algorithms.md`] section 4 (Inode B-trees) for the algorithm map
//! with DOCUMENTED/MEASURED/IMPLEMENTED/HYPOTHESIS labels.

use crate::libxfuse::{
    definitions::{XfsAgblock, XfsIno},
    error::{FsError, FsResult},
    sb::Sb,
};

/// A free inode, and where in the group's records it was found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FreeIno {
    /// The inode's number across the whole file system.
    pub ino:         XfsIno,
    /// The leaf that holds the chunk's record.
    pub block:       XfsAgblock,
    /// The first inode number of the chunk.
    pub chunk_start: u64,
    /// Which bit of the chunk's mask says this inode is free.
    pub bit:         u32,
}

/// Every chunk a tree holds, with the leaf it is in, in the order the tree holds
/// them.
///
/// The order is the point: chunks are visited left to right so the first free
/// inode found is the lowest one, which makes allocation repeatable and keeps a
/// group filling from the start rather than from wherever its last hole was.
pub fn chunks_in_order<F>(root: XfsAgblock, mut fetch: F) -> FsResult<Vec<(XfsAgblock, InoRange)>>
where
    F: FnMut(XfsAgblock) -> FsResult<Box<[u8]>>,
{
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(block) = stack.pop() {
        let node = InobtNode::from_bytes(fetch(block)?)?;
        if node.is_leaf() {
            for chunk in node.ranges()? {
                out.push((block, chunk));
            }
            continue;
        }
        let mut children: Vec<XfsAgblock> = node.children()?.into_iter().map(|(_, b)| b).collect();
        children.reverse();
        stack.extend(children);
    }
    Ok(out)
}

/// The first free inode a group has.
///
/// The tree of used inode numbers is the only thing that knows which chunks
/// exist: the inode number says where a chunk *would* be, and the tree says
/// whether one is there.  These images space their records 160 inodes apart
/// rather than sixty-four, so working it out from the number alone would be
/// wrong -- and a chunk that has not been allocated has no record to find.
///
/// Only an existing chunk is offered.  Allocating a new one needs blocks out of
/// the group's free space and an entry added to this tree, which is a different
/// operation and is not done here.
pub fn first_free_ino<F>(
    sb: &Sb,
    agno: u32,
    root: XfsAgblock,
    fetch: F,
) -> FsResult<Option<FreeIno>>
where
    F: FnMut(XfsAgblock) -> FsResult<Box<[u8]>>,
{
    for (block, chunk) in chunks_in_order(root, fetch)? {
        if chunk.free != 0 {
            let bit = chunk.free.trailing_zeros();
            return Ok(Some(FreeIno {
                ino: sb.make_ino(agno, (chunk.start + u64::from(bit)) as u32),
                block,
                chunk_start: chunk.start,
                bit,
            }));
        }
    }
    Ok(None)
}

/// The magic at the start of every node: "IABT".
use crate::libxfuse::alloc::{agf::NULL_AGBLOCK, free_space::GroupBlocks};

/// The magic every node of the inode b-tree carries.
///
/// Both depths carry the same one: a 512-byte image's group 1 has an interior
/// node at block 12 and a leaf at block 6 and both read `IABT`.  The level field
/// is what tells them apart, which is why there is one magic here and not two.
///
/// Two magics exist depending on whether the filesystem has CRC support enabled:
/// the non-CRC `XFS_INOBT_MAGIC` ("IABT") and the CRC `XFS_INOBT_CRC_MAGIC`
/// ("IAB3").  `from_bytes` accepts either so that either layout decodes; which
/// one a node carries is fixed by the superblock and is only needed when *writing*
/// a node, to avoid rewriting a v4 image in v5 spelling or vice versa.
const XFS_INOBT_MAGIC: u32 = 0x4941_4254;
const XFS_INOBT_CRC_MAGIC: u32 = 0x4941_4233;

const MAGIC: usize = 0;
const LEVEL: usize = 4;
const NUMRECS: usize = 6;
const LEFTSIB: usize = 8;
const RIGHTSIB: usize = 12;
/// Where a leaf's records start on a file system without checksums.
const BODY_NO_CRC: usize = 16;
/// Where a leaf's records start on a file system with checksums.
///
/// The CRC header adds LSN (8), UUID (16), owner (8), and CRC (4) = 36 bytes,
/// rounded to 40 bytes of additional header, making 56 total.
const BODY_CRC: usize = 56;
/// How much a leaf's records are apart.
///
/// Sixteen, not the twelve the three fields it shows would suggest.  That is
/// the sort of thing the tool's own rendering hides: it prints the three fields
/// it knows about and nothing says a record ends after the third.
const LEAF_RECORD: usize = 16;

/// One node of the b-tree of used inode numbers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InobtNode {
    bytes:   Box<[u8]>,
    level:   u16,
    numrecs: u16,
    has_crc: bool,
}

/// A range of inode numbers the tree says are in use.
///
/// `free_count` and `first_free` are what a file system with a free inode tree
/// uses to carve a gap out of the range.  A version 1 header has no such tree, so
/// both are zero and the range is wholly used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InoRange {
    /// The first inode number this chunk covers.
    pub start:      u64,
    /// How many of the chunk's inodes are free, which is the number of set bits
    /// in [`Self::free`] and is checked against it.
    pub free_count: u32,
    /// Which of the chunk's inodes are free, one bit per inode.
    pub free:       u64,
}

/// How many inode numbers one chunk covers, which is the width of the mask.
/// `XFS_INODES_PER_CHUNK` -- `(NBBY * sizeof(xfs_inofree_t))`, and `xfs_inofree_t`
/// is a `uint64`.
///
/// Which is *why* a chunk is 64 inodes and not any other number: the free mask is a
/// single 64-bit word, so a chunk cannot hold more inodes than the mask has bits, and
/// there is no second word to grow into.  A writer that wanted a larger chunk has
/// nowhere to put the extra mask.
///
/// # The record, and the width that matters
///
/// ```text
///  0  ir_startino            u32   starting inode number
///  4  ir_freecount            u32   free count          -- normal chunk
///  4  ir_holemask             u16   hole mask           -- sparse chunk
///  6  ir_count                u8    total inode count   -- sparse chunk
///  7  ir_freecount            u8    free count          -- sparse chunk
///  8  ir_free                 u64   the free mask, one bit per inode
/// ```
///
/// Sixteen bytes either way, and **bytes 4..8 mean different things depending on the
/// chunk**.  A sparse chunk keeps its free count in a *single byte*, sharing those
/// four with a hole mask and a total count.  A writer that always writes `freecount`
/// as a `u32` at offset 4 corrupts every inode chunk on a sparse-metadata file
/// system -- and only those, which is what makes it the kind of defect that passes
/// every test in this repository and fails on the first real-world image.
///
/// So the kind is a property of the **chunk**, taken from the owning inode's
/// `XFS_DINODE_F_SPINODES` format flag, and never guessed from the record itself: a
/// record does not say which of the two layouts it is using.
pub const INODES_PER_CHUNK: u64 = 64;

/// Byte offsets within an `xfs_inobt_rec`, and the two meanings of `4..8`.
pub mod chunk_rec {
    /// `ir_startino` -- four bytes.
    pub const STARTINO: usize = 0;
    /// Where the two layouts disagree: a normal chunk's `ir_freecount`, or a sparse
    /// chunk's `ir_holemask`, `ir_count` and `ir_freecount`.
    pub const UNION: usize = 4;
    /// Size of that union, and so the offset of the free mask.
    pub const UNION_LEN: usize = 4;
    /// The whole record.
    pub const SIZE: usize = 16;
    /// `ir_free` -- the 64-bit mask, one bit per inode in the chunk.
    pub const FREE: usize = 8;

    /// Which of the two layouts a chunk uses.
    ///
    /// Taken from the owning inode, never inferred: **the record does not say.**
    /// Guessing produces an image that is right for half the world.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Kind {
        /// A full chunk: four bytes of `ir_freecount`.
        Normal,
        /// A sparse chunk: `ir_holemask`, `ir_count` and a **one-byte**
        /// `ir_freecount`.
        Sparse,
    }

    /// Reading and writing the free count at the chunk's width.
    ///
    /// A trait rather than inherent methods, so that every call has to *name* the
    /// layout it is assuming.  That is the whole point: the width is the one thing
    /// that differs, and a signature that does not mention it is how a sparse chunk
    /// gets read as a normal one and produces a plausible wrong number.
    pub trait Freecount {
        /// Read the free count out of a record's bytes.
        fn freecount(bytes: &[u8], kind: Kind) -> u32;
        /// Write the free count into a record's bytes.
        fn set_freecount(bytes: &mut [u8], kind: Kind, count: u32);
        /// The width of the free count, which is the only part that is not fixed.
        fn freecount_len(kind: Kind) -> usize;
    }

    impl Freecount for Kind {
        fn freecount_len(kind: Kind) -> usize {
            match kind {
                Kind::Normal => 4,
                Kind::Sparse => 1,
            }
        }

        fn freecount(bytes: &[u8], kind: Kind) -> u32 {
            match kind {
                Kind::Normal => {
                    u32::from_be_bytes(bytes[UNION..UNION + 4].try_into().expect("a chunk record"))
                }
                Kind::Sparse => bytes[UNION + 3] as u32,
            }
        }

        fn set_freecount(bytes: &mut [u8], kind: Kind, count: u32) {
            match kind {
                Kind::Normal => bytes[UNION..UNION + 4].copy_from_slice(&count.to_be_bytes()),
                Kind::Sparse => bytes[UNION + 3] = count as u8,
            }
        }
    }
}

impl InoRange {
    /// The lowest free inode in this chunk, if it has one.
    ///
    /// The lowest set bit rather than any set bit, so that allocation does not
    /// skip over the free inodes at the bottom of a chunk and leave them to be
    /// found all over again next time.
    pub fn first_free_ino(&self) -> Option<u64> {
        if self.free == 0 {
            return None;
        }
        Some(self.start + u64::from(self.free.trailing_zeros()))
    }

    /// Every free inode in this chunk, lowest first.
    pub fn free_inos(&self) -> Vec<u64> {
        let mut out = Vec::with_capacity(self.free_count as usize);
        let mut bits = self.free;
        while bits != 0 {
            let bit = bits.trailing_zeros();
            out.push(self.start + u64::from(bit));
            bits &= bits - 1;
        }
        out
    }

    /// Whether the chunk's free count agrees with its mask.
    ///
    /// They are two copies of one fact, and the format documentation says so, so
    /// a chunk where they disagree is a chunk that cannot be believed -- and
    /// there is no way to tell from here which of the two is the wrong one.
    pub fn count_agrees(&self) -> bool {
        self.free.count_ones() == self.free_count
    }
}

/// Insert a chunk into the tree, growing the tree if it has to.
///
/// Returns the tree's new root, which is the same block unless the tree was a
/// single leaf that had to become an interior node -- see
/// [`insert_chunk_above`] for what "growing" involves.
///
/// The two things this has to get right, and which the free space trees already
/// have to get right and got wrong twice between them:
///
/// * **a new node comes from the group.**  A tree that cannot grow without a
///   block must not pretend otherwise, which is why inserting into a full tree was
///   a refusal rather than a write.  This is not a corner case: a leaf in a
///   512-byte block holds 31 records and `xfsv4.img`'s group 1 has nine of them,
///   all full, so the *first* new chunk in that group needs a split.
/// * **a new node's siblings have to be relinked.**  The chain is structure, not
///   a hint: a node left out of it is one no walk can reach, and no comparison of
///   free space would ever notice.
pub fn insert_chunk<B: GroupBlocks>(blocks: &mut B, root: u32, range: InoRange) -> FsResult<u32> {
    // Down to the leaf that should hold the record, remembering the way: which
    // parent, and which child of it we came through.
    let mut path: Vec<(u32, usize)> = Vec::new();
    let mut block = root;
    let mut leaf = loop {
        let node = InobtNode::from_bytes(blocks.get(block)?)?;
        if node.is_leaf() {
            break node;
        }
        let children = node.children()?;
        if children.is_empty() {
            return Err(FsError::corrupt(
                "an interior node of the inode tree has no children",
            ));
        }
        // The child whose keys cover `start`.  The last child covers everything
        // above its first key, which is what makes a tree's keys separators
        // rather than bounds on their own.
        let at = child_index(&children, range.start).min(children.len() - 1);
        path.push((block, at));
        block = children[at].1;
    };
    match leaf.insert_range(&range) {
        Ok(()) => {
            // **Write the leaf back.**  Not doing so is silent and total: the
            // insert succeeds, the tree is not changed, the caller believes the
            // chunk is there, and nothing anywhere reports an error.  The tree
            // never grows either, because the leaf it keeps landing in is never
            // any fuller -- which is how this was found, by a test that added two
            // hundred chunks to a group whose leaves were full and watched none of
            // them split.
            leaf.update_crc();
            blocks.put(block, leaf.into_bytes())?;
            Ok(root)
        }
        Err(FsError::NoSpace) => {
            let root = insert_chunk_above(blocks, root, &mut path, block, leaf, &range)?;
            // **And then insert the chunk**, which the split has not done: a split
            // makes room, it does not use it.  Dropping the record here is silent
            // too, and shows up one counter away, as a group that claims an inode
            // the tree has never heard of --
            //
            // ```text
            // agi_count 16960, counted 16896 in ag 1
            // sb_icount 22656, counted 22592
            // ```
            //
            // -- a whole chunk short.  The tree is bigger by then, so the walk
            // starts again from the new root; it strictly grows, so this cannot
            // loop, and the bound is here only so that a bug says so rather than
            // hanging.
            let mut root = root;
            for _ in 0..depth_bound(blocks, root) {
                let mut path = Vec::new();
                let mut block = root;
                let mut leaf = loop {
                    let node = InobtNode::from_bytes(blocks.get(block)?)?;
                    if node.is_leaf() {
                        break node;
                    }
                    let children = node.children()?;
                    if children.is_empty() {
                        return Err(FsError::corrupt(
                            "an interior node of the inode tree has no children",
                        ));
                    }
                    let at = child_index(&children, range.start).min(children.len() - 1);
                    path.push((block, at));
                    block = children[at].1;
                };
                match leaf.insert_range(&range) {
                    Ok(()) => {
                        leaf.update_crc();
                        blocks.put(block, leaf.into_bytes())?;
                        return Ok(root);
                    }
                    Err(FsError::NoSpace) => {
                        root = insert_chunk_above(blocks, root, &mut path, block, leaf, &range)?;
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(FsError::Corrupt {
                what: "the inode tree kept splitting and the chunk never fitted".into(),
            })
        }
        Err(e) => Err(e),
    }
}

/// A bound on how deep a tree can be, for a loop that must end.
///
/// The root's own level plus one, read from the tree rather than assumed: every
/// round of that loop splits a leaf, and a split raises the tree by at most a level,
/// so the root's depth bounds the number of rounds there can be.
fn depth_bound<B: GroupBlocks>(blocks: &mut B, root: u32) -> usize {
    match blocks
        .get(root)
        .and_then(|b| InobtNode::from_bytes(b.into_vec()))
    {
        Ok(node) => node.level() as usize + 2,
        Err(_) => 2,
    }
}

/// Which child of an interior node a chunk belongs in.
fn child_index(children: &[(u64, u32)], start: u64) -> usize {
    children.partition_point(|(key, _)| *key < start)
}

/// Split a full leaf and put the new one into the tree.
///
/// `leaf_block` is the full leaf and `path` is the way down to it, outermost
/// first.  Returns the tree's new root.
fn insert_chunk_above<B: GroupBlocks>(
    blocks: &mut B,
    root: u32,
    path: &mut Vec<(u32, usize)>,
    leaf_block: u32,
    mut leaf: InobtNode,
    _range: &InoRange,
) -> FsResult<u32> {
    // Half each, and the right half keeps the higher records, so the order is the
    // same on both sides of the split as it was before it.
    let records = leaf.ranges()?;
    let half = records.len().div_ceil(2);
    let (low, high) = records.split_at(half);
    let size = leaf.len();
    let old_right = leaf.right_sibling().filter(|b| *b != NULL_AGBLOCK);
    leaf.replace_ranges(low)?;
    leaf.link_right(NULL_AGBLOCK);
    leaf.update_crc();
    let has_crc = leaf.has_crc;
    blocks.put(leaf_block, leaf.into_bytes())?;
    let sibling_block = blocks.take_inode_tree_block()?;
    let mut sibling = InobtNode::empty_leaf(size, has_crc);
    sibling.replace_ranges(high)?;
    sibling.update_crc();
    blocks.put(sibling_block, sibling.into_bytes())?;

    // The chain, both ways.  The new node goes between the leaf and whatever was
    // to its right, and **both sides** are relinked: a chain that names the new
    // node only on one side is a chain no walk can follow.
    let mut sibling = InobtNode::from_bytes(blocks.get(sibling_block)?)?;
    sibling.link_left(leaf_block);
    if let Some(right) = old_right {
        sibling.link_right(right);
        let mut node = InobtNode::from_bytes(blocks.get(right)?)?;
        node.link_left(sibling_block);
        node.update_crc();
        blocks.put(right, node.into_bytes())?;
    }
    sibling.update_crc();
    blocks.put(sibling_block, sibling.into_bytes())?;
    let mut leaf = InobtNode::from_bytes(blocks.get(leaf_block)?)?;
    leaf.link_right(sibling_block);
    leaf.update_crc();
    blocks.put(leaf_block, leaf.into_bytes())?;

    let sibling_key = high
        .first()
        .map(|c| c.start)
        .ok_or_else(|| FsError::corrupt("a split left the new node with no records"))?;

    // And now the parent, the innermost one -- one step, not a loop, because each
    // step either finishes or hands the same problem one level up.
    if let Some((parent_block, at)) = path.pop() {
        let mut parent = InobtNode::from_bytes(blocks.get(parent_block)?)?;
        match parent.insert_child(at + 1, sibling_key, sibling_block) {
            Ok(()) => {
                parent.update_crc();
                blocks.put(parent_block, parent.into_bytes())?;
                return Ok(root);
            }
            Err(FsError::NoSpace) => {
                // This parent is full, which is the same problem one level up, so
                // it goes through the same split with the path above it already
                // built.
                let child = parent.child_block(at)?;
                insert_chunk_above(blocks, root, path, child, parent, _range)?;
                return Ok(root);
            }
            Err(e) => return Err(e),
        }
    }

    // Nothing above the leaf: the tree was a single leaf, so it needs a new
    // interior node above it, and that is a root that has moved.
    let left = InobtNode::from_bytes(blocks.get(leaf_block)?)?;
    let left_key = left.ranges()?.first().map(|c| c.start).unwrap_or(0);
    let root_block = blocks.take_inode_tree_block()?;
    let mut new_root = InobtNode::new_interior(size, has_crc);
    new_root.push_child(left_key, leaf_block)?;
    new_root.push_child(sibling_key, sibling_block)?;
    new_root.update_crc();
    blocks.put(root_block, new_root.into_bytes())?;
    // The two nodes below it must no longer claim a parent, and their chain must
    // start at the new root's children rather than at each other.
    let mut left = InobtNode::from_bytes(blocks.get(leaf_block)?)?;
    left.link_left(NULL_AGBLOCK);
    left.update_crc();
    blocks.put(leaf_block, left.into_bytes())?;
    let mut right = InobtNode::from_bytes(blocks.get(sibling_block)?)?;
    right.link_left(NULL_AGBLOCK);
    right.update_crc();
    blocks.put(sibling_block, right.into_bytes())?;
    Ok(root_block)
}

/// Every range of inode numbers a tree says are in use, in the order the tree
/// holds them.
///
/// The order matters: the ranges are runs of used inode numbers and they are read
/// in key order, so the counts can be added up in that order and mean something.
///
/// Unlike the free space trees, this takes no geometry: a node's shape follows
/// from its own size, because a record is sixteen bytes whichever way round it
/// is counted.
pub fn ranges_in_tree<F>(root: u32, mut fetch: F) -> FsResult<Vec<InoRange>>
where
    F: FnMut(u32) -> FsResult<Box<[u8]>>,
{
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(block) = stack.pop() {
        let node = InobtNode::from_bytes(fetch(block)?)?;
        if node.is_leaf() {
            out.extend(node.ranges()?);
            continue;
        }
        // Pushed in reverse so that popping visits them left to right, which is
        // what makes the ranges come out in the order the tree holds them.
        let mut children = node.children()?;
        children.reverse();
        stack.extend(children.into_iter().map(|(_, block)| block));
    }
    Ok(out)
}

impl InobtNode {
    /// Read a node out of a block's bytes.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> FsResult<Self> {
        let bytes = bytes.into();
        if bytes.len() < BODY_NO_CRC + 4 {
            return Err(FsError::corrupt(
                "an inode b-tree node is too short to hold a header",
            ));
        }
        let magic = u32::from_be_bytes(bytes[MAGIC..MAGIC + 4].try_into().unwrap());
        if magic != XFS_INOBT_MAGIC && magic != XFS_INOBT_CRC_MAGIC {
            return Err(FsError::corrupt(
                "expected the start of an inode b-tree node",
            ));
        }
        let has_crc = magic == XFS_INOBT_CRC_MAGIC;
        Ok(Self {
            level: u16::from_be_bytes(bytes[LEVEL..LEVEL + 2].try_into().unwrap()),
            numrecs: u16::from_be_bytes(bytes[NUMRECS..NUMRECS + 2].try_into().unwrap()),
            has_crc,
            bytes: bytes.into_boxed_slice(),
        })
    }

    /// The bytes as they were read, for writing back.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The bytes, for writing back.
    pub fn into_bytes(self) -> Box<[u8]> {
        self.bytes
    }

    /// Recompute the node's checksum over the node it now is.
    ///
    /// A record changed without it changed is a record nothing can be trusted
    /// from -- but **only where the file system checksums its metadata**, and
    /// neither reference image does: both are version 4 built without checksums,
    /// so these nodes carry none and there is nothing to recompute.
    ///
    /// That is not the same as there being nothing to do on a checksummed file
    /// system, and this is not written to pretend otherwise.  Where a checksum
    /// goes in a version 5 inode b-tree node has not been established here, and
    /// writing one at a guessed offset would corrupt a real image rather than
    /// protect it.  So this does nothing until that offset is known, and the
    /// gap is recorded here rather than papered over.
    pub fn update_crc(&mut self) {
        // Nothing to recompute: see above.
        let _ = &mut self.bytes;
    }

    fn u64_at(&self, at: usize) -> u64 {
        let mut v = [0u8; 8];
        v.copy_from_slice(&self.bytes[at..at + 8]);
        u64::from_be_bytes(v)
    }

    fn u32_at(&self, at: usize) -> u32 {
        let mut v = [0u8; 4];
        v.copy_from_slice(&self.bytes[at..at + 4]);
        u32::from_be_bytes(v)
    }

    /// How deep this node is; a leaf is zero.
    pub const fn level(&self) -> u16 {
        self.level
    }

    /// How many records, or children, it holds.
    pub const fn numrecs(&self) -> u16 {
        self.numrecs
    }

    /// Whether this node holds records rather than children.
    pub const fn is_leaf(&self) -> bool {
        self.level == 0
    }

    /// The block holding the node to its left, if there is one.
    pub fn left_sibling(&self) -> Option<u32> {
        self.sibling(LEFTSIB)
    }

    /// The block holding the node to its right, if there is one.
    pub fn right_sibling(&self) -> Option<u32> {
        self.sibling(RIGHTSIB)
    }

    fn sibling(&self, at: usize) -> Option<u32> {
        match self.u32_at(at) {
            u32::MAX => None,
            block => Some(block),
        }
    }

    /// How many records a leaf of this size can hold.
    pub fn leaf_capacity(blocksize: usize, has_crc: bool) -> usize {
        let body = if has_crc { BODY_CRC } else { BODY_NO_CRC };
        (blocksize - body) / LEAF_RECORD
    }

    /// How many children an interior node of this size can hold.
    pub fn interior_capacity(blocksize: usize, has_crc: bool) -> usize {
        let body = if has_crc { BODY_CRC } else { BODY_NO_CRC };
        (blocksize - body) / 8
    }

    /// Put a chunk into this leaf, in the order the leaf keeps them.
    ///
    /// The records are the tree's keys, so they have to be in order: a record
    /// inserted in the wrong place makes a tree whose *contents* are right and
    /// whose *shape* is wrong, which is a fault that a walk cannot see and
    /// `xfs_repair` reports as an out-of-order record.
    ///
    /// A leaf that is full is refused rather than overflowing.  A full leaf needs
    /// a split, which needs a block from the group's free space, and this is a
    /// leaf mutation: it cannot take a block for itself, and a caller that cannot
    /// finish the operation must not have started it.  `ENOSPC` here means "the
    /// tree would have to grow", not "the group is full", and the difference
    /// matters to whoever catches it.
    pub fn insert_range(&mut self, range: &InoRange) -> FsResult<()> {
        if !self.is_leaf() {
            return Err(FsError::invalid(
                libc::EINVAL,
                "only a leaf of the inode tree holds chunks",
            ));
        }
        let at = Self::leaf_capacity(self.bytes.len(), self.has_crc);
        if self.numrecs as usize >= at {
            return Err(FsError::NoSpace);
        }
        let existing = self.ranges()?;
        let at = existing.partition_point(|c| c.start < range.start);
        if existing.iter().any(|c| c.start == range.start) {
            return Err(FsError::corrupt(format!(
                "the inode tree already holds a chunk starting at {}",
                range.start
            )));
        }
        if let Some(next) = existing.get(at) {
            if range.start < next.start || range.start + INODES_PER_CHUNK > next.start {
                return Err(FsError::corrupt(format!(
                    "a chunk starting at {} would overlap the one at {}",
                    range.start, next.start
                )));
            }
        }
        self.write_range(at, range)?;
        self.numrecs += 1;
        self.renumber();
        Ok(())
    }

    fn write_range(&mut self, at: usize, range: &InoRange) -> FsResult<()> {
        if range.count_agrees() {
            // nothing to say, and the two fields that matter are written below
        } else {
            return Err(FsError::corrupt(
                "a chunk's free count disagrees with its mask",
            ));
        }
        let start = u32::try_from(range.start).map_err(|_| FsError::Corrupt {
            what: format!("inode number {} does not fit a chunk record", range.start),
        })?;
        let body = if self.has_crc { BODY_CRC } else { BODY_NO_CRC };
        let at = body + at * LEAF_RECORD;
        let room = self.bytes.len().checked_sub(at + LEAF_RECORD);
        if room.is_none() {
            return Err(FsError::corrupt(
                "a leaf of the inode tree does not hold the record it claims to",
            ));
        }
        self.bytes[at..at + 4].copy_from_slice(&start.to_be_bytes());
        self.bytes[at + 4..at + 8].copy_from_slice(&range.free_count.to_be_bytes());
        self.bytes[at + 8..at + 16].copy_from_slice(&range.free.to_be_bytes());
        Ok(())
    }

    /// Put the record count the header says this node has back into its bytes.
    ///
    /// The count lives in two places -- the field the struct was built with and
    /// the bytes -- and they have to move together, which is invisible in a test
    /// that reads a node back through the struct that wrote it.
    fn renumber(&mut self) {
        self.bytes[6..8].copy_from_slice(&self.numrecs.to_be_bytes());
    }

    /// The ranges a leaf holds.
    pub fn ranges(&self) -> FsResult<Vec<InoRange>> {
        if !self.is_leaf() {
            return Err(FsError::invalid(
                libc::EINVAL,
                "an interior node holds children, not ranges",
            ));
        }
        (0..self.numrecs as usize)
            .map(|i| {
                let body = if self.has_crc { BODY_CRC } else { BODY_NO_CRC };
                let at = body + i * LEAF_RECORD;
                Ok(InoRange {
                    start:      u64::from(self.u32_at(at)),
                    free_count: self.u32_at(at + 4),
                    free:       self.u64_at(at + 8),
                })
            })
            .collect()
    }

    /// The first inode number under each child of an interior node, paired with
    /// the block holding it.
    /// How many bytes this node is, which is how a new one is sized to match.
    fn len(&self) -> usize {
        self.bytes.len()
    }

    /// A node of this size that holds nothing.
    fn empty_like(&self, leaf: bool) -> Self {
        Self::blank(self.bytes.len(), if leaf { 0 } else { 1 }, self.has_crc)
    }

    /// A leaf of this size that holds nothing.
    fn empty_leaf(size: usize, has_crc: bool) -> Self {
        Self::blank(size, 0, has_crc)
    }

    /// An interior node of this size that holds nothing.
    fn new_interior(size: usize, has_crc: bool) -> Self {
        Self::blank(size, 1, has_crc)
    }

    /// A node of this size and depth that holds nothing and has no siblings.
    ///
    /// A new node's siblings are set by whoever links it in, and are the null
    /// block until they are: a node that claimed a sibling it had not been given
    /// would put the chain into a block that is not part of the tree.
    fn blank(size: usize, level: u16, has_crc: bool) -> Self {
        let mut bytes = vec![0u8; size];
        let magic = if has_crc {
            XFS_INOBT_CRC_MAGIC
        } else {
            XFS_INOBT_MAGIC
        };
        bytes[MAGIC..MAGIC + 4].copy_from_slice(&magic.to_be_bytes());
        bytes[LEVEL..LEVEL + 2].copy_from_slice(&level.to_be_bytes());
        bytes[LEFTSIB..LEFTSIB + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        bytes[RIGHTSIB..RIGHTSIB + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        InobtNode {
            bytes: bytes.into_boxed_slice(),
            level,
            numrecs: 0,
            has_crc,
        }
    }

    /// Put this node's records in place of its own.
    ///
    /// Used by a split: the records that were here are now two halves, and this is
    /// one of them.  The count moves with them, because a leaf whose count and
    /// records disagree is a leaf nothing can be read out of.
    fn replace_ranges(&mut self, ranges: &[InoRange]) -> FsResult<()> {
        if !self.is_leaf() {
            return Err(FsError::invalid(
                libc::EINVAL,
                "only a leaf of the inode tree holds chunks",
            ));
        }
        let at = Self::leaf_capacity(self.bytes.len(), self.has_crc);
        if ranges.len() > at {
            return Err(FsError::corrupt(
                "a split left more records in a node than the node holds",
            ));
        }
        self.numrecs = 0;
        for (i, r) in ranges.iter().enumerate() {
            self.write_range(i, r)?;
            self.numrecs += 1;
        }
        self.renumber();
        Ok(())
    }

    /// Say which node is to this one's left, or none.
    pub fn link_left(&mut self, block: u32) {
        self.bytes[LEFTSIB..LEFTSIB + 4].copy_from_slice(&block.to_be_bytes());
    }

    /// Say which node is to this one's right, or none.
    pub fn link_right(&mut self, block: u32) {
        self.bytes[RIGHTSIB..RIGHTSIB + 4].copy_from_slice(&block.to_be_bytes());
    }

    /// The block of this interior node's `at`th child.
    fn child_block(&self, at: usize) -> FsResult<u32> {
        if self.is_leaf() {
            return Err(FsError::invalid(
                libc::EINVAL,
                "a leaf holds chunks, not children",
            ));
        }
        let body = if self.has_crc { BODY_CRC } else { BODY_NO_CRC };
        let ptrs = body + self.capacity() * 4 + at * 4;
        Ok(self.u32_at(ptrs))
    }

    /// Put a child at `at`, making room by refusing when there is none.
    ///
    /// A node that is full is a node that has to split, and refusing here is what
    /// makes that the caller's to arrange: a node cannot take a block for itself.
    pub fn insert_child(&mut self, at: usize, key: u64, block: u32) -> FsResult<()> {
        if self.is_leaf() {
            return Err(FsError::invalid(
                libc::EINVAL,
                "a leaf holds chunks, not children",
            ));
        }
        let capacity = self.capacity();
        if self.numrecs as usize >= capacity {
            return Err(FsError::NoSpace);
        }
        let at = at.min(self.numrecs as usize);
        let keys = if self.has_crc { BODY_CRC } else { BODY_NO_CRC };
        let ptrs = keys + capacity * 4;
        // Shift the tail of both arrays right by one, from the back so nothing is
        // overwritten before it has been moved.
        for i in (at..self.numrecs as usize).rev() {
            let k = self.u32_at(keys + i * 4);
            let p = self.u32_at(ptrs + i * 4);
            self.bytes[keys + (i + 1) * 4..keys + (i + 1) * 4 + 4]
                .copy_from_slice(&k.to_be_bytes());
            self.bytes[ptrs + (i + 1) * 4..ptrs + (i + 1) * 4 + 4]
                .copy_from_slice(&p.to_be_bytes());
        }
        let key = u32::try_from(key).map_err(|_| FsError::Corrupt {
            what: format!("chunk start {key} does not fit an interior key"),
        })?;
        self.bytes[keys + at * 4..keys + at * 4 + 4].copy_from_slice(&key.to_be_bytes());
        self.bytes[ptrs + at * 4..ptrs + at * 4 + 4].copy_from_slice(&block.to_be_bytes());
        self.numrecs += 1;
        self.renumber();
        Ok(())
    }

    /// Add a child at the end, which is where a split's new child goes.
    fn push_child(&mut self, key: u64, block: u32) -> FsResult<()> {
        let at = self.numrecs as usize;
        self.insert_child(at, key, block)
    }

    pub fn children(&self) -> FsResult<Vec<(u64, u32)>> {
        if self.is_leaf() {
            return Err(FsError::invalid(
                libc::EINVAL,
                "a leaf holds ranges, not children",
            ));
        }
        let keys = if self.has_crc { BODY_CRC } else { BODY_NO_CRC };
        let ptrs = keys + self.capacity() * 4;
        (0..self.numrecs as usize)
            .map(|i| {
                Ok((
                    u64::from(self.u32_at(keys + i * 4)),
                    self.u32_at(ptrs + i * 4),
                ))
            })
            .collect()
    }

    fn capacity(&self) -> usize {
        InobtNode::interior_capacity(self.bytes.len(), self.has_crc)
    }
}

#[cfg(test)]
mod t {
    use super::*;
    use crate::libxfuse::{alloc::agi::Agi, error::FsResult, sb::Sb};

    /// A file system made by hand, whose free space is heavily fragmented.
    fn golden() -> Option<std::path::PathBuf> {
        crate::libxfuse::alloc::golden("xfsv4.img")
    }
    /// A file system made by mkfs, whose inode numbers agree with its own headers.
    fn fresh() -> Option<std::path::PathBuf> {
        crate::libxfuse::alloc::golden("xfs_writable.img")
    }

    /// The values `xfs_db` prints inside brackets, as in `1:[32,0,0]`.
    ///
    /// Not every number on the line: it also numbers the records, so taking
    /// every number would compare our values against `1, 2, 3...`.
    fn bracketed(line: &str) -> Vec<u64> {
        line.rsplit_once('[')
            .and_then(|(_, rest)| rest.split_once(']'))
            // `rsplit_once` leaves the remainder second, `split_once` leaves what
            // precedes the delimiter first, so the two are unpacked opposite ways
            // on purpose.
            .map(|(inner, _)| {
                inner
                    .split(',')
                    .filter_map(|v| v.trim().parse::<u64>().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The values `xfs_db` prints after each colon, as in `1:6 2:11`.
    fn after_colon(line: &str) -> Vec<u32> {
        line.split_whitespace()
            .filter_map(|w| w.split_once(':'))
            .map(|(_, v)| v)
            .filter_map(|v| v.parse::<u32>().ok())
            .collect()
    }

    /// Ask `xfs_db` to print one field of a node, so the reader is checked
    /// against the tool rather than against itself.
    fn shown(block: u32, field: &str) -> Option<Vec<String>> {
        let golden = golden()?;
        let out = std::process::Command::new("xfs_db")
            .arg("-r")
            .arg("-c")
            .arg(format!("daddr {block}"))
            .arg("-c")
            .arg("type inobt")
            .arg("-c")
            .arg(format!("p {field}"))
            .arg(golden.as_os_str())
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        if text.contains("supported types") {
            return None;
        }
        // Every line is offered to the extractors, and they throw away what is
        // not data.  Filtering the lines first looks tidier and is wrong twice
        // over: record numbers run past nine, and `xfs_db` prints the child
        // blocks on the same line as the field name rather than one per line.
        Some(
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect(),
        )
    }

    /// A scalar field of a node, as `xfs_db` prints it: `level = 1`.
    fn scalar_of(block: u32, field: &str) -> Option<String> {
        let golden = golden()?;
        let out = std::process::Command::new("xfs_db")
            .arg("-r")
            .arg("-c")
            .arg(format!("daddr {block}"))
            .arg("-c")
            .arg("type inobt")
            .arg("-c")
            .arg(format!("p {field}"))
            .arg(golden.as_os_str())
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        eprintln!(
            "DEBUG scalar_of({block},{field}) status={} out={:?} err={:?}",
            out.status,
            text,
            String::from_utf8_lossy(&out.stderr)
        );
        text.lines().map(str::trim).find_map(|l| {
            l.strip_prefix(&format!("{field} = "))
                .map(|v| v.trim().to_string())
        })
    }

    fn read(block: u32) -> Option<InobtNode> {
        let golden = golden()?;
        let mut reader = std::io::BufReader::new(std::fs::File::open(golden.as_os_str()).ok()?);
        let sb = crate::libxfuse::sb::Sb::from(&mut reader);
        let bytes = std::fs::read(golden.as_os_str()).ok()?;
        let at = block as usize * sb.sb_blocksize as usize;
        InobtNode::from_bytes(bytes[at..at + sb.sb_blocksize as usize].to_vec()).ok()
    }

    /// An interior node reads as the keys and child blocks `xfs_db` prints.
    #[test]
    fn an_interior_node_reads_as_xfs_db_prints_it() {
        if !crate::libxfuse::alloc::have_xfs_db() {
            eprintln!("skipping: no xfs_db to check against");
            return;
        }
        if golden().is_none() {
            eprintln!("skipping: no unpacked xfsv4.img");
            return;
        }
        let root = 12u32;
        let node = read(root).expect("the inode b-tree root");
        let children = node.children().expect("children");

        let keys: Vec<u64> = shown(root, "keys")
            .expect("xfs_db prints the keys")
            .iter()
            .flat_map(|l| bracketed(l))
            .collect();
        assert_eq!(
            children.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            keys,
            "the inode number keys disagree with xfs_db"
        );

        let ptrs: Vec<u32> = shown(root, "ptrs")
            .expect("xfs_db prints the child blocks")
            .iter()
            .flat_map(|l| after_colon(l))
            .collect();
        assert_eq!(
            children.iter().map(|(_, b)| *b).collect::<Vec<_>>(),
            ptrs,
            "the child blocks disagree with xfs_db"
        );

        // And the node's own header, which is what says which of the two shapes
        // it is.
        let level = scalar_of(root, "level").expect("xfs_db prints a level");
        assert_eq!(
            node.level().to_string(),
            level,
            "level disagrees with xfs_db"
        );
        let numrecs = scalar_of(root, "numrecs").expect("xfs_db prints a count");
        assert_eq!(
            node.numrecs().to_string(),
            numrecs,
            "record count disagrees with xfs_db"
        );
    }

    /// The extraction itself, on the exact lines the tool prints.
    #[test]
    fn the_extraction_handles_what_xfs_db_prints() {
        for line in ["1:[32]", "2:[2400]", "1:[32,0,0]", "17:[2240,0,0]"] {
            eprintln!("line={line:?} -> {:?}", bracketed(line));
        }
        for l in [
            "recs[1-17] = [startino,freecount,free]",
            "1:[32,0,0]",
            "2:[192,0,0]",
        ] {
            eprintln!("line={l:?} -> {:?}", bracketed(l));
        }
        assert_eq!(bracketed("1:[32,0,0]"), vec![32, 0, 0]);
        assert_eq!(bracketed("2:[2400]"), vec![2400]);
        assert_eq!(after_colon("1:6 2:11"), vec![6, 11]);
    }

    /// The first free inode a group has is the lowest one, and there really are
    /// as many of them as the header says.
    ///
    /// The tree is the only thing that knows which chunks exist, so this checks
    /// it two ways against the file system's own accounting: the free inodes it
    /// finds must add up to the group header's count, and the lowest one it finds
    /// must be the lowest bit of the lowest chunk that has one.  Getting the
    /// order wrong would not change either number -- it would just hand out
    /// higher inode numbers than it should, which nothing else here would notice.
    #[test]
    fn the_first_free_inode_is_the_lowest_and_the_counts_agree() {
        let Some(sb) = crate::libxfuse::alloc::sb_of_xfsv4() else {
            eprintln!("skipping: no unpacked xfsv4.img");
            return;
        };
        let Some(golden) = crate::libxfuse::alloc::golden("xfsv4.img") else {
            return;
        };
        let bytes = std::fs::read(&golden).expect("the image");
        let bs = sb.sb_blocksize as usize;
        let mut total_free = 0u64;
        let mut lowest: Option<u64> = None;
        for agno in 0..sb.agcount() {
            let at = sb.ag_header_offset(agno, Sb::AGI_SECTOR) as usize;
            let agi = Agi::from_bytes(bytes[at..at + bs].to_vec(), sb.has_crc()).expect("a header");
            let ag_offset = sb.ag_offset(agno) as usize;
            let fetch = |b: XfsAgblock| -> FsResult<Box<[u8]>> {
                let at = ag_offset + b as usize * bs;
                Ok(bytes[at..at + bs].to_vec().into_boxed_slice())
            };
            let chunks = chunks_in_order(agi.inobt_root(), fetch).expect("the chunks");
            let mut in_group = 0u64;
            for (_, chunk) in &chunks {
                in_group += u64::from(chunk.free.count_ones());
            }
            assert_eq!(
                in_group,
                agi.free_inodes(),
                "group {agno}: the chunks hold {in_group} free inodes and the header says {}",
                agi.free_inodes()
            );
            total_free += in_group;
            if let Some(found) =
                first_free_ino(&sb, agno, agi.inobt_root(), fetch).expect("a search")
            {
                let here = lowest.is_none();
                if here {
                    lowest = Some(found.ino);
                }
            }
        }
        assert_eq!(
            total_free, sb.sb_ifree,
            "the chunks hold {total_free} free inodes across the file system and the superblock \
             says {}",
            sb.sb_ifree
        );
        // The lowest free inode in the file system is the one group 0 offers,
        // since groups are numbered before the numbers that reach them.
        let lowest = lowest.expect("a file system with free inodes");
        let loc = sb.locate_ino(lowest);
        assert_eq!(
            loc.agno, 0,
            "the lowest free inode should be in the first group"
        );
        // And it must really be free in the tree that says so.
        let at = sb.ino_to_offset(lowest) as usize;
        assert!(
            at + 2 <= bytes.len(),
            "inode {lowest} has no bytes in the image"
        );
        eprintln!(
            "lowest free inode is {lowest} (group {}, block {}, slot {}); {total_free} free \
             inodes in total",
            loc.agno, loc.agbno, loc.slot
        );
    }

    /// A chunk's free count is the number of free inodes in its mask.
    ///
    /// The format documentation says these are one fact written twice, so this is
    /// the invariant to hold them to: a chunk whose count disagrees with its mask
    /// cannot be believed, and nothing here can say which of the two is wrong.
    ///
    /// It is also the strongest check available on reading a chunk, because it
    /// ties the eight byte mask to the four byte count beside it.  A reader that
    /// had the mask's offset wrong would still produce a plausible count, and
    /// would fail here -- which is how a sixteen byte record that shows three
    /// printed fields turns out to be three printed fields and a mask.
    #[test]
    fn a_chunks_free_count_is_the_number_of_free_inodes_in_its_mask() {
        if !crate::libxfuse::alloc::have_xfs_db() {
            eprintln!("skipping: no xfs_db to check against");
            return;
        }
        // Only the freshly made image.  The hand-built one's inode tree does not
        // resolve from its own headers -- walking it runs into a block that is
        // not a node of that tree -- which is the same disagreement about inode
        // numbers that the count check below runs into.  It is a good image for
        // most things and cannot be asked about inodes.
        let Some(path) = fresh() else {
            eprintln!("skipping: no unpacked xfs_writable.img");
            return;
        };

        for golden in [&path] {
            let Ok(bytes) = std::fs::read(golden) else {
                continue;
            };
            let mut reader = std::io::BufReader::new(std::fs::File::open(golden).unwrap());
            let sb = Sb::from(&mut reader);
            let bs = sb.sb_blocksize as usize;
            let fetch = |block: u32| -> FsResult<Box<[u8]>> {
                let at = block as usize * bs;
                Ok(bytes[at..at + bs].to_vec().into_boxed_slice())
            };
            let mut checked = 0usize;
            for agno in 0..sb.agcount() {
                let at = sb.ag_header_offset(agno, Sb::AGI_SECTOR) as usize;
                let Ok(agi) = Agi::from_bytes(bytes[at..at + bs].to_vec(), sb.has_crc()) else {
                    continue;
                };
                for chunk in ranges_in_tree(agi.inobt_root(), fetch).expect("the chunks") {
                    assert!(
                        chunk.count_agrees(),
                        "{golden:?} ag{agno}: the chunk at inode {} claims {} free inodes but its \
                         mask has {} bits set",
                        chunk.start,
                        chunk.free_count,
                        chunk.free.count_ones()
                    );
                    assert_eq!(
                        chunk.free_inos().len() as u32,
                        chunk.free_count,
                        "{golden:?} ag{agno}: the chunk does not name as many free inodes as it \
                         says"
                    );
                    for ino in chunk.free_inos() {
                        assert!(
                            (chunk.start..chunk.start + INODES_PER_CHUNK).contains(&ino),
                            "{golden:?} ag{agno}: inode {ino} is outside the chunk that claims it"
                        );
                    }
                    if chunk.free_count > 0 {
                        assert!(
                            chunk.first_free_ino().is_some(),
                            "{golden:?} ag{agno}: a chunk with free inodes cannot name one"
                        );
                    }
                    checked += 1;
                }
            }
            if checked > 0 {
                eprintln!("{golden:?}: {checked} chunks checked");
            }
        }
    }

    /// The header's free inode count is the ranges' own free counts, added up.
    ///
    /// This is the check the walk rests on, and it settles two things at once:
    /// that the ranges are being read correctly, and that free inodes are counted
    /// where the tree says rather than by counting gaps.
    ///
    /// The gap below the first range is *not* free, and that is worth stating
    /// rather than leaving as a surprise: a group's inode numbers start at zero
    /// and its first used range starts at thirty-two, so the numbers below that
    /// are reserved and are not offered to anyone.
    ///
    /// The third field of a range is not used here, and this does not say what it
    /// is.  `xfs_db` prints it as a value that does not look like an offset or a
    /// count, and the range's own length is not established either -- only that
    /// its free count is.  Guessing at the rest would be a number that means
    /// something plausible, which is the failure this whole file is written to
    /// avoid.
    #[test]
    fn the_free_inode_count_is_the_ranges_own_counts() {
        // The freshly made image, and not the hand-built one: the hand-built
        // image's inode numbers disagree with its own headers -- a group's next
        // inode number sits before where the packed layout puts it -- so it
        // cannot be asked which inodes are free.
        let Some(image) = fresh() else {
            eprintln!("skipping: no unpacked xfs_writable.img");
            return;
        };
        let image = image.to_string_lossy().into_owned();
        let bytes = std::fs::read(&image).expect("the unpacked image");
        let mut reader = std::io::BufReader::new(std::fs::File::open(&image).unwrap());
        let sb = Sb::from(&mut reader);
        let bs = sb.sb_blocksize as usize;
        let fetch = |block: u32| -> FsResult<Box<[u8]>> {
            let at = block as usize * bs;
            Ok(bytes[at..at + bs].to_vec().into_boxed_slice())
        };

        for agno in 0..sb.agcount() {
            let at = sb.ag_header_offset(agno, Sb::AGI_SECTOR) as usize;
            let agi = Agi::from_bytes(bytes[at..at + bs].to_vec(), sb.has_crc())
                .expect("a group inode header");
            if agi.inode_count() == 0 {
                continue;
            }
            let ranges = ranges_in_tree(agi.inobt_root(), fetch).expect("the used ranges");
            let free: u64 = ranges.iter().map(|r| u64::from(r.free_count)).sum();
            assert_eq!(
                free,
                agi.free_inodes(),
                "group {agno}: the ranges hold {free} free inodes and the header says {}",
                agi.free_inodes()
            );
            assert!(
                ranges.windows(2).all(|w| w[0].start < w[1].start),
                "group {agno}: the ranges are not in order"
            );
        }
    }

    /// A leaf reads as the ranges `xfs_db` prints.
    #[test]
    fn a_leaf_reads_as_xfs_db_prints_it() {
        if !crate::libxfuse::alloc::have_xfs_db() {
            eprintln!("skipping: no xfs_db to check against");
            return;
        }
        if golden().is_none() {
            eprintln!("skipping: no unpacked xfsv4.img");
            return;
        }
        let leaf = 6u32;
        let node = read(leaf).expect("a leaf of the inode b-tree");
        assert!(node.is_leaf(), "the block does not read as a leaf");
        let ranges = node.ranges().expect("ranges");
        assert_eq!(ranges.len(), node.numrecs() as usize);

        let printed: Vec<(u64, u32, u64)> = shown(leaf, "recs")
            .expect("xfs_db prints the ranges")
            .iter()
            .flat_map(|l| bracketed(l))
            .collect::<Vec<u64>>()
            .chunks(3)
            .filter_map(|c| match c {
                // The third field is a sixty-four bit mask, and the tool prints
                // it as one; a reader that took it as thirty-two bits would
                // disagree with the tool here, which is the point.
                [a, b, c] => Some((*a, *b as u32, *c)),
                _ => None,
            })
            .collect();
        assert_eq!(
            ranges.len(),
            printed.len(),
            "range count disagrees with xfs_db"
        );
        for (ours, theirs) in ranges.iter().zip(printed.iter()) {
            assert_eq!(
                (ours.start, ours.free_count, ours.free),
                *theirs,
                "a range disagrees with xfs_db"
            );
        }
    }

    /// A leaf's records cost twelve bytes each and an interior node's children
    /// cost eight, so the two hold different numbers of them -- and a reader
    /// that used one figure for both would run off the end of a node.
    #[test]
    fn the_two_node_shapes_hold_different_numbers_of_records() {
        assert_eq!(InobtNode::leaf_capacity(512, false), 31);
        assert_eq!(InobtNode::interior_capacity(512, false), 62);
        // Which is what the tool's own numbering shows: the root of the tree in
        // this image holds two children and its key array is sized for sixty-two.
        if golden().is_some() {
            let node = read(12).expect("the inode b-tree root");
            assert!(
                node.numrecs() as usize <= node.children().expect("children").len(),
                "more records than the node can hold"
            );
        }
    }
}

#[cfg(test)]
mod chunk_rec_tests {
    use super::chunk_rec::{Freecount, Kind, FREE, SIZE, STARTINO, UNION};

    /// The two layouts of the same sixteen bytes, and the one thing that differs.
    #[test]
    fn the_free_count_is_four_bytes_in_one_and_one_in_the_other() {
        let mut normal = vec![0u8; SIZE];
        let mut sparse = vec![0u8; SIZE];
        normal[STARTINO..STARTINO + 4].copy_from_slice(&100u32.to_be_bytes());
        sparse[STARTINO..STARTINO + 4].copy_from_slice(&100u32.to_be_bytes());

        <Kind as Freecount>::set_freecount(&mut normal, Kind::Normal, 61);
        <Kind as Freecount>::set_freecount(&mut sparse, Kind::Sparse, 61);

        assert_eq!(<Kind as Freecount>::freecount(&normal, Kind::Normal), 61);
        assert_eq!(<Kind as Freecount>::freecount(&sparse, Kind::Sparse), 61);

        // And the trap is subtler than it looks: with a **zero** hole mask and a
        // count under 256, reading a sparse record as a normal one returns the
        // right answer, because the one-byte count sits where the four-byte field
        // would have ended.  A first attempt at this test asserted they must differ
        // and they did not -- which is exactly how the bug survives.
        assert_eq!(
            <Kind as Freecount>::freecount(&sparse, Kind::Normal),
            61,
            "with no holes the two readings agree, which is how this defect hides"
        );

        // A sparse chunk is sparse because it has **holes**, and that is what breaks
        // it: the hole mask is now non-zero, so reading it as the high bytes of a
        // four-byte count gives a number nobody intended.
        sparse[UNION] = 0x01; // one hole
        <Kind as Freecount>::set_freecount(&mut sparse, Kind::Sparse, 61);
        assert_eq!(<Kind as Freecount>::freecount(&sparse, Kind::Sparse), 61);
        assert_ne!(
            <Kind as Freecount>::freecount(&sparse, Kind::Normal),
            61,
            "with holes present, reading a sparse record as normal must not agree"
        );
    }

    /// The free mask is one 64-bit word, which is why a chunk is 64 inodes.
    #[test]
    fn the_mask_is_one_word_at_a_fixed_offset() {
        let mut rec = [0u8; SIZE];
        assert_eq!(FREE, 8);
        assert_eq!(SIZE - FREE, 8, "the mask is exactly the rest of the record");
        // 64 inodes need 64 bits and have them, which is why there is no wider form.
        assert_eq!(u64::BITS, 64);
        rec[FREE..].copy_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(
            u64::from_be_bytes(rec[FREE..].try_into().unwrap()),
            u64::MAX
        );
        let _ = UNION;
    }
}
