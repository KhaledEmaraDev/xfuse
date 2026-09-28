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
//! Access to the image that holds the file system.
//!
//! A [`BlockDevice`] is the only thing in this program that is allowed to touch
//! the image file.  It exists to make three promises that the write path
//! depends on:
//!
//! * **Positional access only.**  Every operation takes an explicit byte
//!   offset, so there is no hidden file cursor whose position two different
//!   parts of the file system could disagree about.  Read-ahead and write-back
//!   decisions belong to the layers above.
//! * **No buffering.**  A block is either on the image or in a
//!   [`BlockCache`](crate::libxfuse::block_cache::BlockCache) that has already
//!   decided what to do with it.  Because the device itself never remembers
//!   anything, the file system cannot end up with two independent copies of one
//!   physical block disagreeing with each other.
//! * **Explicit access mode.**  A read-write image is opened read-write.  A
//!   read-only mount cannot write even by accident, because the underlying file
//!   descriptor was never opened for writing.
//!
//! The device is addressed in raw byte offsets.  Translating an XFS file system
//! block number into a byte offset is the job of [`Sb`](crate::libxfuse::sb::Sb),
//! which knows the file system geometry.

use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::fs::{FileExt, MetadataExt},
    path::Path,
};

use cfg_if::cfg_if;
use tracing::warn;

#[cfg(target_os = "freebsd")]
mod ffi {
    nix::ioctl_read! {
        /// get the size of the entire device in bytes.  this should be a multiple of the sector
        /// size.
        diocgmediasize, 'd', 129, nix::libc::off_t
    }

    nix::ioctl_read! {
        /// Get the sector size of the device in bytes.  The sector size is the smallest unit of
        /// data which can be transferred from this device.  Usually this is a power of 2 but it
        /// might not be (i.e. CDROM audio).
        diocgsectorsize, b'd', 128, u32
    }
}

#[cfg(target_os = "linux")]
mod ffi {
    nix::ioctl_read! {
        /// Get the size of the entire device in bytes.
        blkgetsize64, 0x12, 114, u64
    }
}

/// May the file system modify the image?
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    /// The image may only be read.  This is the default for every mount.
    ReadOnly,
    /// The image may be modified.  Any mutation that reaches the device will
    /// be written through to the image.
    ReadWrite,
}

impl Access {
    /// Can this access mode write to the image?
    pub const fn is_writable(self) -> bool {
        matches!(self, Access::ReadWrite)
    }
}

/// A handle to the image holding the file system.
///
/// The handle is not buffered and not thread-safe by itself; it is cheap to
/// clone through an `Arc`, and all of its methods take `&self` because the
/// underlying positional I/O does not disturb a shared cursor.
#[derive(Debug)]
pub struct BlockDevice {
    file:       File,
    /// Size of the image in bytes.  It should not change while mounted.
    size:       u64,
    /// The smallest unit of I/O the device can perform.
    sectorsize: usize,
    access:     Access,
}

impl BlockDevice {
    /// Open the image at `path` with the given access mode.
    pub fn open(path: &Path, access: Access) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(access.is_writable())
            .open(path)?;
        let sectorsize = Self::file_sectorsize(&file);
        let size = Self::file_mediasize(&file);
        Ok(Self {
            file,
            size,
            sectorsize,
            access,
        })
    }

    /// The size of the image in bytes.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// The device's sector size, which is the granularity of the smallest I/O
    /// the device can perform.  File system blocks are always a multiple of it.
    pub const fn sectorsize(&self) -> usize {
        self.sectorsize
    }

    /// Can this device be written to?
    pub const fn is_writable(&self) -> bool {
        self.access.is_writable()
    }

    /// Fill `buf` with the bytes stored at `offset`.
    ///
    /// The whole buffer must be inside the image.  A short read is reported as
    /// an error rather than silently returning fewer bytes, because a file
    /// system structure that cannot be read in full is a damaged file system,
    /// and the caller must not continue as if the missing tail were zeroes.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.check_range(offset, buf.len())?;
        self.file.read_exact_at(buf, offset)?;
        Ok(())
    }

    /// Store `buf` at `offset`.
    ///
    /// The whole buffer must be inside the image.  Writing to a read-only device
    /// is refused before any part of the buffer reaches the image.
    pub fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        if !self.access.is_writable() {
            return Err(io::Error::from_raw_os_error(libc::EROFS));
        }
        self.check_range(offset, buf.len())?;
        self.file.write_all_at(buf, offset)?;
        Ok(())
    }

    /// Push anything the operating system is still holding on to out to the
    /// underlying storage.
    pub fn flush(&self) -> io::Result<()> {
        if !self.access.is_writable() {
            return Ok(());
        }
        self.file.sync_data()
    }

    /// Reject an access that runs off the end of the image, so that a bug shows
    /// up as an error at the point of the access rather than as a short read
    /// somewhere else.
    fn check_range(&self, offset: u64, len: usize) -> io::Result<()> {
        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        if end > self.size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("access at offset {offset} length {len} is beyond the end of the image"),
            ));
        }
        Ok(())
    }

    fn file_mediasize(f: &File) -> u64 {
        use std::os::{fd::AsRawFd, unix::fs::FileTypeExt};

        let md = f.metadata().unwrap();
        let ft = md.file_type();
        if ft.is_block_device() || ft.is_char_device() {
            cfg_if! {
                if #[cfg(target_os = "freebsd")] {
                    let mut mediasize = std::mem::MaybeUninit::<i64>::uninit();
                    unsafe {
                        // This ioctl is always safe
                        ffi::diocgmediasize(f.as_raw_fd(), mediasize.as_mut_ptr()).unwrap();
                        mediasize.assume_init() as u64
                    }
                } else if #[cfg(target_os = "linux")] {
                    let mut mediasize = std::mem::MaybeUninit::<u64>::uninit();
                    unsafe {
                        // This ioctl is always safe
                        ffi::blkgetsize64(f.as_raw_fd(), mediasize.as_mut_ptr()).unwrap();
                        mediasize.assume_init()
                    }
                } else {
                    warn!("No mediasize ioctl is supported on this operating system");
                    0
                }
            }
        } else if ft.is_file() {
            md.size()
        } else {
            warn!("Trying to use a {:?} as a real-time device", ft);
            0
        }
    }

    fn file_sectorsize(f: &File) -> usize {
        let md = f.metadata().unwrap();
        cfg_if! {
            if #[cfg(target_os = "freebsd")] {
                use std::os::{
                    fd::AsRawFd,
                    unix::fs::FileTypeExt
                };

                let ft = md.file_type();
                if ft.is_block_device() || ft.is_char_device() {
                    let mut sectorsize = std::mem::MaybeUninit::<u32>::uninit();
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
}

#[cfg(test)]
mod t {
    use super::*;

    /// A 1 MiB image file with a recognisable pattern in it.
    fn image() -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        f.as_file().set_len(1 << 20).unwrap();
        let dev = BlockDevice::open(f.path(), Access::ReadWrite).unwrap();
        for block in 0..64u64 {
            let mut buf = vec![0u8; dev.sectorsize()];
            buf[0..8].copy_from_slice(&(block * 7).to_be_bytes());
            dev.write_at(&buf, block * dev.sectorsize() as u64).unwrap();
        }
        dev.flush().unwrap();
        f
    }

    #[test]
    fn size_and_sectorsize() {
        let f = image();
        let dev = BlockDevice::open(f.path(), Access::ReadOnly).unwrap();
        assert_eq!(dev.size(), 1 << 20);
        assert!(dev.sectorsize() > 0);
        assert!(!dev.is_writable());
    }

    /// A random access read must return the bytes that were stored at that
    /// offset, in any order.
    #[test]
    fn random_read() {
        let f = image();
        let dev = BlockDevice::open(f.path(), Access::ReadOnly).unwrap();
        let ss = dev.sectorsize() as u64;
        for block in [63u64, 0, 31, 7, 40] {
            let mut buf = vec![0u8; ss as usize];
            dev.read_at(&mut buf, block * ss).unwrap();
            assert_eq!(&buf[0..8], &(block * 7u64).to_be_bytes());
            assert!(buf[8..].iter().all(|b| *b == 0));
        }
    }

    /// A partial update must leave the rest of the block alone.
    #[test]
    fn partial_write() {
        let f = image();
        let dev = BlockDevice::open(f.path(), Access::ReadWrite).unwrap();
        let ss = dev.sectorsize() as u64;
        dev.write_at(b"hello", 4 * ss + 1).unwrap();
        dev.flush().unwrap();

        let mut buf = vec![0u8; ss as usize];
        dev.read_at(&mut buf, 4 * ss).unwrap();
        // The marker for block 4 is its last byte, so everything else in the
        // block must still be the zero that `image` wrote.
        assert_eq!(buf[0], 0);
        assert_eq!(&buf[1..6], b"hello");
        assert_eq!(buf[6], 0);
        assert_eq!(buf[7], (4u64 * 7).to_be_bytes()[7]);
        assert!(buf[8..].iter().all(|b| *b == 0));

        // The neighbouring blocks are untouched.
        dev.read_at(&mut buf, 5 * ss).unwrap();
        assert_eq!(buf[7], (5u64 * 7).to_be_bytes()[7]);
        assert!(buf[..7].iter().all(|b| *b == 0));
        assert!(buf[8..].iter().all(|b| *b == 0));
    }

    /// A read that would run off the end of the image must fail, rather than
    /// quietly returning zeroes for the missing part.
    #[test]
    fn read_past_end() {
        let f = image();
        let dev = BlockDevice::open(f.path(), Access::ReadOnly).unwrap();
        let ss = dev.sectorsize() as u64;
        let mut buf = vec![0u8; ss as usize];
        assert!(dev.read_at(&mut buf, dev.size() - ss + 1).is_err());
        assert!(dev.read_at(&mut buf, dev.size() + ss).is_err());
    }

    /// Writing past the end of the image must fail too.
    #[test]
    fn write_past_end() {
        let f = image();
        let dev = BlockDevice::open(f.path(), Access::ReadWrite).unwrap();
        let ss = dev.sectorsize() as usize;
        let buf = vec![0u8; ss];
        assert!(dev.write_at(&buf, dev.size() - (ss as u64) + 1).is_err());
        assert!(dev.write_at(&buf, dev.size()).is_err());
    }

    /// A read-only device must refuse to write, and must refuse with EROFS so
    /// that the error can be handed straight back to FUSE.
    #[test]
    fn read_only_refuses_writes() {
        let f = image();
        let dev = BlockDevice::open(f.path(), Access::ReadOnly).unwrap();
        let err = dev.write_at(b"nope", 0).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EROFS));
    }
}
