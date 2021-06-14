use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};

use super::agi::Agi;
use super::definitions::XfsIno;
use super::dinode::{DiU, Dinode};
use super::dir2_sf::XfsDir2Inou;
use super::dir2_sf::{XFS_DIR3_FT_DIR, XFS_DIR3_FT_REG_FILE};
use super::sb::Sb;

use fuse::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, Request, FUSE_ROOT_ID,
};
use libc::{ENOENT, S_IFDIR, S_IFMT, S_IFREG};
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

        let mut inode: Option<XfsIno> = None;

        match dinode.di_u {
            DiU::DiDir2Sf(dir) => {
                for entry in dir.list.into_iter() {
                    if entry.name == name.to_string_lossy().into_owned() {
                        inode = match entry.inumber {
                            XfsDir2Inou::XfsDir2Ino8(inumber) => Some(inumber),
                            XfsDir2Inou::XfsDir2Ino4(inumber) => Some(inumber as u64),
                        };
                    }
                }
            }
        }

        if let Some(ino) = inode {
            let dinode = Dinode::from(
                BufReader::new(&self.device).by_ref(),
                &self.sb,
                if ino == FUSE_ROOT_ID {
                    self.sb.sb_rootino
                } else {
                    ino as XfsIno
                },
            );

            let kind = match (dinode.di_core.di_mode as u32) & S_IFMT {
                S_IFREG => FileType::RegularFile,
                S_IFDIR => FileType::Directory,
                _ => {
                    reply.error(ENOENT);
                    return;
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

            reply.entry(&ttl, &attr, dinode.di_core.di_gen as u64);
        } else {
            reply.error(ENOENT);
            return;
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

        let kind = match (dinode.di_core.di_mode as u32) & S_IFMT {
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

        match dinode.di_u {
            DiU::DiDir2Sf(dir) => {
                for entry in dir.list.into_iter() {
                    if i64::from(entry.offset) <= offset {
                        continue;
                    }

                    let ino = match entry.inumber {
                        XfsDir2Inou::XfsDir2Ino8(inumber) => inumber,
                        XfsDir2Inou::XfsDir2Ino4(inumber) => inumber as u64,
                    };

                    let kind = match entry.ftype {
                        XFS_DIR3_FT_REG_FILE => FileType::RegularFile,
                        XFS_DIR3_FT_DIR => FileType::Directory,
                        _ => {
                            panic!("Unknown file type.")
                        }
                    };

                    reply.add(ino, entry.offset as i64, kind, entry.name);
                }
                reply.ok();
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
