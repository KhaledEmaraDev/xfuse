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
use std::{
    collections::HashMap,
    ffi::OsStr,
    io::Read,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime},
};

use fuser::{
    consts::{
        FOPEN_CACHE_DIR,
        FOPEN_KEEP_CACHE,
        FUSE_ASYNC_READ,
        FUSE_EXPORT_SUPPORT,
        FUSE_NO_OPENDIR_SUPPORT,
        FUSE_NO_OPEN_SUPPORT,
    },
    Filesystem,
    KernelConfig,
    ReplyAttr,
    ReplyDirectory,
    ReplyEmpty,
    ReplyEntry,
    ReplyLseek,
    ReplyOpen,
    ReplyStatfs,
    ReplyWrite,
    ReplyXattr,
    Request,
    FUSE_ROOT_ID,
};
use libc::{mode_t, ERANGE, S_IFMT, S_IFREG};
use tracing::{debug, warn};

use super::{
    attr::Attr,
    block_device::{Access, BlockDevice},
    block_reader::BlockReader,
    capabilities::FsCapabilities,
    definitions::XfsIno,
    dinode::Dinode,
    dinode_core::XfsDinodeFmt,
    dir3::Dir3,
    error::{no_entry, FsError, FsResult},
    inode::RawDinode,
    sb::Sb,
    transaction::{CommitMode, Transaction, TransactionContext},
};

/// We must store the Superblock in a global variable.  This is unfortunate, and limits us to only
/// opening one disk image at a time, but it's necessary in order to use information from the
/// superblock within a Decode::decode implementation.
pub(super) static SUPERBLOCK: OnceLock<Sb> = OnceLock::new();

#[derive(Debug)]
struct OpenInode {
    dinode: Dinode,
    count:  u64,
}

/// An open file, as FUSE sees it.
///
/// FUSE asks the file system to open a file and then refers to that open file
/// by the handle it returns.  The handle is what tells a write that it is being
/// asked to modify a file that was actually opened, rather than a file number
/// that the kernel made up.
///
/// The file's position is not here.  FUSE sends the offset of every read and
/// write, so the kernel holds the position, and a second copy of it could only
/// disagree with the first.
#[derive(Debug)]
struct OpenFile {
    ino:   u64,
    flags: i32,
}

#[derive(Debug)]
pub struct Volume {
    device:      BlockReader,
    rt_device:   Option<BlockReader>,
    sb:          Sb,
    open_files:  HashMap<u64, OpenInode>,
    /// The files the kernel has open, by handle.
    handles:     HashMap<u64, OpenFile>,
    next_handle: u64,
    /// Where transactions get their device, cache, and superblock.
    tx:          TransactionContext,
    /// May this mount change the image?
    writable:    bool,
    no_open:     bool,
    no_opendir:  bool,
}

impl Volume {
    const TTL_RO: Duration = Duration::from_secs(u64::MAX);
    /// How long the kernel may cache an attribute or a directory entry.
    ///
    /// A read-only mount can hand out entries that never expire, because
    /// nothing in it ever changes, and that is what it does.
    ///
    /// A read-write mount cannot cache at all.  A write updates the modification
    /// time of the file it wrote, and the kernel would go on reporting the old
    /// one out of its own cache.  The proper fix is to tell the kernel to throw
    /// the inode away when it changes, which needs a notifier that this
    /// version of the FUSE library does not hand to a file system, so the cache
    /// is simply switched off.  That costs one round trip per attribute, which
    /// is the right trade for a file system that is still experimental: being
    /// right is worth more here than being quick.
    const TTL_RW: Duration = Duration::ZERO;

    /// Open an image.
    ///
    /// `writable` asks for a read-write mount.  It is refused, rather than
    /// quietly downgraded, if the image has features that this implementation
    /// cannot keep up to date, because a read-write mount that silently ignores
    /// a feature corrupts the image.
    pub fn new(
        device_name: &Path,
        rt_device_name: Option<&PathBuf>,
        writable: bool,
    ) -> FsResult<Volume> {
        let access = if writable {
            Access::ReadWrite
        } else {
            Access::ReadOnly
        };
        let block_device = Arc::new(BlockDevice::open(device_name, access)?);
        let mut device = BlockReader::from_device(Arc::clone(&block_device));
        let rt_device = rt_device_name.map(|n| BlockReader::open(n)).transpose()?;

        let superblock = Sb::from(device.by_ref());
        let capabilities = FsCapabilities::inspect(&superblock, rt_device.is_some());
        if writable && !capabilities.writable() {
            return Err(FsError::read_only(capabilities.refusal()));
        }
        if superblock.is_read_only() {
            return Err(FsError::read_only(
                "the superblock says that this file system is read-only",
            ));
        }
        SUPERBLOCK.set(superblock).map_err(|_| FsError::Corrupt {
            what: "a second image was opened in the same process".into(),
        })?;

        if let Some(rtdev) = &rt_device {
            // Check that rtdev's size matches superblock.sb_rblocks
            let rtdev_blocks = rtdev.size / u64::from(superblock.sb_blocksize);
            if rtdev_blocks != superblock.sb_rblocks {
                warn!(
                    "realtime device size mismatch.  Expected {} blocks; found {}",
                    superblock.sb_rblocks, rtdev_blocks
                );
            }
        }

        if superblock.sb_rootino == 0 {
            return Err(FsError::Corrupt {
                what: "the superblock has no root inode".into(),
            });
        }
        let root_inode = Dinode::from(device.by_ref(), &superblock, superblock.sb_rootino);
        let mut open_files = HashMap::new();
        // Prepopulate the root inode into the cache, since fusefs never sends a lookup for it.
        open_files.insert(
            FUSE_ROOT_ID,
            OpenInode {
                dinode: root_inode,
                count:  1,
            },
        );

        debug!(
            "mounting {device_name:?}: {capabilities}, real-time device: {}, {} allocation \
             groups, {} bytes per block",
            capabilities.has_realtime(),
            superblock.agcount(),
            superblock.sb_blocksize
        );

        let mode = if writable {
            CommitMode::Direct
        } else {
            CommitMode::ReadOnly
        };
        let tx = TransactionContext::new(block_device, &superblock, mode);

        Ok(Volume {
            device,
            rt_device,
            sb: superblock,
            open_files,
            handles: HashMap::new(),
            next_handle: 1,
            tx,
            writable,
            no_open: false,
            no_opendir: false,
        })
    }

    /// How long the kernel may cache things, given whether this mount can
    /// change them.
    /// How long the kernel may cache things, given whether this mount can
    /// change them.
    fn ttl(&self) -> Duration {
        if self.writable {
            Self::TTL_RW
        } else {
            Self::TTL_RO
        }
    }

    /// The image's inode number for a FUSE inode number.
    ///
    /// FUSE insists that the root directory be inode 1, and XFS does not agree,
    /// so one file has two names here.  Everything that goes to the image needs
    /// the XFS one.
    fn xfs_ino(&self, ino: u64) -> XfsIno {
        if ino == FUSE_ROOT_ID {
            self.sb.sb_rootino
        } else {
            ino as XfsIno
        }
    }

    fn open_inode(&mut self, ino: u64) -> &mut OpenInode {
        let sb = &self.sb;
        let xfs_ino = if ino == FUSE_ROOT_ID {
            sb.sb_rootino
        } else {
            ino as XfsIno
        };
        self.open_files
            .entry(ino)
            .and_modify(|e| e.count += 1)
            .or_insert_with(|| {
                self.device.set_bufsize(sb.inode_size());
                let dinode = Dinode::from(self.device.by_ref(), sb, xfs_ino);
                OpenInode { dinode, count: 1 }
            })
    }

    /// Begin a transaction on the data device.
    fn begin(&mut self) -> Transaction<'_> {
        self.tx.begin()
    }

    /// Overwrite part of an existing file.
    ///
    /// Only bytes that are already inside a written extent of the file may be
    /// written.  Anything else -- past the end of the file, into a hole, into
    /// a preallocated-but-unwritten extent, into a directory -- is refused,
    /// because answering those needs an allocator, and guessing at one would
    /// mean writing blocks that belong to nobody.
    ///
    /// The whole range is checked before any of it is written, so a write that
    /// is refused leaves the file exactly as it was.
    fn write_data(&mut self, ino: u64, offset: u64, data: &[u8]) -> FsResult<u32> {
        if !self.writable {
            return Err(FsError::read_only("write"));
        }
        let oi = self
            .open_files
            .get_mut(&ino)
            .ok_or_else(|| no_entry(b"an inode the kernel has not looked up"))?;

        if oi.dinode.di_core.di_mode as mode_t & S_IFMT != S_IFREG {
            return Err(FsError::invalid(
                libc::EBADF,
                "only regular files can be written to",
            ));
        }
        if oi.dinode.is_realtime() {
            return Err(FsError::unsupported(
                "writing to a file on a real-time device",
            ));
        }
        // The two fork formats that hold a mapping from logical blocks to
        // physical ones.  Anything else -- a local fork, a device inode -- has
        // no mapping to consult, and writing into it would be a guess.
        let format = oi.dinode.di_core.di_format;
        if !matches!(format, XfsDinodeFmt::Extents | XfsDinodeFmt::Btree) {
            return Err(FsError::unsupported(format!(
                "writing to a data fork in format {format:?}"
            )));
        }

        let size = u64::try_from(oi.dinode.fsize()).map_err(FsError::from)?;
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| FsError::invalid(libc::EFBIG, "write runs past the end of the file"))?;
        if end > size {
            return Err(FsError::invalid(
                libc::EFBIG,
                format!(
                    "write at {offset} length {} runs past the end of the file ({size} bytes); \
                     growing a file is not supported yet",
                    data.len()
                ),
            ));
        }
        if data.is_empty() {
            return Ok(0);
        }

        // Work out where every block of the write lands before writing any of
        // it.  A range that straddles a hole is refused whole, because
        // writing the part that happens to be mapped would leave the caller
        // believing the whole write happened.
        let blocksize = u64::from(self.sb.sb_blocksize);
        let mut runs: Vec<(u64, u64, u64)> = Vec::new();
        {
            self.device.set_bufsize(self.sb.inode_size());
            let file = oi.dinode.get_file().map_err(FsError::from)?;
            let mut pos = offset;
            while pos < end {
                let dblock = pos / blocksize;
                let within = pos % blocksize;
                let n = std::cmp::min(blocksize - within, end - pos);
                let (start, len) = file
                    .lookup(self.device.by_ref(), &self.sb, dblock)
                    .map_err(|errno| FsError::invalid(errno, "extent lookup failed"))?;
                let start = start.ok_or_else(|| {
                    FsError::invalid(
                        libc::ENXIO,
                        format!(
                            "block {dblock} of the file is a hole; writing into a hole needs an \
                             allocator"
                        ),
                    )
                })?;
                // The byte to write is `within` bytes into the block the
                // logical block starts at.
                let image_offset = self.sb.fsb_to_offset(start) + within;
                match runs.last_mut() {
                    // Keep a run going as long as the next piece of the write
                    // is the very next byte of the image.
                    Some(run) if run.0 + run.1 == image_offset && run.1 + n <= len * blocksize => {
                        run.1 += n;
                    }
                    _ => runs.push((image_offset, n, len * blocksize)),
                }
                pos += n;
            }
        }

        debug!(
            "writing {len} bytes to inode {ino} at offset {offset}: {runs:?}",
            len = data.len()
        );

        let inode_offset = self.sb.inode_offset(self.xfs_ino(ino));
        let inode_size = self.sb.inode_size();
        let now = SystemTime::now();
        let mut tx = self.begin();
        let mut written = 0u64;
        for (at, len, _run) in runs {
            let start = written as usize;
            let end = start + len as usize;
            tx.write_data(at, &data[start..end])?;
            written += len;
        }

        // The inode records when the file was last written, and when its
        // metadata last changed.  Both are now.
        {
            let raw = tx.read_bytes(inode_offset, inode_size)?;
            let mut raw = RawDinode::from_bytes(raw)?;
            raw.set_mtime(now);
            raw.set_ctime(now);
            raw.finalise();
            tx.write_bytes(inode_offset, raw.as_bytes())?;
        }
        tx.commit()?;

        // The read side may be holding a copy of a block that has just
        // changed, and the cached inode holds timestamps that have just
        // changed.  Both are replaced with what is now on the image.
        self.device.invalidate();
        self.device.set_bufsize(self.sb.inode_size());
        let xfs_ino = self.xfs_ino(ino);
        let sb = &self.sb;
        let dinode = Dinode::from(self.device.by_ref(), sb, xfs_ino);
        if let Some(oi) = self.open_files.get_mut(&ino) {
            oi.dinode = dinode;
        }
        Ok(written as u32)
    }
}

impl Filesystem for Volume {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let parent_oi = &mut self.open_files.get_mut(&parent).unwrap();
        let dirsize = self.sb.sb_blocksize << self.sb.sb_dirblklog;
        self.device.set_bufsize(dirsize as usize);
        let dir = parent_oi.dinode.get_dir(self.device.by_ref(), &self.sb);
        match dir.lookup(self.device.by_ref(), &self.sb, name) {
            Ok(ino) => {
                let ttl = self.ttl();
                let oi = self.open_inode(ino);
                match oi.dinode.di_core.stat(ino) {
                    Ok(attr) => {
                        // We don't need to report the inode generation since this is a read-only
                        // file system.  But we'll do it anyway.
                        reply.entry(&ttl, &attr, oi.dinode.di_core.di_gen.into())
                    }
                    Err(err) => reply.error(err),
                }
            }
            Err(err) => reply.error(err),
        }
    }

    fn lseek(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        let uoffset = if let Ok(offs) = u64::try_from(offset) {
            offs
        } else {
            reply.error(libc::EINVAL);
            return;
        };

        let oi = &mut self.open_files.get_mut(&ino).unwrap();
        if offset > oi.dinode.fsize() {
            reply.error(libc::ENXIO);
            return;
        }

        match oi.dinode.lseek(self.device.by_ref(), uoffset, whence) {
            Ok(ofs) => reply.offset(i64::try_from(ofs).unwrap()),
            Err(e) => reply.error(e),
        }
    }

    fn forget(&mut self, _req: &Request, ino: u64, nlookup: u64) {
        if ino == FUSE_ROOT_ID {
            // Special case: since fusefs never does a lookup for the root
            // inode, its FORGETs may be "unmatched"
            return;
        }
        match self.open_files.get_mut(&ino) {
            Some(oi) => {
                oi.count -= nlookup;
                if oi.count == 0 {
                    self.open_files.remove(&ino);
                } else {
                    // AFAICT the kernel will never send a partial forget.  Alert the admin if it
                    // ever happens.
                    warn!("Partial forget for ino {}", ino);
                }
            }
            None => warn!("Forget without lookup for inode {}", ino),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let ttl = self.ttl();
        let attr = self
            .open_files
            .get(&ino)
            .expect("getattr before lookup")
            .dinode
            .di_core
            .stat(ino)
            .expect("Unknown file type");

        reply.attr(&ttl, &attr)
    }

    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> Result<(), i32> {
        // Open handles are only useful, and only correct, if the kernel sends
        // them.  A read-only mount has no reason to keep any, so it still asks
        // for the zero-message form and gets out of the open call entirely.
        if !self.writable {
            if config.add_capabilities(FUSE_NO_OPEN_SUPPORT).is_ok() {
                self.no_open = true;
            }
            if config.add_capabilities(FUSE_NO_OPENDIR_SUPPORT).is_ok() {
                self.no_opendir = true;
            }
        }
        let _ = config.add_capabilities(FUSE_ASYNC_READ | FUSE_EXPORT_SUPPORT);
        Ok(())
    }

    fn readlink(&mut self, _req: &Request, ino: u64, reply: fuser::ReplyData) {
        self.device.set_bufsize(self.sb.sb_blocksize as usize);
        reply.data(
            self.open_files
                .get(&ino)
                .expect("readlink before lookup")
                .dinode
                .get_link_data(self.device.by_ref(), &self.sb)
                .as_bytes(),
        );
    }

    /// Open a file, handing back a handle that later operations use.
    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        if self.no_open {
            reply.error(libc::ENOSYS);
            return;
        }
        let handle = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(handle, OpenFile { ino, flags });
        reply.opened(handle, FOPEN_KEEP_CACHE)
    }

    /// Write into a file that is already open.
    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        // A write has to be against a file the kernel actually opened for
        // writing.  Anything else is a request that does not make sense, and
        // answering it would mean writing to a file nobody asked us to write.
        match self.handles.get(&fh) {
            Some(of) if of.ino == ino => {
                if of.flags & libc::O_ACCMODE == libc::O_RDONLY {
                    reply.error(libc::EBADF);
                    return;
                }
            }
            Some(_) => {
                reply.error(libc::EBADF);
                return;
            }
            None => {
                reply.error(libc::EBADF);
                return;
            }
        }
        let offset = match u64::try_from(offset) {
            Ok(offset) => offset,
            Err(_) => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        match self.write_data(ino, offset, data) {
            Ok(n) => reply.written(n),
            Err(e) => {
                warn!(
                    "write of {} bytes to inode {ino} at {offset} failed: {e}",
                    data.len()
                );
                reply.error(e.errno())
            }
        }
    }

    /// Called on every close of a file descriptor.  Nothing is left to do: each
    /// write was committed before the kernel was told it had succeeded, so by
    /// the time the last descriptor is closed there is nothing in flight.
    fn flush(&mut self, _req: &Request, _ino: u64, _fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        reply.ok()
    }

    /// Flush the image, if the caller wants the data to have reached the
    /// underlying storage.
    fn fsync(&mut self, _req: &Request, _ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        let result = self.tx.device().flush();
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    /// Close a file handle.
    #[allow(clippy::too_many_arguments)]
    fn release(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.handles.remove(&fh);
        reply.ok()
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyData,
    ) {
        let oi = &mut self.open_files.get_mut(&ino).unwrap();
        self.device.set_bufsize(self.sb.sb_blocksize as usize);

        let rtdev = if oi.dinode.is_realtime() {
            if let Some(rtd) = &mut self.rt_device {
                Some(rtd.by_ref())
            } else {
                warn!("Realtime device not mounted");
                reply.error(libc::ENXIO);
                return;
            }
        } else {
            None
        };
        match oi.dinode.read(self.device.by_ref(), rtdev, offset, size) {
            Ok((v, ignore)) => reply.data(&v[ignore..]),
            Err(e) => reply.error(e),
        }
    }

    fn opendir(&mut self, _req: &Request, _ino: u64, _flags: i32, reply: ReplyOpen) {
        if self.no_opendir {
            reply.error(libc::ENOSYS)
        } else {
            reply.opened(0, FOPEN_CACHE_DIR)
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let dirsize = self.sb.sb_blocksize << self.sb.sb_dirblklog;
        self.device.set_bufsize(dirsize as usize);
        let oi = &mut self.open_files.get_mut(&ino).unwrap();

        let dir = oi.dinode.get_dir(self.device.by_ref(), &self.sb);

        let mut off = offset;
        loop {
            let res = dir.next(self.device.by_ref(), &self.sb, off);
            match res {
                Ok((ino, offset, kind, name)) => {
                    // FUSE requires the file system's root directory to have a
                    // fixed inode number.
                    let ino = if ino == self.sb.sb_rootino {
                        FUSE_ROOT_ID
                    } else {
                        ino
                    };
                    let kind = match kind {
                        Some(kind) => kind,
                        None => {
                            // This is very inefficient.  Frequently, getattr will be called for
                            // every entry returned by readdir.  In such cases, this code will read
                            // the inode twice.  The best solution is for everybody to use the
                            // ftype option in their XFS format.
                            self.device.set_bufsize(self.sb.inode_size());
                            let dinode = Dinode::from(
                                self.device.by_ref(),
                                &self.sb,
                                if ino == FUSE_ROOT_ID {
                                    self.sb.sb_rootino
                                } else {
                                    ino as XfsIno
                                },
                            );
                            match dinode.di_core.stat(ino) {
                                Ok(attr) => attr.kind,
                                Err(e) => {
                                    reply.error(e);
                                    return;
                                }
                            }
                        }
                    };
                    let res = reply.add(ino, offset, kind, name);
                    if res {
                        reply.ok();
                        return;
                    }
                    off = offset;
                }
                // TODO: don't ignore errors other than ENOENT
                Err(_) => {
                    reply.ok();
                    return;
                }
            }
        }
    }

    fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
        reply.statfs(
            self.sb.sb_dblocks - u64::from(self.sb.sb_logblocks),
            self.sb.sb_fdblocks,
            self.sb.sb_fdblocks,
            self.sb.sb_icount,
            self.sb.sb_ifree,
            self.sb.sb_blocksize,
            255,
            self.sb.sb_blocksize,
        )
    }

    fn getxattr(&mut self, _req: &Request, ino: u64, name: &OsStr, size: u32, reply: ReplyXattr) {
        let mut nameparts = name.as_bytes().splitn(2, |c| *c == b'.');
        let _namespace = nameparts.next().unwrap();
        let name = OsStr::from_bytes(nameparts.next().unwrap());

        let oi = &mut self.open_files.get_mut(&ino).unwrap();
        self.device.set_bufsize(self.sb.sb_blocksize as usize);
        match oi.dinode.get_attrs(self.device.by_ref(), &self.sb) {
            Some(attrs) => match attrs.get(self.device.by_ref(), &self.sb, name) {
                Ok(value) => {
                    let len: u32 = value.len().try_into().unwrap();
                    if size == 0 {
                        reply.size(len);
                    } else if len > size {
                        reply.error(ERANGE);
                    } else {
                        reply.data(value.as_slice())
                    }
                }
                Err(e) => reply.error(e),
            },
            None => {
                reply.error(crate::libxfuse::ENOATTR);
            }
        }
    }

    fn listxattr(&mut self, _req: &Request, ino: u64, size: u32, reply: ReplyXattr) {
        let oi = &mut self
            .open_files
            .get_mut(&ino)
            .expect("listxattr before lookup");
        self.device.set_bufsize(self.sb.sb_blocksize as usize);
        match oi.dinode.get_attrs(self.device.by_ref(), &self.sb) {
            Some(ref mut attrs) => {
                let attrs_size = attrs.get_total_size(self.device.by_ref(), &self.sb);

                if size == 0 {
                    reply.size(attrs_size);
                    return;
                }

                if attrs_size > size {
                    reply.error(ERANGE);
                    return;
                }

                let list = attrs.list(self.device.by_ref(), &self.sb);
                // Assert that we calculated the list size correctly.  This assertion is only
                // safe since we're a read-only file system.
                assert_eq!(
                    list.len(),
                    attrs_size as usize,
                    "size calculation was wrong!"
                );
                reply.data(list.as_slice());
            }
            None => {
                reply.size(0);
            }
        }
    }
}
