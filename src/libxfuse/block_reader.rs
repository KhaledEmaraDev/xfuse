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
    fs::File,
    io::{self, BufRead, Read, Result as IoResult, Seek, SeekFrom},
    mem,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::Path,
};

use bincode::{de::read::Reader, error::DecodeError};
use cfg_if::cfg_if;

#[cfg(target_os = "freebsd")]
mod ffi {
    nix::ioctl_read! {
        /// Get the sector size of the device in bytes.  The sector size is the smallest unit of
        /// data which can be transferred from this device.  Usually this is a power of 2 but it
        /// might not be (i.e. CDROM audio).
        diocgsectorsize, b'd', 128, u32
    }
}

#[derive(Debug)]
pub struct BlockReader {
    file:       File,
    block:      Vec<u8>,
    idx:        usize,
    /// The absolute minimum that we can read in any operation
    sectorsize: usize,
}

impl BlockReader {
    fn sectorsize(f: &File) -> usize {
        let md = f.metadata().unwrap();
        cfg_if! {
            if #[cfg(target_os = "freebsd")] {
                use std::os::unix::fs::FileTypeExt;

                let ft = md.file_type();
                if ft.is_block_device() || ft.is_char_device() {
                    let mut sectorsize = mem::MaybeUninit::<u32>::uninit();
                    unsafe {
                        // This ioctl is always safe
                        ffi::diocgsectorsize(f.as_raw_fd(), sectorsize.as_mut_ptr()).unwrap();
                        return sectorsize.assume_init() as usize;
                    }
                }
            }
        }
        md.blksize() as usize
    }

    pub fn open(path: &Path) -> IoResult<Self> {
        let file = File::options().read(true).write(false).open(path)?;

        let sectorsize = Self::sectorsize(&file);
        let block = vec![0u8; sectorsize];
        Ok(Self {
            file,
            block,
            idx: sectorsize,
            sectorsize,
        })
    }

    fn refill(&mut self) -> IoResult<()> {
        self.file.read_exact(&mut self.block)?;
        self.idx = 0;
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
    /// After this operation, the buffer should be considered undefined until the next absolute
    /// Seek operation.
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
        let bs = self.bufsize() as u64;
        match pos {
            SeekFrom::Start(pos) => {
                let real = self.file.seek(SeekFrom::Start(pos / bs * bs))?;
                let rem = pos - real;
                assert!(rem < bs);

                self.refill()?;
                self.idx = rem as usize;

                Ok(real + rem)
            }
            SeekFrom::Current(offset) => {
                let real = self.file.stream_position()?;
                let cur = real - self.block.len() as u64 + self.idx as u64;
                let newidx = offset + self.idx as i64;
                if newidx >= 0 && newidx < self.bufsize() as i64 {
                    // The data is already buffered; just adjust the pointer
                    self.idx = newidx as usize;
                    Ok(real - self.block.len() as u64 + newidx as u64)
                } else if cur as i64 + offset < 0 {
                    Err(io::Error::from_raw_os_error(libc::EINVAL))
                } else {
                    self.seek(SeekFrom::Start((cur as i64 + offset) as u64))
                }
            }
            SeekFrom::End(_) => todo!("SeekFrom::End()"),
        }
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

        /// Seeking to SeekFrom::Current(0) should refill the internal buffer but otherwise be a
        /// no-op.
        #[test]
        #[allow(clippy::seek_from_current)] // That's the whole point of the test
        fn current_0() {
            let mut br = harness();
            let bs = br.bufsize();
            let pos = bs + (bs >> 2);
            br.seek(SeekFrom::Start(pos as u64)).unwrap();
            let idx = br.idx;
            let real_pos = br.file.stream_position().unwrap();

            br.seek(SeekFrom::Current(0)).unwrap();
            assert_eq!(real_pos, br.file.stream_position().unwrap());
            assert_eq!(idx, br.idx);
        }

        /// Seek to a negative offset from current
        #[test]
        fn current_neg() {
            let mut br = harness();
            let bs = br.bufsize();
            let initial = bs + (bs >> 2);
            br.seek(SeekFrom::Start(initial as u64)).unwrap();
            let idx = br.idx as u64;
            let real_pos = br.file.stream_position().unwrap();

            br.seek(SeekFrom::Current(-1)).unwrap();
            assert_eq!(
                real_pos + idx - 1,
                br.file.stream_position().unwrap() + br.idx as u64
            );
        }

        /// Seek to a negative absolute offset using SeekFrom::Current
        #[test]
        fn current_neg_neg() {
            let mut br = harness();
            let bs = br.bufsize();
            let initial = bs + (bs >> 2);
            br.seek(SeekFrom::Start(initial as u64)).unwrap();

            let e = br.seek(SeekFrom::Current(-2 * initial as i64)).unwrap_err();
            assert_eq!(libc::EINVAL, e.raw_os_error().unwrap());
        }

        /// Seek to a small positive offset from current, within the current block
        #[test]
        fn current_pos_incr() {
            let mut br = harness();
            let bs = br.bufsize();
            let initial = bs + (bs >> 2);
            br.seek(SeekFrom::Start(initial as u64)).unwrap();
            let idx = br.idx as u64;
            let real_pos = br.file.stream_position().unwrap();

            br.seek(SeekFrom::Current(1)).unwrap();
            assert_eq!(
                real_pos + idx + 1,
                br.file.stream_position().unwrap() + br.idx as u64
            );
        }

        /// Seek to a large positive offset from current
        #[test]
        fn current_pos_large() {
            let mut br = harness();
            let bs = br.bufsize();
            let initial = bs + (bs >> 2);
            br.seek(SeekFrom::Start(initial as u64)).unwrap();
            let idx = br.idx as u64;
            let real_pos = br.file.stream_position().unwrap();

            br.seek(SeekFrom::Current(bs as i64)).unwrap();
            assert_eq!(
                real_pos + idx + bs as u64,
                br.file.stream_position().unwrap() + br.idx as u64
            );
        }
    }
}
