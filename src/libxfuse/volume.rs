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
    ReplyStatfs, ReplyXattr, Request, FUSE_ROOT_ID,
};
use libc::{mode_t, ERANGE, S_IFDIR, S_IFMT, S_IFREG};
use time::Timespec;

#[derive(Debug)]
pub struct Volume {
    pub device: File,
    pub sb: Sb,
    pub agi: Agi,
    pub root_ino: Dinode,
    pub open_files: Vec<Dinode>,
}

impl Volume {
    pub fn from(device_name: &str) -> Volume {
        let device = File::open(device_name).unwrap();
        let mut buf_reader = BufReader::new(&device);

        let superblock = Sb::from(buf_reader.by_ref());

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
            open_files: Vec::new(),
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
            InodeType::Dir2Node(dir) => {
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
            InodeType::Dir2Btree(dir) => {
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
            _ => {
                panic!("Not a directory.")
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

    fn readlink(&mut self, _req: &Request, ino: u64, reply: fuse::ReplyData) {
        println!("readlink: {}", ino);
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
            InodeType::SymlinkSf(data) => {
                reply.data(data.as_bytes_with_nul());
            }
            InodeType::SymlinkExtents(data) => {
                reply.data(data.as_bytes_with_nul());
            }
            _ => {
                panic!("Not a symlink.");
            }
        }
    }

    fn open(&mut self, _req: &Request, ino: u64, flags: u32, reply: ReplyOpen) {
        println!("open: {}", ino);

        let dinode = Dinode::from(
            BufReader::new(&self.device).by_ref(),
            &self.sb,
            if ino == FUSE_ROOT_ID {
                self.sb.sb_rootino
            } else {
                ino as XfsIno
            },
        );

        self.open_files.push(dinode);

        reply.opened((self.open_files.len() as u64) - 1, flags)
    }

    fn read(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        reply: fuse::ReplyData,
    ) {
        println!("read: {}", _ino);

        let dinode = &self.open_files[fh as usize];
        let mut buf_reader = BufReader::new(&self.device);

        let mut file = dinode.get_file(buf_reader.by_ref(), &self.sb);

        reply.data(
            file.read(buf_reader.by_ref(), &self.sb, offset, size)
                .as_slice(),
        );
    }

    fn release(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        println!("release: {}", _ino);

        self.open_files.remove(fh as usize);

        reply.ok();
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
            InodeType::Dir2Node(dir) => {
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
            InodeType::Dir2Btree(dir) => {
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
            _ => {
                panic!("Not a directory.")
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

    fn getxattr(&mut self, _req: &Request, ino: u64, name: &OsStr, size: u32, reply: ReplyXattr) {
        println!("getxattr: {:?}", name);

        let name = name.to_string_lossy();
        let name: Vec<&str> = name.split('.').collect();
        let name = name[1];

        let mut buf_reader = BufReader::new(&self.device);
        let inode_number = if ino == FUSE_ROOT_ID {
            self.sb.sb_rootino
        } else {
            ino as XfsIno
        };
        let dinode = Dinode::from(buf_reader.by_ref(), &self.sb, inode_number);

        let attrs = dinode.get_attrs(buf_reader.by_ref(), &self.sb);
        match attrs {
            Some(attrs) => {
                let attrs_size = attrs.get_size(buf_reader.by_ref(), &self.sb, &name);

                if size == 0 {
                    reply.size(attrs_size);
                    return;
                }

                if attrs_size > size {
                    reply.error(ERANGE);
                    return;
                }

                reply.data(attrs.get(buf_reader.by_ref(), &self.sb, &name).as_slice());
            }
            None => {
                if size == 0 {
                    reply.size(0);
                } else {
                    panic!("No attributes!");
                }
            }
        }
    }

    fn listxattr(&mut self, _req: &Request, ino: u64, size: u32, reply: ReplyXattr) {
        println!("listxattr: {}", ino);

        let mut buf_reader = BufReader::new(&self.device);
        let inode_number = if ino == FUSE_ROOT_ID {
            self.sb.sb_rootino
        } else {
            ino as XfsIno
        };
        let dinode = Dinode::from(buf_reader.by_ref(), &self.sb, inode_number);

        let attrs = dinode.get_attrs(buf_reader.by_ref(), &self.sb);
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

                reply.data(attrs.list(buf_reader.by_ref(), &self.sb).as_slice());
            }
            None => {
                if size == 0 {
                    reply.size(0);
                } else {
                    panic!("No attributes!");
                }
            }
        }
    }

    fn access(&mut self, _req: &Request, _ino: u64, _mask: u32, reply: ReplyEmpty) {
        println!("access: {}", _ino);
        reply.ok();
    }
}
