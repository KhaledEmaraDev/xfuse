/**
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
use std::io::{BufRead, Seek};
use std::time::{Duration, UNIX_EPOCH};

use super::S_IFMT;
use super::{
    definitions::*,
    dinode::Dinode,
    dir3::Dir3,
    sb::Sb,
    utils::{get_file_type, FileKind},
};

use byteorder::{BigEndian, ReadBytesExt};
use fuser::{FileAttr, FileType};
use libc::{c_int, ENOENT};

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

impl<R: BufRead + Seek> Dir3<R> for Dir2Sf {
    fn lookup(
        &self,
        buf_reader: &mut R,
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
                atime: UNIX_EPOCH + Duration::new(
                    dinode.di_core.di_atime.t_sec as u64,
                    dinode.di_core.di_atime.t_nsec,
                ),
                mtime: UNIX_EPOCH + Duration::new(
                    dinode.di_core.di_mtime.t_sec as u64,
                    dinode.di_core.di_mtime.t_nsec,
                ),
                ctime: UNIX_EPOCH + Duration::new(
                    dinode.di_core.di_ctime.t_sec as u64,
                    dinode.di_core.di_ctime.t_nsec,
                ),
                crtime: UNIX_EPOCH,
                kind,
                perm: dinode.di_core.di_mode & !S_IFMT,
                nlink: dinode.di_core.di_nlink,
                uid: dinode.di_core.di_uid,
                gid: dinode.di_core.di_gid,
                rdev: 0,
                blksize: 0,
                flags: 0,
            };

            Ok((attr, dinode.di_core.di_gen.into()))
        } else {
            Err(ENOENT)
        }
    }

    fn next(
        &self,
        _buf_reader: &mut R,
        _super_block: &Sb,
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
