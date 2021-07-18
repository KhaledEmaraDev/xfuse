use std::io::{BufRead, Seek};

use super::{
    definitions::*,
    dinode::Dinode,
    dir3::Dir3,
    sb::Sb,
    utils::{get_file_type, FileKind},
};

use byteorder::{BigEndian, ReadBytesExt};
use fuse::{FileAttr, FileType};
use libc::{c_int, ENOENT, S_IFMT};
use time::Timespec;

// pub type XfsDir2SfOff = [u8; 2];

#[derive(Debug, Clone)]
pub enum XfsDir2Inou {
    XfsDir2Ino8(u64),
    XfsDir2Ino4(u32),
}

#[derive(Debug, Clone)]
pub struct Dir2SfHdr {
    pub count: u8,
    pub i8count: u8,
    pub parent: XfsDir2Inou,
}

impl Dir2SfHdr {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir2SfHdr {
        let count = buf_reader.read_u8().unwrap();
        let i8count = buf_reader.read_u8().unwrap();

        let parent = if i8count > 0 {
            XfsDir2Inou::XfsDir2Ino8(buf_reader.read_u64::<BigEndian>().unwrap())
        } else {
            XfsDir2Inou::XfsDir2Ino4(buf_reader.read_u32::<BigEndian>().unwrap())
        };

        Dir2SfHdr {
            count,
            i8count,
            parent,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Dir2SfEntry {
    pub namelen: u8,
    pub offset: u16,
    pub name: String,
    pub ftype: u8,
    pub inumber: XfsDir2Inou,
}

impl Dir2SfEntry {
    pub fn from<T: BufRead>(buf_reader: &mut T, i8count: u8) -> Dir2SfEntry {
        let namelen = buf_reader.read_u8().unwrap();

        let offset = buf_reader.read_u16::<BigEndian>().unwrap();

        let mut name = String::new();
        for _i in 0..namelen {
            name.push(buf_reader.read_u8().unwrap() as char);
        }

        let ftype = buf_reader.read_u8().unwrap();

        let inumber = if i8count > 0 {
            XfsDir2Inou::XfsDir2Ino8(buf_reader.read_u64::<BigEndian>().unwrap())
        } else {
            XfsDir2Inou::XfsDir2Ino4(buf_reader.read_u32::<BigEndian>().unwrap())
        };

        Dir2SfEntry {
            namelen,
            offset,
            name,
            ftype,
            inumber,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Dir2Sf {
    pub hdr: Dir2SfHdr,
    pub list: Vec<Dir2SfEntry>,
}

impl Dir2Sf {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir2Sf {
        let hdr = Dir2SfHdr::from(buf_reader.by_ref());

        let mut list = Vec::<Dir2SfEntry>::new();
        for _i in 0..hdr.count {
            list.push(Dir2SfEntry::from(buf_reader.by_ref(), hdr.i8count))
        }

        Dir2Sf { hdr, list }
    }
}

impl Dir3 for Dir2Sf {
    fn lookup<T: BufRead + Seek>(
        &self,
        buf_reader: &mut T,
        super_block: &Sb,
        name: &str,
    ) -> Result<(FileAttr, u64), c_int> {
        let mut inode: Option<XfsIno> = None;

        for entry in self.list.iter() {
            if entry.name == name {
                inode = match entry.inumber {
                    XfsDir2Inou::XfsDir2Ino8(inumber) => Some(inumber),
                    XfsDir2Inou::XfsDir2Ino4(inumber) => Some(inumber as u64),
                };
            }
        }

        if let Some(ino) = inode {
            let dinode = Dinode::from(buf_reader.by_ref(), super_block, ino);

            let kind = get_file_type(FileKind::Mode(dinode.di_core.di_mode))?;

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

            Ok((attr, dinode.di_core.di_gen.into()))
        } else {
            Err(ENOENT)
        }
    }

    fn next<T: BufRead + Seek>(
        &self,
        _buf_reader: &mut T,
        offset: i64,
    ) -> Result<(XfsIno, i64, FileType, String), c_int> {
        for entry in self.list.iter() {
            if i64::from(entry.offset) <= offset {
                continue;
            }

            let ino = match entry.inumber {
                XfsDir2Inou::XfsDir2Ino8(inumber) => inumber,
                XfsDir2Inou::XfsDir2Ino4(inumber) => inumber as u64,
            };

            let kind = get_file_type(FileKind::Type(entry.ftype))?;

            let name = entry.name.to_owned();

            return Ok((ino, entry.offset as i64, kind, name));
        }

        Err(-1)
    }
}
