use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};

use crate::libxfuse::dir3::Dir3;

use super::agi::Agi;
use super::definitions::XfsIno;
use super::dinode::{Dinode, InodeType};
use super::sb::Sb;

use fuse::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, Request, FUSE_ROOT_ID,
};
use libc::{mode_t, S_IFDIR, S_IFMT, S_IFREG};
use time::Timespec;

#[derive(Debug)]
pub struct Volume {
    pub device: File,
    pub sb: Sb,
    pub agi: Agi,
    pub root_ino: Dinode,
}

impl Volume {
    pub fn from(device_name: &str) -> Volume {
        let device = File::open(device_name).unwrap();
        let mut buf_reader = BufReader::new(&device);

        let superblock = Sb::from(buf_reader.by_ref());

        buf_reader
            .seek(SeekFrom::Start((superblock.sb_sectsize as u64) * 2))
            .unwrap();
        let agi = Agi::from(buf_reader.by_ref());

        let root_ino = Dinode::from(buf_reader.by_ref(), &superblock, superblock.sb_rootino);

        Volume {
            device,
            sb: superblock,
            agi,
            root_ino,
        }
    }
}

impl Filesystem for Volume {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        println!("lookup: {:?}", name);

        let dinode = Dinode::from(
            BufReader::new(&self.device).by_ref(),
            &self.sb,
            if parent == FUSE_ROOT_ID {
                self.sb.sb_rootino
            } else {
                parent as XfsIno
            },
        );

        let ttl = Timespec {
            sec: 86400,
            nsec: 0,
        };

        match dinode.get_data(BufReader::new(&self.device).by_ref(), &self.sb) {
            InodeType::Dir2Sf(dir) => {
                match dir.lookup(
                    BufReader::new(&self.device).by_ref(),
                    &self.sb,
                    &name.to_string_lossy().to_owned(),
                ) {
                    Ok((attr, generation)) => {
                        reply.entry(&ttl, &attr, generation);
                    }
                    Err(err) => reply.error(err),
                }
            }
            InodeType::Dir2Block(dir) => {
                match dir.lookup(
                    BufReader::new(&self.device).by_ref(),
                    &self.sb,
                    &name.to_string_lossy().to_owned(),
                ) {
                    Ok((attr, generation)) => {
                        reply.entry(&ttl, &attr, generation);
                    }
                    Err(err) => reply.error(err),
                }
            }
            InodeType::Dir2Leaf(dir) => {
                match dir.lookup(
                    BufReader::new(&self.device).by_ref(),
                    &self.sb,
                    &name.to_string_lossy().to_owned(),
                ) {
                    Ok((attr, generation)) => {
                        reply.entry(&ttl, &attr, generation);
                    }
                    Err(err) => reply.error(err),
                }
            }
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        println!("getattr: {}", ino);
        let dinode = Dinode::from(
            BufReader::new(&self.device).by_ref(),
            &self.sb,
            if ino == FUSE_ROOT_ID {
                self.sb.sb_rootino
            } else {
                ino as XfsIno
            },
        );

        let ttl = Timespec {
            sec: 86400,
            nsec: 0,
        };

        let kind = match (dinode.di_core.di_mode as mode_t) & S_IFMT {
            S_IFREG => FileType::RegularFile,
            S_IFDIR => FileType::Directory,
            _ => {
                panic!("Unknown file type.")
            }
        };

        let attr = FileAttr {
            ino,
            size: dinode.di_core.di_size as u64,
            blocks: dinode.di_core.di_nblocks,
            atime: Timespec {
                sec: dinode.di_core.di_atime.t_sec as i64,
                nsec: dinode.di_core.di_atime.t_nsec,
            },
            mtime: Timespec {
                sec: dinode.di_core.di_mtime.t_sec as i64,
                nsec: dinode.di_core.di_mtime.t_nsec,
            },
            ctime: Timespec {
                sec: dinode.di_core.di_ctime.t_sec as i64,
                nsec: dinode.di_core.di_ctime.t_nsec,
            },
            crtime: Timespec { sec: 0, nsec: 0 },
            kind,
            perm: dinode.di_core.di_mode & (!(S_IFMT as u16)),
            nlink: dinode.di_core.di_nlink,
            uid: dinode.di_core.di_uid,
            gid: dinode.di_core.di_gid,
            rdev: 0,
            flags: 0,
        };

        reply.attr(&ttl, &attr)
    }

    fn opendir(&mut self, _req: &Request, _ino: u64, _flags: u32, reply: ReplyOpen) {
        println!("opendir: {}", _ino);
        reply.opened(0, 0);
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        println!("readdir: {}", ino);
        let dinode = Dinode::from(
            BufReader::new(&self.device).by_ref(),
            &self.sb,
            if ino == FUSE_ROOT_ID {
                self.sb.sb_rootino
            } else {
                ino as XfsIno
            },
        );

        match dinode.get_data(BufReader::new(&self.device).by_ref(), &self.sb) {
            InodeType::Dir2Sf(dir) => {
                let mut off = offset;
                loop {
                    let res = dir.next(BufReader::new(&self.device).by_ref(), off);
                    match res {
                        Ok((ino, offset, kind, name)) => {
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
            InodeType::Dir2Block(dir) => {
                let mut off = offset;
                loop {
                    let res = dir.next(BufReader::new(&self.device).by_ref(), off);
                    match res {
                        Ok((ino, offset, kind, name)) => {
                            if reply.add(ino, offset, kind, name) {
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
            InodeType::Dir2Leaf(dir) => {
                let mut off = offset;
                loop {
                    let res = dir.next(BufReader::new(&self.device).by_ref(), off);
                    match res {
                        Ok((ino, offset, kind, name)) => {
                            if reply.add(ino, offset, kind, name) {
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
        }
    }

    fn releasedir(&mut self, _req: &Request, _ino: u64, _fh: u64, _flags: u32, reply: ReplyEmpty) {
        println!("releasedir: {}", _ino);
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
        println!("statfs: {}", _ino);
        reply.statfs(
            self.sb.sb_dblocks,
            self.sb.sb_fdblocks,
            self.sb.sb_fdblocks,
            self.sb.sb_icount,
            self.sb.sb_ifree,
            self.sb.sb_blocksize,
            255,
            self.sb.sb_blocksize,
        )
    }

    fn access(&mut self, _req: &Request, _ino: u64, _mask: u32, reply: ReplyEmpty) {
        println!("access: {}", _ino);
        reply.ok();
    }
}
