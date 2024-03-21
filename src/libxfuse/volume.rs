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
use std::collections::HashMap;
use std::convert::TryInto;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::sync::OnceLock;
use std::time::Duration;

use super::agi::Agi;
use super::definitions::XfsIno;
use super::dinode::Dinode;
use super::dir3::Dir3;
use super::sb::Sb;

use fuser::{
    Filesystem, ReplyAttr, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyXattr, Request, FUSE_ROOT_ID,
    consts::{FOPEN_KEEP_CACHE, FOPEN_CACHE_DIR}
};
use libc::ERANGE;
use tracing::warn;

/// We must store the Superblock in a global variable.  This is unfortunate, and limits us to only
/// opening one disk image at a time, but it's necessary in order to use information from the
/// superblock within a Decode::decode implementation.
pub(super) static SUPERBLOCK: OnceLock<Sb> = OnceLock::new();

#[derive(Debug)]
struct OpenInode {
    dinode: Dinode,
    count: u32
}

#[derive(Debug)]
pub struct Volume {
    pub device: File,
    pub sb: Sb,
    pub agi: Agi,
    pub root_ino: Dinode,
    open_files: HashMap<u64, OpenInode>,
}

impl Volume {
    // Allow the kernel to cache attributes and entries for an unlimited amount
    // of time, since nothing will ever change.
    const TTL: Duration = Duration::from_secs(u64::MAX);

    pub fn from(device_name: &str) -> Volume {
        let device = File::open(device_name).unwrap();
        let mut buf_reader = BufReader::new(&device);

        let superblock = Sb::from(buf_reader.by_ref());
        SUPERBLOCK.set(superblock).unwrap();

        buf_reader
            .seek(SeekFrom::Start(u64::from(superblock.sb_sectsize) * 2))
            .unwrap();
        let agi = Agi::from(buf_reader.by_ref());

        let root_ino = Dinode::from(buf_reader.by_ref(), &superblock, superblock.sb_rootino);

        Volume {
            device,
            sb: superblock,
            agi,
            root_ino,
            open_files: HashMap::new(),
        }
    }

    fn open_inode(&mut self, ino: u64) {
        let f = &self.device;
        let sb = &self.sb;
        self.open_files.entry(ino)
            .and_modify(|e| e.count +=1 )
            .or_insert_with(|| {
                let dinode = Dinode::from(
                    BufReader::new(f).by_ref(),
                    sb,
                    if ino == FUSE_ROOT_ID {
                        sb.sb_rootino
                    } else {
                        ino as XfsIno
                    },
                );
                OpenInode{dinode, count: 1}
            });
    }
}

impl Filesystem for Volume {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let mut buf_reader = BufReader::new(&self.device);
        let r = match self.open_files.get_mut(&parent) {
            Some(oi) => {
                let dir = oi.dinode.get_dir(buf_reader.by_ref(), &self.sb);
                dir.lookup(
                    BufReader::new(&self.device).by_ref(),
                    &self.sb,
                    name,
                )
            },
            None => {
                let inode_number = if parent == FUSE_ROOT_ID {
                    self.sb.sb_rootino
                } else {
                    parent as XfsIno
                };
                let mut dinode = Dinode::from(buf_reader.by_ref(), &self.sb, inode_number);
                let dir = dinode.get_dir(buf_reader.by_ref(), &self.sb);
                dir.lookup(
                    BufReader::new(&self.device).by_ref(),
                    &self.sb,
                    name,
                )
            }
        };

        match r {
            Ok((attr, generation)) => {
                reply.entry(&Self::TTL, &attr, generation);
            }
            Err(err) => reply.error(err),
        };
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        let attr = match self.open_files.get(&ino) {
            Some(oi) => oi.dinode.di_core.stat(ino),
            None => {
                let inode_number = if ino == FUSE_ROOT_ID {
                    self.sb.sb_rootino
                } else {
                    ino as XfsIno
                };
                let mut buf_reader = BufReader::new(&self.device);
                let dinode = Dinode::from(buf_reader.by_ref(), &self.sb, inode_number);
                dinode.di_core.stat(ino)
            }
        }.expect("Unknown file type");

        reply.attr(&Self::TTL, &attr)
    }

    fn readlink(&mut self, _req: &Request, ino: u64, reply: fuser::ReplyData) {
        let dinode = Dinode::from(
            BufReader::new(&self.device).by_ref(),
            &self.sb,
            if ino == FUSE_ROOT_ID {
                self.sb.sb_rootino
            } else {
                ino as XfsIno
            },
        );

        let mut buf_reader = BufReader::new(&self.device);

        reply.data(
            dinode
                .get_link_data(buf_reader.by_ref(), &self.sb)
                .as_bytes(),
        );
    }

    fn open(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
        self.open_inode(ino);
        reply.opened(0, FOPEN_KEEP_CACHE)
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
        let mut buf_reader = BufReader::new(&self.device);

        let mut file = oi.dinode.get_file(buf_reader.by_ref());

        match file.read(buf_reader.by_ref(), offset, size) {
            Ok((v, ignore)) => reply.data(&v[ignore..]),
            Err(e) => reply.error(e)
        }
    }

    fn release(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.open_files.get_mut(&ino) {
            Some(oi) => {
                oi.count -= 1;
                if oi.count == 0 {
                    self.open_files.remove(&ino);
                }
            },
            None => warn!("close without open for inode {}", ino)
        }

        reply.ok();
    }

    fn opendir(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
        self.open_inode(ino);
        reply.opened(0, FOPEN_CACHE_DIR);
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let mut buf_reader = BufReader::new(&self.device);
        let oi = &mut self.open_files.get_mut(&ino).unwrap();

        let dir = oi.dinode.get_dir(buf_reader.by_ref(), &self.sb);

        let mut off = offset;
        loop {
            let res = dir.next(BufReader::new(&self.device).by_ref(), &self.sb, off);
            match res {
                Ok((ino, offset, kind, name)) => {
                    // FUSE requires the file system's root directory to have a
                    // fixed inode number.
                    let ino = if ino == self.sb.sb_rootino {
                        FUSE_ROOT_ID
                    } else {
                        ino
                    };
                    let res = reply.add(ino, offset, kind, name);
                    if res {
                        reply.ok();
                        return;
                    }
                    off = offset;
                }
                Err(_) => {
                    reply.ok();
                    return;
                }
            }
        }
    }

    fn releasedir(&mut self, req: &Request, ino: u64, fh: u64, flags: i32, reply: ReplyEmpty) {
        self.release(req, ino, fh, flags, None, false, reply)
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

        let mut buf_reader = BufReader::new(&self.device);
        let attrs = match self.open_files.get(&ino) {
            Some(oi) => oi.dinode.get_attrs(buf_reader.by_ref(), &self.sb),
            None => {
                let inode_number = if ino == FUSE_ROOT_ID {
                    self.sb.sb_rootino
                } else {
                    ino as XfsIno
                };
                let dinode = Dinode::from(buf_reader.by_ref(), &self.sb, inode_number);
                dinode.get_attrs(buf_reader.by_ref(), &self.sb)
            }
        };

        match attrs {
            Some(attrs) => {
                match attrs.get(buf_reader.by_ref(), &self.sb, name) {
                    Ok(value) => {
                        let len: u32 = value.len().try_into().unwrap();
                        if size == 0 {
                            reply.size(len);
                        } else if len > size {
                            reply.error(ERANGE);
                        } else {
                             reply.data(value.as_slice())
                        }
                    },
                    Err(e) => reply.error(e)
                }
            }
            None => {
                reply.error(libc::ENOATTR);
            }
        }
    }

    fn listxattr(&mut self, _req: &Request, ino: u64, size: u32, reply: ReplyXattr) {
        let mut buf_reader = BufReader::new(&self.device);
        let attrs = match self.open_files.get(&ino) {
            Some(oi) => oi.dinode.get_attrs(buf_reader.by_ref(), &self.sb),
            None => {
                let inode_number = if ino == FUSE_ROOT_ID {
                    self.sb.sb_rootino
                } else {
                    ino as XfsIno
                };
                let dinode = Dinode::from(buf_reader.by_ref(), &self.sb, inode_number);
                dinode.get_attrs(buf_reader.by_ref(), &self.sb)
            }
        };

        match attrs {
            Some(mut attrs) => {
                let attrs_size = attrs.get_total_size(buf_reader.by_ref(), &self.sb);

                if size == 0 {
                    reply.size(attrs_size);
                    return;
                }

                if attrs_size > size {
                    reply.error(ERANGE);
                    return;
                }

                let list = attrs.list(buf_reader.by_ref(), &self.sb);
                // Assert that we calculated the list size correctly.  This assertion is only safe
                // since we're a read-only file system.
                assert_eq!(list.len(), attrs_size as usize, "size calculation was wrong!");
                reply.data(list.as_slice());
            }
            None => {
                reply.size(0);
            }
        }
    }

    fn access(&mut self, _req: &Request, _ino: u64, _mask: i32, reply: ReplyEmpty) {
        reply.ok();
    }
}
