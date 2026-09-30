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
//! Handing out blocks from a group, through a transaction.
//!
//! # What this is
//!
//! [`FreeSpace`](super::free_space::FreeSpace) knows how to take blocks out of a
//! group's two free space trees, and is given a group in memory because that is
//! what makes an allocation testable.  This is the other half: the group as it is
//! actually stored, where every block read and written goes through a
//! transaction, and where the group's own header has to be told what happened.
//!
//! # Why all of it goes through one transaction
//!
//! An allocation changes three things: a leaf in each of the two free space
//! trees, and the group header's count of what is left.  All three have to move
//! together, because a group header that says a block is free when its tree says
//! it is taken is a block that gets handed out twice.  Nothing here writes to the
//! image: the caller begins a transaction, this fills it, and committing makes
//! the whole change real at once.  A transaction that is never committed leaves
//! the image exactly as it was.
//!
//! # Blocks, sectors, and why the header is read as bytes
//!
//! A group header sits at a fixed *sector* within the group, and a sector is
//! the file system's basic block, which is not always a whole file system block:
//! with 1 KiB blocks and 512-byte sectors the header starts half way through a
//! block.  In the first group that half of the block is the superblock.  So the
//! header is read and written as bytes at a computed offset, and the block around
//! it is never written as a whole -- writing it would take the superblock with
//! it.
//!
//! # What this does not do
//!
//! It does not merge a leaf that has fallen below half full, so a group with
//! such a leaf reports itself full for runs that leaf would have served.  It
//! does not reclaim a block, which means inserting a run and merging it with its
//! neighbours, so freeing a block belongs with the operation that frees one.  And
//! it does not put anything in the group's free list, which in the images tested
//! here is not initialised at all.

use super::{
    agf::Agf,
    free_space::{FreeRun, FreeSpace, GroupBlocks, GroupGeometry},
};
use crate::libxfuse::{
    error::{FsError, FsResult},
    sb::Sb,
    transaction::Transaction,
};

/// A group's blocks, as the tree operations see them, over a transaction.
///
/// A read goes through the transaction's block cache, so a node that was changed
/// earlier in the same transaction reads back with the change, which is what
/// makes read-modify-write of one node work.  A write marks the block changed and
/// nothing more.
pub struct TransactionBlocks<'a, 't> {
    transaction: &'a mut Transaction<'t>,
    /// The image offset of the group, so that a group-relative block number can be
    /// turned into one.
    ag_offset:   u64,
    blocksize:   usize,
}

impl<'a, 't> TransactionBlocks<'a, 't> {
    /// Take a transaction and the group it is working on.
    pub fn new(transaction: &'a mut Transaction<'t>, ag_offset: u64, blocksize: usize) -> Self {
        Self {
            transaction,
            ag_offset,
            blocksize,
        }
    }

    /// Where a group-relative block is in the image.
    fn offset_of(&self, block: u32) -> FsResult<u64> {
        self.ag_offset
            .checked_add(u64::from(block) * self.blocksize as u64)
            .ok_or_else(|| {
                FsError::invalid(libc::EFBIG, "a block number past the end of the image")
            })
    }
}

impl GroupBlocks for TransactionBlocks<'_, '_> {
    fn get(&mut self, block: u32) -> FsResult<Box<[u8]>> {
        let bytes = self
            .transaction
            .read_bytes(self.offset_of(block)?, self.blocksize)?;
        Ok(bytes.into_boxed_slice())
    }

    fn put(&mut self, block: u32, bytes: Box<[u8]>) -> FsResult<()> {
        self.transaction.write_bytes(self.offset_of(block)?, &bytes)
    }
}

/// Where a group's header is in the image.
///
/// Sectors 1, 2 and 3 of a group are the group file, the group inode header and
/// the group free list, so the file is in the second sector of the group.
pub fn agf_offset(sb: &Sb, agno: u32) -> u64 {
    sb.ag_header_offset(agno, Sb::AGF_SECTOR)
}

/// Read a group's header.
pub fn read_agf(transaction: &mut Transaction<'_>, sb: &Sb, agno: u32) -> FsResult<Agf> {
    let at = agf_offset(sb, agno);
    let bytes = transaction
        .read_bytes(at, sb.sb_blocksize as usize)
        .map_err(|_| FsError::corrupt(format!("allocation group {agno} has no header")))?;
    let agf = Agf::from_bytes(bytes, sb.sb_blocksize as usize)?;
    agf.check_usable(sb)?;
    Ok(agf)
}

/// Write a group's header, with its checksum if it has one.
pub fn write_agf(
    transaction: &mut Transaction<'_>,
    sb: &Sb,
    agno: u32,
    agf: &mut Agf,
) -> FsResult<()> {
    agf.update_crc();
    let bytes = agf.as_bytes().to_vec();
    transaction.write_bytes(agf_offset(sb, agno), &bytes)
}

/// The two numbers a group's header keeps: blocks free, and the longest run.
///
/// They are there so that a group can be passed over without being read, and a
/// group that cannot satisfy a request from its longest run certainly cannot
/// satisfy it at all.
pub fn group_summaries(
    transaction: &mut Transaction<'_>,
    sb: &Sb,
    agno: u32,
) -> FsResult<(u32, u32)> {
    let agf = read_agf(transaction, sb, agno)?;
    Ok((agf.free_blocks(), agf.longest_free()))
}

/// Take `count` blocks out of one group, through one transaction.
///
/// Returns `None` when the group cannot spare them, which is not a failure: a
/// caller allocating from a file asks each group in turn, and the one that can
/// serve is the one that serves.  Returns `ENOSPC` only when the caller has run
/// out of groups.
///
/// The two free space trees, and the group's header, are all changed inside the
/// caller's transaction, so the caller commits it and the whole change becomes
/// real at once -- or it does not, and the image is as it was.
pub fn allocate_in_group(
    transaction: &mut Transaction<'_>,
    sb: &Sb,
    agno: u32,
    count: u32,
) -> FsResult<Option<FreeRun>> {
    if count == 0 {
        return Err(FsError::invalid(libc::EINVAL, "no blocks to allocate"));
    }
    let mut agf = read_agf(transaction, sb, agno)?;
    // A group that cannot spare the blocks from its own summary certainly
    // cannot spare them, and there is no point walking its trees to find out.
    if (agf.free_blocks() as u64) < count as u64 || agf.longest_free() < count {
        return Ok(None);
    }
    let geometry = GroupGeometry::new(sb.sb_agblocks, sb.has_crc(), true);
    let (by_block_root, by_size_root) = (agf.block_btree_root(), agf.extent_btree_root());
    let mut store =
        TransactionBlocks::new(transaction, sb.ag_offset(agno), sb.sb_blocksize as usize);
    let mut space = FreeSpace::new(&mut store, geometry, by_block_root, by_size_root);
    let Some(run) = space.allocate(count)? else {
        return Ok(None);
    };
    // The group's own numbers move with its trees, in the same transaction, or
    // the header and the trees would describe different file systems.
    let (free, longest) = space.summaries()?;
    let free = u32::try_from(free).map_err(|_| FsError::Corrupt {
        what: "a group claims more free blocks than a file system can hold".into(),
    })?;
    agf.set_free_blocks(free);
    agf.set_longest_free(longest);
    write_agf(transaction, sb, agno, &mut agf)?;
    Ok(Some(run))
}

/// Take `count` blocks, trying one group after another.
///
/// This is the shape an allocation has: a file asks for blocks, the groups are
/// tried in order, and the first that can serve does.  A group that reports
/// itself full for a run it cannot spare is passed over, which is why the caller
/// gets `ENOSPC` only when every group has said no.
pub fn allocate(
    transaction: &mut Transaction<'_>,
    sb: &Sb,
    agno: u32,
    count: u32,
) -> FsResult<FreeRun> {
    for agno in agno..sb.agcount() {
        if let Some(run) = allocate_in_group(transaction, sb, agno, count)? {
            return Ok(run);
        }
    }
    Err(FsError::NoSpace)
}

/// The end-to-end tests, over a real image file and a real transaction.
///
/// Everything below the allocator is the code a write would use -- a block
/// device, a block cache, a transaction -- so what these check is the part that
/// cannot be checked by inspecting structs: that an allocation comes out the
/// other end of a transaction as a coherent change.
#[cfg(test)]
mod t {
    use std::{os::unix::fs::FileExt as _, sync::Arc};

    use byteorder::{BigEndian, ByteOrder};

    use super::{allocate_in_group, read_agf};
    use crate::libxfuse::{
        alloc::{
            agf::XFS_AGF_MAGIC,
            free_space::{
                FreeRun,
                FreeSpaceNode,
                ENTRY_LEN,
                PTR_LEN,
                RECORD_LEN,
                XFS_ABTB_MAGIC,
                XFS_ABTC_MAGIC,
            },
        },
        block_cache::BlockCache,
        block_device::{Access, BlockDevice},
        sb::Sb,
        transaction::{CommitMode, Transaction},
    };

    const BS: usize = 512;
    const AGBLOCKS: u32 = 4096;
    const RUNS_PER_LEAF: usize = 32;

    /// A superblock describing the image the tests build: one group of
    /// [`AGBLOCKS`] blocks, 512-byte blocks and sectors, and no checksums.
    fn sb() -> Sb {
        // The fields the allocator reads, and nothing else: a superblock is
        // decoded by reading an image, and these tests do not have one yet, so
        // this stands in for the geometry.
        crate::libxfuse::sb::Sb::for_tests(BS as u32, BS as u16, AGBLOCKS, 1, 256)
    }

    fn leaf(magic: u32, runs: &[(u32, u32)]) -> Vec<u8> {
        let mut b = vec![0u8; BS];
        BigEndian::write_u32(&mut b[0..], magic);
        BigEndian::write_u16(&mut b[4..], 0);
        BigEndian::write_u16(&mut b[6..], runs.len() as u16);
        BigEndian::write_u32(&mut b[8..], u32::MAX);
        BigEndian::write_u32(&mut b[12..], u32::MAX);
        for (i, (at, len)) in runs.iter().enumerate() {
            BigEndian::write_u32(&mut b[16 + RECORD_LEN * i..], *at);
            BigEndian::write_u32(&mut b[16 + RECORD_LEN * i + 4..], *len);
        }
        b
    }

    fn interior(magic: u32, keys: &[u32], leaves: &[u32]) -> Vec<u8> {
        let mut b = vec![0u8; BS];
        BigEndian::write_u32(&mut b[0..], magic);
        BigEndian::write_u16(&mut b[4..], 1);
        BigEndian::write_u16(&mut b[6..], leaves.len() as u16);
        BigEndian::write_u32(&mut b[8..], u32::MAX);
        BigEndian::write_u32(&mut b[12..], u32::MAX);
        let max = (BS - 16) / ENTRY_LEN;
        for (i, l) in leaves.iter().enumerate() {
            BigEndian::write_u32(&mut b[16 + RECORD_LEN * i..], keys[i]);
            BigEndian::write_u32(&mut b[16 + RECORD_LEN * i + 4..], 0);
            BigEndian::write_u32(&mut b[16 + max * RECORD_LEN + PTR_LEN * i..], *l);
        }
        b
    }

    /// An image file holding one group whose free space is `runs`.
    fn image_with_group(runs: &[(u32, u32)]) -> (tempfile::NamedTempFile, Sb) {
        let sb = sb();
        let f = tempfile::NamedTempFile::new().unwrap();
        f.as_file().set_len(AGBLOCKS as u64 * BS as u64).unwrap();
        let dev = BlockDevice::open(f.path(), Access::ReadWrite).unwrap();
        let chunks: Vec<&[(u32, u32)]> = runs.chunks(RUNS_PER_LEAF).collect();
        let mut by_block_leaves = Vec::new();
        let mut by_size_leaves = Vec::new();
        let mut block_keys = Vec::new();
        let mut size_keys = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let block_leaf = 10 + 2 * i as u32;
            let size_leaf = 11 + 2 * i as u32;
            dev.write_at(
                &leaf(XFS_ABTB_MAGIC, chunk),
                u64::from(block_leaf) * BS as u64,
            )
            .unwrap();
            let mut ordered: Vec<(u32, u32)> = chunk.to_vec();
            ordered.sort_by_key(|(at, len)| (*len, *at));
            dev.write_at(
                &leaf(XFS_ABTC_MAGIC, &ordered),
                u64::from(size_leaf) * BS as u64,
            )
            .unwrap();
            by_block_leaves.push(block_leaf);
            by_size_leaves.push(size_leaf);
            block_keys.push(chunk[0].0);
            size_keys.push(ordered[0].1);
        }
        dev.write_at(
            &interior(XFS_ABTB_MAGIC, &block_keys, &by_block_leaves),
            4 * BS as u64,
        )
        .unwrap();
        dev.write_at(
            &interior(XFS_ABTC_MAGIC, &size_keys, &by_size_leaves),
            5 * BS as u64,
        )
        .unwrap();
        // The group header, at sector 1 of the group: block 1 here.
        let mut agf = vec![0u8; BS];
        BigEndian::write_u32(&mut agf[0..], XFS_AGF_MAGIC);
        BigEndian::write_u32(&mut agf[4..], 1); // version
        BigEndian::write_u32(&mut agf[8..], 0); // sequence
        BigEndian::write_u32(&mut agf[12..], AGBLOCKS);
        BigEndian::write_u32(&mut agf[16..], 4); // block btree root
        BigEndian::write_u32(&mut agf[20..], 5); // length btree root
        BigEndian::write_u32(&mut agf[52..], runs.iter().map(|(_, l)| l).sum::<u32>());
        BigEndian::write_u32(
            &mut agf[56..],
            runs.iter().map(|(_, l)| *l).max().unwrap_or(0),
        );
        dev.write_at(&agf, BS as u64).unwrap();
        dev.flush().unwrap();
        (f, sb)
    }

    fn free_runs_on_image(image: &std::path::Path, by_block: bool) -> Vec<FreeRun> {
        // Walk the image directly, without a transaction, so that what is read
        // is what was actually written.
        let file = std::fs::File::open(image).unwrap();
        let read = |bno: u32| -> Vec<u8> {
            let mut buf = vec![0u8; BS];
            file.read_exact_at(&mut buf, u64::from(bno) * BS as u64)
                .unwrap();
            buf
        };
        // The two trees have their own roots, as the group header records them.
        let root = if by_block { 4u32 } else { 5u32 };
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(bno) = stack.pop() {
            let bytes = read(bno);
            let node =
                FreeSpaceNode::from_bytes(bytes, false, by_block).expect("a node of the tree");
            if node.is_leaf() {
                out.extend(node.runs().expect("runs"));
            } else {
                for child in node.children().expect("children") {
                    stack.push(child);
                }
            }
        }
        out.sort_by_key(|r| (r.start, r.len));
        out
    }

    /// An allocation that is committed leaves the image coherent: both trees have
    /// given up the blocks, they still agree with each other, and the group's
    /// own count follows.
    #[test]
    fn a_committed_allocation_is_a_coherent_change() {
        let runs: Vec<(u32, u32)> = (0..96u32).map(|i| (2000 + i * 20, 4 + i % 7)).collect();
        let total: u32 = runs.iter().map(|(_, l)| l).sum();
        let (f, sb) = image_with_group(&runs);
        let device = Arc::new(BlockDevice::open(f.path(), Access::ReadWrite).unwrap());
        let mut cache = BlockCache::new(BS, 256);

        let taken = {
            let mut tx = Transaction::begin(&device, &mut cache, &sb, CommitMode::Direct);
            let run = allocate_in_group(&mut tx, &sb, 0, 8)
                .expect("allocate")
                .expect("the group can spare eight blocks");
            assert_eq!(run.len, 8);
            // The group's own numbers move with its trees, in the same
            // transaction.
            let agf = read_agf(&mut tx, &sb, 0).expect("read the header");
            assert_eq!(agf.free_blocks(), total - 8);
            assert_eq!(agf.longest_free(), 10);
            tx.commit().expect("commit");
            run
        };

        // What the image holds now, read back without a transaction.
        let by_block = free_runs_on_image(f.path(), true);
        let by_size = free_runs_on_image(f.path(), false);
        assert_eq!(
            by_block, by_size,
            "the two trees on the image no longer agree"
        );
        let free: u32 = by_block.iter().map(|r| r.len).sum();
        assert_eq!(free, total - taken.len);
        // And the blocks that went are not free any more.
        for run in by_block.iter() {
            let (at, len) = (run.start, run.len);
            for block in at..at + len {
                assert!(
                    block < taken.start || block >= taken.start + taken.len,
                    "block {block} was allocated and is still free"
                );
            }
        }
    }

    /// A transaction that is never committed leaves the image exactly as it
    /// was, which is what makes an allocation something that can be undone by
    /// refusing the operation.
    #[test]
    fn an_uncommitted_allocation_changes_nothing() {
        let runs: Vec<(u32, u32)> = (0..40u32).map(|i| (2000 + i * 20, 4 + i % 7)).collect();
        let (f, sb) = image_with_group(&runs);
        let before = std::fs::read(f.path()).unwrap();
        let device = Arc::new(BlockDevice::open(f.path(), Access::ReadWrite).unwrap());
        let mut cache = BlockCache::new(BS, 256);
        {
            let mut tx = Transaction::begin(&device, &mut cache, &sb, CommitMode::Direct);
            assert!(allocate_in_group(&mut tx, &sb, 0, 8)
                .expect("allocate")
                .is_some());
            // Dropped without committing, which is what a refused operation does.
        }
        assert_eq!(
            std::fs::read(f.path()).unwrap(),
            before,
            "the image changed"
        );
    }

    /// A request the group cannot satisfy from its own summary is not worth
    /// walking its trees for.
    #[test]
    fn a_request_the_group_cannot_serve_is_refused_quickly() {
        let runs: Vec<(u32, u32)> = (0..8u32).map(|i| (2000 + i * 20, 4 + i % 7)).collect();
        let (f, sb) = image_with_group(&runs);
        let device = Arc::new(BlockDevice::open(f.path(), Access::ReadWrite).unwrap());
        let mut cache = BlockCache::new(BS, 256);
        let total: u32 = runs.iter().map(|(_, l)| l).sum();
        let mut tx = Transaction::begin(&device, &mut cache, &sb, CommitMode::Direct);
        assert!(
            allocate_in_group(&mut tx, &sb, 0, total + 1)
                .expect("allocate")
                .is_none(),
            "a group with fewer free blocks than that must say no"
        );
        assert!(
            allocate_in_group(&mut tx, &sb, 0, 0).is_err(),
            "zero blocks"
        );
    }
}
