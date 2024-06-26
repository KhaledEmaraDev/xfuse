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
    path::Path,
    sync::OnceLock,
    time::Duration,
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
    ReplyEntry,
    ReplyLseek,
    ReplyOpen,
    ReplyStatfs,
    ReplyXattr,
    Request,
    FUSE_ROOT_ID,
};
use libc::ERANGE;
use tracing::warn;

use super::{
    attr::Attr,
    block_reader::BlockReader,
    definitions::XfsIno,
    dinode::Dinode,
    dir3::Dir3,
    sb::Sb,
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

#[derive(Debug)]
pub struct Volume {
    pub device: BlockReader,
    pub sb:     Sb,
    open_files: HashMap<u64, OpenInode>,
    no_open:    bool,
    no_opendir: bool,
}

impl Volume {
    // Allow the kernel to cache attributes and entries for an unlimited amount
    // of time, since nothing will ever change.
    const TTL: Duration = Duration::from_secs(u64::MAX);

    pub fn from(device_name: &Path) -> Volume {
        let mut device = BlockReader::open(device_name).unwrap();

        let superblock = Sb::from(device.by_ref());
        SUPERBLOCK.set(superblock).unwrap();

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

        Volume {
            device,
            sb: superblock,
            open_files,
            no_open: false,
            no_opendir: false,
        }
    }

    fn open_inode(&mut self, ino: u64) -> &mut OpenInode {
        let sb = &self.sb;
        self.open_files
            .entry(ino)
            .and_modify(|e| e.count += 1)
            .or_insert_with(|| {
                self.device.set_bufsize(sb.inode_size());
                let dinode = Dinode::from(
                    self.device.by_ref(),
                    sb,
                    if ino == FUSE_ROOT_ID {
                        sb.sb_rootino
                    } else {
                        ino as XfsIno
                    },
                );
                OpenInode { dinode, count: 1 }
            })
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
                let oi = self.open_inode(ino);
                match oi.dinode.di_core.stat(ino) {
                    Ok(attr) => {
                        // We don't need to report the inode generation since this is a read-only
                        // file system.  But we'll do it anyway.
                        reply.entry(&Self::TTL, &attr, oi.dinode.di_core.di_gen.into())
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

        let oi = &self.open_files.get(&ino).unwrap();
        let file = oi.dinode.get_file(self.device.by_ref());
        if offset > file.size() {
            reply.error(libc::ENXIO);
            return;
        }

        match file.lseek(self.device.by_ref(), uoffset, whence) {
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

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        let attr = self
            .open_files
            .get(&ino)
            .expect("getattr before lookup")
            .dinode
            .di_core
            .stat(ino)
            .expect("Unknown file type");

        reply.attr(&Self::TTL, &attr)
    }

    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> Result<(), i32> {
        if config.add_capabilities(FUSE_NO_OPEN_SUPPORT).is_ok() {
            self.no_open = true;
        }
        if config.add_capabilities(FUSE_NO_OPENDIR_SUPPORT).is_ok() {
            self.no_opendir = true;
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

    fn open(&mut self, _req: &Request, _ino: u64, _flags: i32, reply: ReplyOpen) {
        if self.no_open {
            reply.error(libc::ENOSYS)
        } else {
            reply.opened(0, FOPEN_KEEP_CACHE)
        }
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
        let oi = &self.open_files.get(&ino).unwrap();
        self.device.set_bufsize(self.sb.sb_blocksize as usize);

        let file = oi.dinode.get_file(self.device.by_ref());

        match file.read(self.device.by_ref(), offset, size) {
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
                reply.error(libc::ENOATTR);
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
