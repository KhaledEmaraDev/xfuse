/*
 * BSD 2-Clause License
 *
 * Copyright (c) 2026, the xfuse authors
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
//! A cache of file system blocks held between the device and the transaction.
//!
//! # Why this layer exists
//!
//! A file system operation almost never writes whole blocks.  Overwriting ten
//! bytes in the middle of a block means reading that block, changing ten bytes,
//! and writing the block back; doing the same to a hundred blocks in a row must
//! not read each one from the device more than once.  The cache is where those
//! blocks live while the operation runs.
//!
//! # What a cached block knows about itself
//!
//! Every cached block carries an explicit [`BlockState`].  The state is what
//! makes it safe to change a block: a block that has only ever been read is
//! `Clean` and may be dropped or overwritten freely, while a block that an
//! operation has changed is `Dirty` and must be written back before the
//! transaction that changed it ends.  `Logged` and `Committed` are the two
//! states that the journal will need once it exists; nothing produces them yet,
//! but they are defined now so that adding the journal does not change this
//! interface.
//!
//! # What the cache deliberately does not do
//!
//! The cache does not decide *when* to write anything.  That belongs to the
//! transaction, which knows when the operation is complete and which blocks the
//! operation is responsible for.  The cache also never evicts a dirty block:
//! dropping a change that nobody has written back would lose data, so an
//! over-full cache first evicts clean blocks, and only complains if every
//! resident block is dirty.

use std::collections::{HashMap, VecDeque};

use super::{
    block_device::BlockDevice,
    error::{FsError, FsResult},
};

/// What the cache knows about a resident block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockState {
    /// The block matches the image, and may be dropped or replaced.
    Clean,
    /// An operation has changed the block.  It must be written back before that
    /// operation ends.
    Dirty,
    /// The block has been written to the journal and the on-disk copy may now
    /// be stale.  Produced once the journal exists.
    Logged,
    /// The block has been written back to the image.  Produced once the journal
    /// exists.
    Committed,
}

impl BlockState {
    /// Does this state mean the block must not be dropped?
    pub const fn is_dirty(self) -> bool {
        matches!(self, BlockState::Dirty | BlockState::Logged)
    }
}

#[derive(Debug)]
struct CachedBlock {
    state: BlockState,
    data:  Vec<u8>,
}

/// A cache of fixed-size file system blocks, keyed by their byte offset in the
/// image.
#[derive(Debug)]
pub struct BlockCache {
    blocksize: usize,
    blocks:    HashMap<u64, CachedBlock>,
    /// Insertion order, so that the cache can evict the block that has been
    /// resident the longest.
    order:     VecDeque<u64>,
    /// The most blocks that may be resident at once.  A dirty block always
    /// counts against this limit.
    limit:     usize,
}

impl BlockCache {
    /// Create a cache for blocks of `blocksize` bytes that will hold at most
    /// `limit` blocks.
    pub fn new(blocksize: usize, limit: usize) -> Self {
        assert!(
            blocksize.is_power_of_two(),
            "block size {blocksize} is not a power of two"
        );
        Self {
            blocksize,
            blocks: HashMap::new(),
            order: VecDeque::new(),
            limit: limit.max(1),
        }
    }

    /// The size, in bytes, of the blocks this cache holds.
    pub const fn blocksize(&self) -> usize {
        self.blocksize
    }

    /// How many blocks are resident, dirty ones included.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Is nothing cached?
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// How many blocks currently hold unsaved changes.
    pub fn dirty_count(&self) -> usize {
        self.blocks.values().filter(|b| b.state.is_dirty()).count()
    }

    /// The state of the block at `offset`, if it is resident.
    pub fn state(&self, offset: u64) -> Option<BlockState> {
        self.blocks.get(&offset).map(|b| b.state)
    }

    /// The offsets of every block that holds unsaved changes, in ascending
    /// order.  Committing in this order keeps writes reproducible.
    pub fn dirty_blocks(&self) -> Vec<u64> {
        let mut dirty: Vec<u64> = self
            .blocks
            .iter()
            .filter(|(_, b)| b.state.is_dirty())
            .map(|(offset, _)| *offset)
            .collect();
        dirty.sort_unstable();
        dirty
    }

    /// The size of the block that contains `offset`, which is the size of every
    /// block.  Spelled out because callers that build offsets from it read
    /// better this way.
    pub const fn block_size(&self) -> u64 {
        self.blocksize as u64
    }

    fn check_aligned(&self, offset: u64) -> FsResult<()> {
        if !offset.is_multiple_of(self.blocksize as u64) {
            return Err(FsError::invalid(
                libc::EINVAL,
                format!("block offset {offset} is not a multiple of the block size"),
            ));
        }
        Ok(())
    }

    /// Make room for one more block, preferring to throw away blocks that hold
    /// nothing worth keeping.
    fn make_room(&mut self) -> FsResult<()> {
        while self.blocks.len() >= self.limit {
            let victim = self
                .order
                .iter()
                .copied()
                .find(|offset| self.blocks.get(offset).is_some_and(|b| !b.state.is_dirty()));
            match victim {
                Some(offset) => {
                    self.blocks.remove(&offset);
                    self.order.retain(|o| *o != offset);
                }
                // Every resident block is dirty.  Dropping one of them would
                // lose a change that nobody has written back.
                None => {
                    return Err(FsError::invalid(
                        libc::ENOSPC,
                        format!(
                            "block cache is full: {} blocks, all of them modified",
                            self.limit
                        ),
                    ))
                }
            }
        }
        Ok(())
    }

    /// Fetch a block, reading it from `device` if it is not resident yet.
    ///
    /// The returned block is marked `Clean` unless it was already resident and
    /// modified, in which case the caller gets the modified contents.  This is
    /// how an operation reads part of a structure it has already changed.
    pub fn read_block(&mut self, device: &BlockDevice, offset: u64) -> FsResult<&[u8]> {
        self.check_aligned(offset)?;
        if !self.blocks.contains_key(&offset) {
            self.make_room()?;
            let mut data = vec![0u8; self.blocksize];
            device.read_at(&mut data, offset)?;
            self.blocks.insert(
                offset,
                CachedBlock {
                    state: BlockState::Clean,
                    data,
                },
            );
            self.order.push_back(offset);
        }
        Ok(&self.blocks.get(&offset).expect("just inserted").data)
    }

    /// Fetch a block for modification.
    ///
    /// The block is read from `device` if it is not resident yet, and it comes
    /// back marked `Dirty`, so that the transaction will write it back.
    pub fn modify_block(&mut self, device: &BlockDevice, offset: u64) -> FsResult<&mut [u8]> {
        self.check_aligned(offset)?;
        if !self.blocks.contains_key(&offset) {
            self.make_room()?;
            let mut data = vec![0u8; self.blocksize];
            device.read_at(&mut data, offset)?;
            self.blocks.insert(
                offset,
                CachedBlock {
                    state: BlockState::Clean,
                    data,
                },
            );
            self.order.push_back(offset);
        }
        let block = self.blocks.get_mut(&offset).expect("just inserted");
        block.state = BlockState::Dirty;
        Ok(&mut block.data)
    }

    /// Replace the whole contents of a block, marking it `Dirty`.
    pub fn write_block(&mut self, device: &BlockDevice, offset: u64, data: &[u8]) -> FsResult<()> {
        if data.len() != self.blocksize {
            return Err(FsError::invalid(
                libc::EINVAL,
                format!(
                    "block write of {} bytes does not fill a {} byte block",
                    data.len(),
                    self.blocksize
                ),
            ));
        }
        let block = self.modify_block(device, offset)?;
        block.copy_from_slice(data);
        Ok(())
    }

    /// Copy `data` into the block at `offset`, which may be unaligned inside
    /// the block.
    ///
    /// The rest of the block is left alone.  This is the primitive behind
    /// partially overwriting a file: the block is fetched for modification, so
    /// the bytes the operation is not touching are the bytes already on the
    /// image.
    pub fn write_within_block(
        &mut self,
        device: &BlockDevice,
        offset: u64,
        data: &[u8],
    ) -> FsResult<()> {
        let start = offset - (offset % self.blocksize as u64);
        let within = usize::try_from(offset - start).expect("offset fits in usize");
        let block = self.modify_block(device, start)?;
        block[within..within + data.len()].copy_from_slice(data);
        Ok(())
    }

    /// Write every modified block back to `device`, in ascending offset order.
    ///
    /// After this returns, the blocks are `Committed` and the device has been
    /// told to push them to the underlying storage.  If a write fails part way
    /// through, the blocks that were written are still marked `Committed` and
    /// the ones that were not are still `Dirty`; the caller is expected to
    /// report the failure and stop touching the file system, because the image
    /// no longer matches the transaction.
    pub fn commit(&mut self, device: &BlockDevice) -> FsResult<()> {
        if !device.is_writable() {
            // Nothing should have been modified in the first place.  Say so
            // loudly rather than pretending the commit worked.
            return Err(FsError::read_only(
                "tried to write back blocks to a read-only image",
            ));
        }
        let mut failed = None;
        for offset in self.dirty_blocks() {
            let block = self.blocks.get(&offset).expect("dirty block is resident");
            if let Err(e) = device.write_at(&block.data, offset) {
                failed = Some(e);
                break;
            }
            if let Some(block) = self.blocks.get_mut(&offset) {
                block.state = BlockState::Committed;
            }
        }
        device.flush()?;
        match failed {
            Some(e) => Err(FsError::Io(e)),
            None => Ok(()),
        }
    }

    /// Forget every unsaved change.
    ///
    /// A block that an operation changed and then abandoned no longer matches
    /// the image, so it has to go: leaving it resident would let a later read
    /// be served bytes that were never written anywhere.  This is what an
    /// aborted transaction calls.
    pub fn abort(&mut self) {
        let doomed: Vec<u64> = self
            .blocks
            .iter()
            .filter(|(_, b)| !matches!(b.state, BlockState::Committed))
            .map(|(offset, _)| *offset)
            .collect();
        for offset in doomed {
            self.blocks.remove(&offset);
        }
        self.order.retain(|o| self.blocks.contains_key(o));
    }

    /// Throw the whole cache away.  The next read comes from the image.
    pub fn invalidate(&mut self) {
        self.blocks.clear();
        self.order.clear();
    }
}

#[cfg(test)]
mod t {
    use super::*;
    use crate::libxfuse::block_device::Access;

    const BS: usize = 512;

    fn device() -> (tempfile::NamedTempFile, BlockDevice) {
        let f = tempfile::NamedTempFile::new().unwrap();
        f.as_file().set_len(BS as u64 * 64).unwrap();
        let dev = BlockDevice::open(f.path(), Access::ReadWrite).unwrap();
        (f, dev)
    }

    /// Distinct blocks must stay distinct, and a block that has not been
    /// written must read back as it was.
    #[test]
    fn round_trip() {
        let (_f, dev) = device();
        let mut cache = BlockCache::new(BS, 8);
        for i in 0..4u64 {
            let offset = i * BS as u64;
            let block = cache.modify_block(&dev, offset).unwrap();
            block.fill(i as u8);
        }
        assert_eq!(cache.dirty_count(), 4);
        cache.commit(&dev).unwrap();
        for i in 0..4u64 {
            assert_eq!(cache.state(i * BS as u64), Some(BlockState::Committed));
        }
        assert_eq!(cache.dirty_count(), 0);

        // After the cache is thrown away, the blocks come back from the image
        // with what was committed.
        cache.invalidate();
        for i in 0..4u64 {
            let data = cache.read_block(&dev, i * BS as u64).unwrap();
            assert!(data.iter().all(|b| *b == i as u8));
            assert_eq!(cache.state(i * BS as u64), Some(BlockState::Clean));
        }
    }

    /// A block must be fetched from the device exactly once, however many times
    /// it is touched.
    #[test]
    fn fetched_once() {
        let (_f, dev) = device();
        let mut cache = BlockCache::new(BS, 8);
        for _ in 0..5 {
            let data = cache.modify_block(&dev, 2 * BS as u64).unwrap();
            data[0] = 0xff;
        }
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.dirty_count(), 1);
    }

    /// A partial update must leave the rest of the block alone, and must be
    /// written back as a whole block.
    #[test]
    fn partial_update() {
        let (_f, dev) = device();
        let mut original = vec![0u8; BS];
        original.fill(0x5a);
        dev.write_at(&original, 0).unwrap();
        dev.flush().unwrap();

        let mut cache = BlockCache::new(BS, 8);
        cache.write_within_block(&dev, 3, b"abcdef").unwrap();
        assert_eq!(cache.state(0), Some(BlockState::Dirty));
        cache.commit(&dev).unwrap();
        cache.invalidate();

        let data = cache.read_block(&dev, 0).unwrap();
        assert_eq!(&data[0..3], &[0x5a, 0x5a, 0x5a]);
        assert_eq!(&data[3..9], b"abcdef");
        assert!(data[9..].iter().all(|b| *b == 0x5a));
    }

    /// An aborted transaction must not leave anything behind that looks like a
    /// saved change.
    #[test]
    fn abort_discards() {
        let (_f, dev) = device();
        let mut cache = BlockCache::new(BS, 8);
        cache.modify_block(&dev, 0).unwrap()[0] = 0xff;
        assert_eq!(cache.dirty_count(), 1);
        cache.abort();
        assert_eq!(cache.dirty_count(), 0);
        assert!(cache.is_empty());

        let data = cache.read_block(&dev, 0).unwrap();
        assert_ne!(data[0], 0xff);
    }

    /// Committing to a read-only image must fail rather than silently succeed.
    #[test]
    fn read_only_commit_fails() {
        let f = tempfile::NamedTempFile::new().unwrap();
        f.as_file().set_len(BS as u64 * 8).unwrap();
        let dev = BlockDevice::open(f.path(), Access::ReadOnly).unwrap();
        let mut cache = BlockCache::new(BS, 8);
        let err = cache.commit(&dev).unwrap_err();
        assert_eq!(err.errno(), libc::EROFS);
    }

    /// A full cache must make room by throwing away clean blocks, and must
    /// refuse to throw away a modified one.
    #[test]
    fn eviction() {
        let (_f, dev) = device();
        let mut cache = BlockCache::new(BS, 2);
        cache.read_block(&dev, 0).unwrap();
        cache.read_block(&dev, BS as u64).unwrap();
        // Both are clean, so the third read may evict the first.
        cache.read_block(&dev, 2 * BS as u64).unwrap();
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.state(0), None);
        assert_eq!(cache.state(2 * BS as u64), Some(BlockState::Clean));

        let mut cache = BlockCache::new(BS, 2);
        cache.modify_block(&dev, 0).unwrap();
        cache.modify_block(&dev, BS as u64).unwrap();
        let err = cache.modify_block(&dev, 2 * BS as u64).unwrap_err();
        assert_eq!(err.errno(), libc::ENOSPC);
    }

    /// Unaligned offsets are a programming error and must be reported.
    #[test]
    fn unaligned_offset_rejected() {
        let (_f, dev) = device();
        let mut cache = BlockCache::new(BS, 8);
        assert!(cache.read_block(&dev, 1).is_err());
        assert!(cache.modify_block(&dev, BS as u64 + 7).is_err());
    }
}
