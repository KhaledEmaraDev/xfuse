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
//! Where a file's data lives.
//!
//! # What this is
//!
//! A file's data fork is a mapping from *logical* blocks — blocks counted from
//! the start of the file — to *physical* blocks in an image.  That mapping can
//! be stored in two ways on disk: directly in the inode, as a list of extent
//! records, or in a B+tree of extent records hanging off the inode.  [`ExtentMap`]
//! is the one place in this program that knows how to answer "where is logical
//! block *n*", whichever of the two representations a file happens to use.
//!
//! The read path and the write path both go through it.  That is not a stylistic
//! preference.  If reading a file and writing a file used two different
//! interpretations of the same extent list, a file could be written to a
//! different block than the one it was read from, and the only symptom would be
//! silent data corruption.
//!
//! # What a hole is
//!
//! Not every logical block has a physical block.  A gap in the mapping is a
//! hole, and reading a hole yields zeroes.  [`ExtentMap::lookup`] reports a hole
//! by returning `None` for the starting block together with the length of the
//! run of blocks that is a hole, which is what lets a reader skip over it and
//! what lets the write path refuse to write into it before it knows how to
//! allocate one.
//!
//! # Unwritten extents
//!
//! A file may own space that it has never written: `posix_fallocate` reserves
//! it, and the extent records say so.  Unwritten extents are not data, so they
//! are not part of the mapping; they read as zeroes just like holes do.  The
//! write path cannot turn one into data either, because doing so changes the
//! extent records, which is phase 9's job.

use std::io::{BufRead, Seek};

use bincode_next::de::read::Reader;

use super::{
    bmbt_rec::{BmbtRec, Bmx},
    btree::{Btree, BtreeRoot},
    definitions::{XfsFileoff, XfsFsblock, XfsFsize},
    sb::Sb,
};

/// The mapping between a file's logical blocks and the image's physical blocks.
#[derive(Debug)]
pub enum ExtentMap {
    /// The extents are stored in the inode itself, which is what
    /// `di_format == extents` means.
    Core { extents: Bmx, size: XfsFsize },
    /// The extents are stored in a B+tree rooted in the inode, which is what
    /// `di_format == btree` means.
    Btree { btree: BtreeRoot, size: XfsFsize },
}

impl ExtentMap {
    /// Build a map from an extent list held in the inode.
    pub fn from_core(extents: Bmx, size: XfsFsize) -> Self {
        ExtentMap::Core { extents, size }
    }

    /// Build a map from a B+tree rooted in the inode.
    pub fn from_btree(btree: BtreeRoot, size: XfsFsize) -> Self {
        ExtentMap::Btree { btree, size }
    }

    /// The size of the file this mapping belongs to, in bytes.
    ///
    /// It is part of the mapping rather than of the inode because it is what
    /// gives a trailing hole its length: a hole that runs to the end of the
    /// file has no extent after it to be bounded by.
    pub const fn size(&self) -> XfsFsize {
        match self {
            ExtentMap::Core { size, .. } | ExtentMap::Btree { size, .. } => *size,
        }
    }

    /// Change the size the mapping is interpreted against.  This does not
    /// allocate anything: a file that grows simply gains a hole.
    pub fn set_size(&mut self, new_size: XfsFsize) {
        match self {
            ExtentMap::Core { size, .. } | ExtentMap::Btree { size, .. } => *size = new_size,
        }
    }

    /// Does this file's mapping live in a B+tree?
    pub const fn is_btree(&self) -> bool {
        matches!(self, ExtentMap::Btree { .. })
    }

    /// The extent records held directly in the inode, if that is where this
    /// file's mapping lives.
    ///
    /// The write path needs this to re-encode the inode.  A file whose extents
    /// are in a B+tree cannot be re-encoded from here; changing it is phase 9's
    /// job.
    pub fn core_extents(&self) -> Option<&[BmbtRec]> {
        match self {
            ExtentMap::Core { extents, .. } => Some(extents.extents()),
            ExtentMap::Btree { .. } => None,
        }
    }

    /// The extent records held directly in the inode, for modification.
    pub fn core_extents_mut(&mut self) -> Option<&mut Vec<BmbtRec>> {
        match self {
            ExtentMap::Core { extents, .. } => Some(extents.extents_mut()),
            ExtentMap::Btree { .. } => None,
        }
    }

    /// Look up the extent containing logical block `block`.
    ///
    /// Returns the physical block where that run of blocks starts — `None` if
    /// the block is in a hole — and how many blocks the run is long, counting
    /// from `block` itself.  For a hole that reaches the end of the file, the
    /// length runs to the end of the file.
    pub fn lookup<R>(
        &self,
        buf_reader: &mut R,
        sb: &Sb,
        block: XfsFileoff,
    ) -> Result<(Option<XfsFsblock>, u64), i32>
    where
        R: BufRead + Reader + Seek,
    {
        let (start, len) = match self {
            ExtentMap::Core { extents, .. } => extents.get_extent(block),
            ExtentMap::Btree { btree, .. } => btree.map_block(buf_reader.by_ref(), block)?,
        };
        let file_blocks = (self.size().max(0) as u64).div_ceil(sb.sb_blocksize as u64);
        // A run that reaches the end of the file has no extent to be bounded
        // by, so the end of the file bounds it instead.
        let len = len.unwrap_or_else(|| file_blocks.saturating_sub(block));
        Ok((start, len))
    }

    /// Like `lseek(2)`, but only for `SEEK_DATA` and `SEEK_HOLE`.
    pub fn lseek<R>(&self, buf_reader: &mut R, offset: u64, whence: i32) -> Result<u64, i32>
    where
        R: BufRead + Reader + Seek,
    {
        match self {
            ExtentMap::Core { extents, .. } => extents.lseek(offset, whence),
            ExtentMap::Btree { btree, .. } => btree.lseek(buf_reader.by_ref(), offset, whence),
        }
    }
}

#[cfg(test)]
mod t {
    use std::io::{BufReader, Cursor};

    use super::*;

    fn rec(startoff: u64, startblock: u64, blockcount: u64) -> BmbtRec {
        BmbtRec {
            br_startoff:   startoff,
            br_startblock: startblock,
            br_blockcount: blockcount,
            br_flag:       false,
        }
    }

    fn sb() -> Sb {
        let mut sb: Sb = unsafe { std::mem::zeroed() };
        sb.sb_blocksize = 512;
        sb.sb_blocklog = 9;
        sb
    }

    /// A reader that satisfies the bound the extent map needs, for the cases
    /// where the mapping is held in the inode and nothing is read from it.
    fn no_device() -> BufReader<Cursor<Vec<u8>>> {
        BufReader::new(Cursor::new(Vec::new()))
    }

    fn map(extents: &[(u64, u64, u64)], size: i64) -> ExtentMap {
        let bmx = Bmx::from(extents.iter().copied().map(|(o, b, l)| rec(o, b, l)));
        ExtentMap::from_core(bmx, size)
    }

    /// A block inside an extent must resolve to the right physical block.
    #[test]
    fn lookup_in_extent() {
        let m = map(&[(0, 100, 4), (6, 200, 2)], 8 * 512);
        let sb = sb();
        let mut r = no_device();
        assert_eq!(m.lookup(&mut r, &sb, 0), Ok((Some(100), 4)));
        assert_eq!(m.lookup(&mut r, &sb, 3), Ok((Some(103), 1)));
        assert_eq!(m.lookup(&mut r, &sb, 6), Ok((Some(200), 2)));
    }

    /// A hole must be reported as a hole, with a length that runs to the next
    /// extent.  Blocks 0 and 1 are before the first extent, and block 4 is in
    /// the second one.
    #[test]
    fn lookup_in_hole() {
        let m = map(&[(2, 100, 4), (6, 200, 2)], 8 * 512);
        let sb = sb();
        let mut r = no_device();
        assert_eq!(m.lookup(&mut r, &sb, 0), Ok((None, 2)));
        assert_eq!(m.lookup(&mut r, &sb, 1), Ok((None, 1)));
        assert_eq!(m.lookup(&mut r, &sb, 4), Ok((Some(102), 2)));
    }

    /// A hole at the end of the file has no extent after it, so the end of the
    /// file bounds it.
    #[test]
    fn lookup_in_trailing_hole() {
        let m = map(&[(0, 100, 4)], 16 * 512);
        let sb = sb();
        let mut r = no_device();
        assert_eq!(m.lookup(&mut r, &sb, 4), Ok((None, 12)));
    }

    /// A preallocated but unwritten extent is not data.
    #[test]
    fn unwritten_extent_is_a_hole() {
        let mut prealloc = rec(2, 100, 4);
        prealloc.br_flag = true;
        let bmx = Bmx::from([rec(0, 50, 1), prealloc]);
        let m = ExtentMap::from_core(bmx, 16 * 512);
        let sb = sb();
        let mut r = no_device();
        assert_eq!(m.lookup(&mut r, &sb, 2), Ok((None, 14)));
    }

    /// Growing a file must not invent an extent; it only lengthens the hole at
    /// the end.
    #[test]
    fn set_size_does_not_allocate() {
        let mut m = map(&[(0, 100, 4)], 4 * 512);
        let sb = sb();
        let mut r = no_device();
        assert_eq!(m.lookup(&mut r, &sb, 4), Ok((None, 0)));
        m.set_size(16 * 512);
        assert_eq!(m.size(), 16 * 512);
        assert_eq!(m.lookup(&mut r, &sb, 4), Ok((None, 12)));
        assert_eq!(m.core_extents().unwrap().len(), 1);
    }

    /// `SEEK_DATA` and `SEEK_HOLE` must skip over holes.
    ///
    /// The extent list's hole scan reads the block size out of the process-wide
    /// superblock, so this test installs one.  Whichever test installs it first
    /// decides, which is why every test here asks for the installed one rather
    /// than assuming its own.
    #[test]
    fn lseek_skips_holes() {
        super::super::volume::SUPERBLOCK.get_or_init(sb);
        let sb = super::super::volume::SUPERBLOCK.get().unwrap();
        let bs = sb.sb_blocklog;
        let m = map(&[(2, 100, 4)], 16 * 512);
        let data = 2 << bs;
        assert_eq!(m.lseek(&mut no_device(), 0, libc::SEEK_DATA), Ok(data));
        assert_eq!(m.lseek(&mut no_device(), 0, libc::SEEK_HOLE), Ok(0));
        assert_eq!(
            m.lseek(&mut no_device(), data, libc::SEEK_HOLE),
            Ok(data + (4 << bs))
        );
    }
}
