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
//! Transactions: the only way the file system changes the image.
//!
//! # The rule
//!
//! Nothing above this module writes to the device.  A file system operation
//! that needs to change an inode, an extent list, or a file's data says so by
//! asking a [`Transaction`] to do it, and the change becomes real when the
//! transaction commits.  That is what keeps "several blocks change together"
//! from turning into "the first three blocks are on the image and the fourth
//! is not".
//!
//! # The shape of a change
//!
//! ```text
//! let mut tx = Transaction::begin(&mut volume);
//! tx.write_data(ino, offset, data)?;      // file data, via the block cache
//! tx.modify_inode(ino, |raw| { ... })?;   // inode, via the block cache
//! tx.commit()?;                           // now the image has both, together
//! ```
//!
//! # Commit strategies
//!
//! The interface hides *how* a commit becomes durable, because that is expected
//! to change:
//!
//! * [`CommitMode::ReadOnly`] is the default and refuses every change with
//!   `EROFS`.  A read-only mount never writes, whatever the operation asked
//!   for.
//! * [`CommitMode::Direct`] writes the changed blocks straight to the image.
//!   It is **not crash safe**: if the machine loses power in the middle, the
//!   image is left with some of the operation applied and the rest missing.  It
//!   exists so that the operations above this layer can be written and tested
//!   before the journal exists, and it is reachable only behind
//!   `--experimental-rw`.
//!
//! When the journal arrives it becomes a third strategy behind the same
//! `commit`, and nothing above this module changes.

use std::sync::Arc;

use tracing::debug;

use super::{
    block_cache::BlockCache,
    block_device::BlockDevice,
    error::{FsError, FsResult},
    sb::Sb,
};

/// How a committed transaction reaches the image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitMode {
    /// Refuse all changes.  This is what a read-only mount uses.
    ReadOnly,
    /// Write the changed blocks straight to the image.  Experimental: this is
    /// not crash safe and must not be described as such.
    Direct,
}

impl CommitMode {
    /// Can this mode change the image at all?
    pub const fn is_writable(self) -> bool {
        matches!(self, CommitMode::Direct)
    }
}

/// What a transaction is allowed to touch.
///
/// The transaction borrows the device and the block cache for its lifetime, so
/// that a running operation is the only writer of the block cache.  That is a
/// deliberate simplification: the first version of the write path serialises
/// its transactions rather than interleaving them, and it is much easier to
/// prove correct.
pub struct Transaction<'a> {
    device:  &'a BlockDevice,
    cache:   &'a mut BlockCache,
    sb:      &'a Sb,
    mode:    CommitMode,
    /// Whether the device in use is the real-time device.  Real-time blocks
    /// live on a separate image, so a transaction has to know which one it is
    /// writing to.
    realtime: bool,
    done:    bool,
}

impl<'a> Transaction<'a> {
    /// Begin a transaction.
    pub fn begin(
        device: &'a BlockDevice,
        cache: &'a mut BlockCache,
        sb: &'a Sb,
        mode: CommitMode,
    ) -> Self {
        Self {
            device,
            cache,
            sb,
            mode,
            realtime: false,
            done: false,
        }
    }

    /// A transaction that writes to the real-time device rather than to the
    /// data device.  The block size and the rest of the geometry are the same;
    /// only the image differs, and that difference is in the device handle.
    pub fn begin_realtime(
        device: &'a BlockDevice,
        cache: &'a mut BlockCache,
        sb: &'a Sb,
        mode: CommitMode,
    ) -> Self {
        let mut tx = Self::begin(device, cache, sb, mode);
        tx.realtime = true;
        tx
    }

    /// The commit mode this transaction was created with.
    pub const fn mode(&self) -> CommitMode {
        self.mode
    }

    /// The superblock, so that callers do not have to carry a second reference
    /// to it around.
    pub const fn sb(&self) -> &Sb {
        self.sb
    }

    /// The size, in bytes, of one file system block.
    pub const fn blocksize(&self) -> u64 {
        self.sb.sb_blocksize as u64
    }

    /// The byte offset within the image at which file system block `block`
    /// starts, taking the real-time device into account.
    pub fn block_offset(&self, block: u64) -> u64 {
        if self.realtime {
            self.sb.fsb_to_offset_rt(block)
        } else {
            self.sb.fsb_to_offset(block)
        }
    }

    fn refuse(&self, what: &str) -> FsResult<()> {
        if self.mode.is_writable() {
            Ok(())
        } else {
            Err(FsError::read_only(format!(
                "{what} was attempted on a read-only file system"
            )))
        }
    }

    /// Read a file system block's contents, going through the cache.
    pub fn read_block(&mut self, block: u64) -> FsResult<Vec<u8>> {
        let offset = self.block_offset(block);
        Ok(self.cache.read_block(self.device, offset)?.to_vec())
    }

    /// Get a file system block for modification, together with the byte offset
    /// it lives at, which callers need in order to write into the middle of it.
    pub fn modify_block(&mut self, block: u64) -> FsResult<(u64, &mut [u8])> {
        self.refuse("modifying a block")?;
        let offset = self.block_offset(block);
        let data = self.cache.modify_block(self.device, offset)?;
        Ok((offset, data))
    }

    /// Get the block that contains byte `offset` in the image, for
    /// modification, together with that byte's offset within the block.
    pub fn modify_block_at(&mut self, offset: u64) -> FsResult<(u64, u64, &mut [u8])> {
        let bs = self.blocksize();
        let start = offset - (offset % bs);
        let within = offset - start;
        let data = self.cache.modify_block(self.device, start)?;
        Ok((start, within, data))
    }

    /// Overwrite a run of bytes in the image.
    ///
    /// `offset` need not be block aligned.  Whole blocks are replaced, and the
    /// blocks at the two ends are read, patched, and written back, so the
    /// bytes outside the range are preserved.  This is the primitive behind
    /// overwriting part of a file.
    pub fn write_bytes(&mut self, offset: u64, data: &[u8]) -> FsResult<()> {
        self.refuse("writing data")?;
        let bs = self.blocksize();
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| FsError::invalid(libc::EFBIG, "write runs off the end of the image"))?;
        if end > self.device.size() {
            return Err(FsError::invalid(
                libc::EFBIG,
                format!("write at {offset} length {} runs past the end of the image", data.len()),
            ));
        }
        if data.is_empty() {
            return Ok(());
        }

        // A write that is not block aligned needs read-modify-write at both
        // ends.  The middle, if any, can go in whole.
        let mut written = 0u64;
        if offset % bs != 0 {
            let first_len = std::cmp::min(bs - (offset % bs), data.len() as u64);
            self.cache
                .write_within_block(self.device, offset, &data[..first_len as usize])?;
            written = first_len;
        }
        while written + bs <= data.len() as u64 {
            let start = offset + written;
            self.cache
                .write_within_block(self.device, start, &data[written as usize..(written + bs) as usize])?;
            written += bs;
        }
        if (written as usize) < data.len() {
            self.cache.write_within_block(
                self.device,
                offset + written,
                &data[written as usize..],
            )?;
        }
        Ok(())
    }

    /// Overwrite a run of blocks belonging to a file's data, given in image
    /// offsets.  [`write_bytes`](Self::write_bytes) is the general form; this
    /// one says out loud that the bytes belong to file data, and refuses to
    /// write to the real-time device until that device is supported for
    /// writing.
    pub fn write_data(&mut self, offset: u64, data: &[u8]) -> FsResult<()> {
        if self.realtime {
            return Err(FsError::unsupported(
                "writing file data to a real-time device",
            ));
        }
        self.write_bytes(offset, data)
    }

    /// Write the changes out.
    pub fn commit(mut self) -> FsResult<()> {
        self.done = true;
        if !self.mode.is_writable() {
            if self.cache.dirty_count() > 0 {
                return Err(FsError::read_only(
                    "a transaction accumulated changes on a read-only file system",
                ));
            }
            return Ok(());
        }
        self.cache.commit(self.device)
    }

    /// Throw the changes away.
    pub fn abort(mut self) {
        self.done = true;
        self.cache.abort();
    }

    /// Has this transaction been committed or aborted?  A transaction that has
    /// not been finished must not be allowed to commit later by accident.
    pub const fn is_done(&self) -> bool {
        self.done
    }
}

impl Drop for Transaction<'_> {
    /// A transaction that is dropped without being committed or aborted is a
    /// bug in the caller, and the only safe thing to do is to discard its
    /// changes.
    fn drop(&mut self) {
        if !self.done {
            debug!("dropping an unfinished transaction; discarding its changes");
            self.cache.abort();
        }
    }
}

/// Owns everything a transaction needs, and is shared by the operations that
/// run them.
///
/// A `Volume` keeps one of these.  The device is shared rather than borrowed so
/// that the read path, which has been running since long before the write path
/// existed, and the write path look at the same image.
#[derive(Debug)]
pub struct TransactionContext {
    device: Arc<BlockDevice>,
    cache:  BlockCache,
    sb:     Sb,
    mode:   CommitMode,
}

impl TransactionContext {
    /// Build a context for one image.
    pub fn new(device: Arc<BlockDevice>, sb: &Sb, mode: CommitMode) -> Self {
        let limit = default_cache_limit(sb.sb_blocksize);
        Self {
            device,
            cache: BlockCache::new(sb.sb_blocksize as usize, limit),
            sb: sb.clone(),
            mode,
        }
    }

    /// The commit mode that new transactions inherit.
    pub const fn mode(&self) -> CommitMode {
        self.mode
    }

    /// Start a transaction.
    pub fn begin(&mut self) -> Transaction<'_> {
        Transaction::begin(&self.device, &mut self.cache, &self.sb, self.mode)
    }

    /// The block cache, for tests and for the invalidation that a commit needs.
    pub const fn cache(&self) -> &BlockCache {
        &self.cache
    }
}

/// How many blocks a transaction may hold at once, chosen so that a
/// multi-block operation fits comfortably without letting a huge file exhaust
/// memory.  64 MiB of file system blocks.
fn default_cache_limit(blocksize: u32) -> usize {
    const BUDGET: usize = 64 << 20;
    (BUDGET / blocksize as usize).max(8)
}

#[cfg(test)]
mod t {
    use super::*;
    use crate::libxfuse::block_device::Access;

    /// A stand-in superblock for a 512-byte-block file system with two
    /// allocation groups of 1024 blocks each.
    fn sb() -> Sb {
        let mut sb: Sb = unsafe { std::mem::zeroed() };
        // Only the geometry that the transaction actually uses is filled in
        // here; the rest of the superblock is irrelevant to block addressing.
        sb.sb_blocksize = 512;
        sb.sb_blocklog = 9;
        sb.sb_agblocks = 1024;
        sb.sb_agblklog = 10;
        sb
    }

    struct Harness {
        _f:   tempfile::NamedTempFile,
        dev:  BlockDevice,
        cache: BlockCache,
        sb:   Sb,
    }

    fn harness(writable: bool) -> Harness {
        let f = tempfile::NamedTempFile::new().unwrap();
        f.as_file().set_len(512 * 4096).unwrap();
        let access = if writable {
            Access::ReadWrite
        } else {
            Access::ReadOnly
        };
        Harness {
            dev: BlockDevice::open(f.path(), access).unwrap(),
            cache: BlockCache::new(512, 64),
            sb: sb(),
            _f: f,
        }
    }

    /// A write that covers whole blocks must land in the image.
    #[test]
    fn whole_block_write() {
        let mut h = harness(true);
        let mut tx = Transaction::begin(&h.dev, &mut h.cache, &h.sb, CommitMode::Direct);
        tx.write_bytes(0, &vec![0xa5u8; 1024]).unwrap();
        tx.commit().unwrap();

        let mut buf = vec![0u8; 1024];
        h.dev.read_at(&mut buf, 0).unwrap();
        assert!(buf.iter().all(|b| *b == 0xa5));
    }

    /// A write that covers parts of blocks must keep the untouched bytes.
    #[test]
    fn partial_block_write() {
        let mut h = harness(true);
        let mut original = vec![0u8; 1024];
        original.fill(0x5a);
        h.dev.write_at(&original, 0).unwrap();
        h.dev.flush().unwrap();

        let mut tx = Transaction::begin(&h.dev, &mut h.cache, &h.sb, CommitMode::Direct);
        tx.write_bytes(500, b"hello world").unwrap();
        tx.commit().unwrap();

        let mut buf = vec![0u8; 1024];
        h.dev.read_at(&mut buf, 0).unwrap();
        assert!(buf[..500].iter().all(|b| *b == 0x5a));
        assert_eq!(&buf[500..511], b"hello world");
        assert!(buf[511..].iter().all(|b| *b == 0x5a));
    }

    /// The same bytes written twice through one transaction must be seen twice,
    /// because the cache holds the first version.
    #[test]
    fn repeated_writes_are_serialized() {
        let mut h = harness(true);
        let mut tx = Transaction::begin(&h.dev, &mut h.cache, &h.sb, CommitMode::Direct);
        tx.write_bytes(0, b"aaaa").unwrap();
        tx.write_bytes(0, b"bbbb").unwrap();
        tx.commit().unwrap();

        let mut buf = vec![0u8; 512];
        h.dev.read_at(&mut buf, 0).unwrap();
        assert_eq!(&buf[..4], b"bbbb");
    }

    /// A read-only transaction must refuse every change.
    #[test]
    fn read_only_refuses() {
        let mut h = harness(false);
        let mut tx = Transaction::begin(&h.dev, &mut h.cache, &h.sb, CommitMode::ReadOnly);
        let err = tx.write_bytes(0, b"nope").unwrap_err();
        assert_eq!(err.errno(), libc::EROFS);
        assert!(tx.read_block(0).is_ok());
        tx.abort();
    }

    /// An aborted transaction must leave the image untouched.
    #[test]
    fn abort_leaves_image_alone() {
        let mut h = harness(true);
        let mut original = vec![0u8; 512];
        original.fill(0x11);
        h.dev.write_at(&original, 0).unwrap();
        h.dev.flush().unwrap();

        let mut tx = Transaction::begin(&h.dev, &mut h.cache, &h.sb, CommitMode::Direct);
        tx.write_bytes(0, b"junkjunk").unwrap();
        tx.abort();

        let mut buf = vec![0u8; 512];
        h.dev.read_at(&mut buf, 0).unwrap();
        assert!(buf.iter().all(|b| *b == 0x11));
    }

    /// Dropping a transaction without committing must also leave the image
    /// alone, because the caller may have lost track of it.
    #[test]
    fn drop_discards() {
        let mut h = harness(true);
        let mut original = vec![0u8; 512];
        original.fill(0x22);
        h.dev.write_at(&original, 0).unwrap();
        h.dev.flush().unwrap();

        {
            let mut tx = Transaction::begin(&h.dev, &mut h.cache, &h.sb, CommitMode::Direct);
            tx.write_bytes(0, b"junkjunk").unwrap();
        }
        let mut buf = vec![0u8; 512];
        h.dev.read_at(&mut buf, 0).unwrap();
        assert!(buf.iter().all(|b| *b == 0x22));
        assert_eq!(h.cache.dirty_count(), 0);
    }

    /// The image's block numbering must be translated the same way the read
    /// path translates it.
    #[test]
    fn block_addressing_matches_read_path() {
        let mut h = harness(true);
        let mut tx = Transaction::begin(&h.dev, &mut h.cache, &h.sb, CommitMode::Direct);
        // Block 1024 is the first block of allocation group 1.
        assert_eq!(tx.block_offset(1024), h.sb.fsb_to_offset(1024));
        assert_eq!(tx.block_offset(0), 0);
        tx.write_bytes(tx.block_offset(1024), b"ag1").unwrap();
        tx.commit().unwrap();

        let mut buf = vec![0u8; 512];
        h.dev
            .read_at(&mut buf, h.sb.fsb_to_offset(1024))
            .unwrap();
        assert_eq!(&buf[..3], b"ag1");
    }

    /// Writing past the end of the image must be refused.
    #[test]
    fn write_past_end_refused() {
        let mut h = harness(true);
        let mut tx = Transaction::begin(&h.dev, &mut h.cache, &h.sb, CommitMode::Direct);
        let err = tx.write_bytes(h.dev.size(), b"x").unwrap_err();
        assert_eq!(err.errno(), libc::EFBIG);
    }

    /// The real-time device cannot be written to yet, and saying so is better
    /// than writing to the wrong image.
    #[test]
    fn realtime_writes_refused() {
        let mut h = harness(true);
        let mut tx = Transaction::begin_realtime(&h.dev, &mut h.cache, &h.sb, CommitMode::Direct);
        let err = tx.write_data(0, b"x").unwrap_err();
        assert_eq!(err.errno(), libc::ENOSYS);
    }
}
