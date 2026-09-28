/*
 * BSD 2-Clause License
 *
 * Copyright (c) 2024, Benjamin Stürz
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
use std::{
    io::{self, BufRead, Read, Result as IoResult, Seek, SeekFrom},
    path::Path,
    sync::Arc,
};

use bincode_next::{de::read::Reader, error::DecodeError};

use super::block_device::{Access, BlockDevice};

/// A forward-only, seekable window onto a [`BlockDevice`].
///
/// This is the read side's view of the image: the file system's parsers want a
/// `Seek` + `Read` stream they can decode structures from, while the write side
/// wants random positional access.  Rather than teach the parsers about
/// positional I/O, the reader keeps one readahead window on top of the device.
///
/// The window is *read only*.  It is never the home of a modified block: the
/// write path goes through the block cache and the transaction, and calls
/// [`BlockReader::invalidate`] after it commits, so no stale copy of a physical
/// block can survive a mutation of the image.
#[derive(Debug)]
pub struct BlockReader {
    device:     Arc<BlockDevice>,
    block:      Vec<u8>,
    /// The next byte to be returned out of `block`.
    idx:        usize,
    /// The image offset at which the contents of `block` begin.
    start:      u64,
    /// The image offset of the next byte a read would return.  This is tracked
    /// separately from `start + idx` because the window can be thrown away or
    /// resized, and because the reader's position must stay put when that
    /// happens.
    pos:        u64,
    /// The absolute minimum that we can read in any operation
    sectorsize: usize,
    /// File's size in bytes.  It should not change while mounted.
    pub size:   u64,
}

impl BlockReader {
    /// Open the image at `path` for reading.
    pub fn open(path: &Path) -> IoResult<Self> {
        Ok(Self::from_device(Arc::new(BlockDevice::open(
            path,
            Access::ReadOnly,
        )?)))
    }

    /// Build a reader over an already opened device.
    pub fn from_device(device: Arc<BlockDevice>) -> Self {
        let sectorsize = device.sectorsize();
        let size = device.size();
        let block = vec![0u8; sectorsize];
        Self {
            device,
            block,
            idx: sectorsize,
            start: 0,
            pos: 0,
            sectorsize,
            size,
        }
    }

    /// The device that this reader is looking at.  Cloning it gives another
    /// handle onto the very same image, which is how the write path reaches the
    /// same bytes.
    pub fn device(&self) -> Arc<BlockDevice> {
        Arc::clone(&self.device)
    }

    /// The image offset of the next byte that a read would return.
    pub const fn position(&self) -> u64 {
        self.pos
    }

    /// Throw the readahead window away, so that the next read comes from the
    /// image.
    ///
    /// The write path calls this after it commits, because the window may
    /// contain bytes that the commit has just replaced.  The reader's position
    /// does not move.
    pub fn invalidate(&mut self) {
        self.idx = self.block.len();
    }

    /// Fill the window, aligning it on the reader's current position.
    ///
    /// Aligning means the window always starts at a multiple of its own size,
    /// which is what keeps every device access on a sector boundary no matter
    /// where in the image the caller asked to read.
    fn refill(&mut self) -> IoResult<()> {
        self.start = self.pos - (self.pos % self.block.len() as u64);
        self.device.read_at(&mut self.block, self.start)?;
        self.idx = (self.pos - self.start) as usize;
        Ok(())
    }

    fn buffered(&self) -> usize {
        self.block.len() - self.idx
    }

    fn refill_if_empty(&mut self) -> IoResult<()> {
        if self.buffered() == 0 {
            self.refill()?;
        }
        Ok(())
    }

    /// The current size of the buffer
    pub fn bufsize(&self) -> usize {
        self.block.len()
    }

    /// Change the reader's bufsize.  It will be rounded up to a multiple of the sectorsize.
    /// After this operation, the window is empty and will be refilled, starting
    /// again at the reader's current position.
    pub fn set_bufsize(&mut self, bufsize: usize) {
        let remainder = bufsize & (self.sectorsize - 1);
        let bufsize = if remainder > 0 {
            bufsize + self.sectorsize - remainder
        } else {
            bufsize
        };
        self.block.resize(bufsize, 0u8);
        self.idx = bufsize;
    }
}

impl Read for BlockReader {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        self.refill_if_empty()?;
        let num = buf.len().min(self.buffered());
        let buf = &mut buf[0..num];
        buf.copy_from_slice(&self.block[self.idx..(self.idx + num)]);
        self.idx += num;
        self.pos += num as u64;
        Ok(num)
    }
}

impl BufRead for BlockReader {
    fn fill_buf(&mut self) -> IoResult<&[u8]> {
        self.refill_if_empty()?;
        Ok(&self.block[self.idx..])
    }

    fn consume(&mut self, amt: usize) {
        assert!(amt <= self.buffered());
        self.idx += amt;
    }
}

impl Seek for BlockReader {
    fn seek(&mut self, pos: SeekFrom) -> IoResult<u64> {
        let target = match pos {
            SeekFrom::Start(target) => target,
            SeekFrom::Current(offset) => {
                let cur = self.position();
                if offset < 0 {
                    cur.checked_sub(offset.unsigned_abs())
                } else {
                    cur.checked_add(offset as u64)
                }
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?
            }
            SeekFrom::End(_) => todo!("SeekFrom::End()"),
        };

        let end = self.start + self.block.len() as u64;
        if (self.start..end).contains(&target) {
            // The window already holds what was asked for; move along it.
            self.idx = (target - self.start) as usize;
            self.pos = target;
            return Ok(target);
        }

        self.pos = target;
        self.refill()?;
        Ok(target)
    }
}

impl Reader for BlockReader {
    fn read(&mut self, bytes: &mut [u8]) -> Result<(), DecodeError> {
        self.read_exact(bytes).map_err(|inner| DecodeError::Io {
            inner,
            additional: bytes.len(),
        })
    }

    fn peek_read(&mut self, n: usize) -> Option<&[u8]> {
        self.block[self.idx..].get(..n)
    }

    fn consume(&mut self, n: usize) {
        <Self as std::io::BufRead>::consume(self, n);
    }
}

#[cfg(test)]
mod t {
    use super::*;

    mod seek {
        use super::*;

        const FSIZE: u64 = 1 << 20;

        fn harness() -> BlockReader {
            let f = tempfile::NamedTempFile::new().unwrap();
            f.as_file().set_len(FSIZE).unwrap();
            let br = BlockReader::open(f.path()).unwrap();
            let bs = br.bufsize();
            assert!(FSIZE > 2 * bs as u64);
            br
        }

        /// Seeking to SeekFrom::Current(0) should be a no-op when the target is
        /// already inside the window.
        #[test]
        #[allow(clippy::seek_from_current)] // That's the whole point of the test
        fn current_0() {
            let mut br = harness();
            let bs = br.bufsize() as u64;
            let pos = bs + (bs >> 2);
            br.seek(SeekFrom::Start(pos)).unwrap();
            let idx = br.idx;

            br.seek(SeekFrom::Current(0)).unwrap();
            assert_eq!(pos as u64, br.position());
            assert_eq!(idx, br.idx);
        }

        /// Seek to a negative offset from current
        #[test]
        fn current_neg() {
            let mut br = harness();
            let bs = br.bufsize() as u64;
            let initial = bs + (bs >> 2);
            br.seek(SeekFrom::Start(initial)).unwrap();
            let idx = br.idx as u64;

            br.seek(SeekFrom::Current(-1)).unwrap();
            assert_eq!(initial - 1, br.position());
            assert_eq!(idx - 1, br.idx as u64);
        }

        /// Seek to a negative absolute offset using SeekFrom::Current
        #[test]
        fn current_neg_neg() {
            let mut br = harness();
            let bs = br.bufsize() as u64;
            let initial = bs + (bs >> 2);
            br.seek(SeekFrom::Start(initial)).unwrap();

            let e = br.seek(SeekFrom::Current(-2 * initial as i64)).unwrap_err();
            assert_eq!(libc::EINVAL, e.raw_os_error().unwrap());
        }

        /// Seek to a small positive offset from current, within the current block
        #[test]
        fn current_pos_incr() {
            let mut br = harness();
            let bs = br.bufsize() as u64;
            let initial = bs + (bs >> 2);
            br.seek(SeekFrom::Start(initial)).unwrap();
            let idx = br.idx as u64;

            br.seek(SeekFrom::Current(1)).unwrap();
            assert_eq!(initial + 1, br.position());
            assert_eq!(idx + 1, br.idx as u64);
        }

        /// Seek to a large positive offset from current
        #[test]
        fn current_pos_large() {
            let mut br = harness();
            let bs = br.bufsize() as u64;
            let initial = bs + (bs >> 2);
            br.seek(SeekFrom::Start(initial)).unwrap();
            let idx = br.idx as u64;

            br.seek(SeekFrom::Current(bs as i64)).unwrap();
            assert_eq!(initial + bs, br.position());
            // The window realigned itself around the new position, so the
            // reader sits at the same place within the window as it did before.
            assert_eq!((initial + bs) % bs, br.idx as u64);
        }

        /// The window must always start on a multiple of its own size, no
        /// matter how the caller got there.
        #[test]
        fn window_alignment() {
            let mut br = harness();
            let bs = br.bufsize() as u64;
            for pos in [0u64, 1, bs - 1, bs, bs + 1, 3 * bs + 17, 7 * bs] {
                br.seek(SeekFrom::Start(pos)).unwrap();
                assert_eq!(pos, br.position());
                assert_eq!(0, br.start % bs);
            }
        }

        /// Changing the buffer size empties the window but must not move the
        /// reader.  The next read refills starting from the same position.
        #[test]
        fn set_bufsize_keeps_position() {
            let mut br = harness();
            let bs = br.bufsize() as u64;
            br.seek(SeekFrom::Start(bs + 1)).unwrap();
            br.set_bufsize(bs as usize * 2);
            assert_eq!(bs + 1, br.position());
            assert_eq!(0, br.buffered());
        }

        /// Invalidating the window must not move the reader either.
        #[test]
        fn invalidate_keeps_position() {
            let mut br = harness();
            let bs = br.bufsize() as u64;
            br.seek(SeekFrom::Start(bs + 3)).unwrap();
            br.invalidate();
            assert_eq!(bs + 3, br.position());
            assert_eq!(0, br.buffered());
        }
    }
}
