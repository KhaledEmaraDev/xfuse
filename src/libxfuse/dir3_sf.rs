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
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, Seek};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::time::{Duration, UNIX_EPOCH};

use super::S_IFMT;
use super::{
    definitions::*,
    dinode::Dinode,
    dir3::{Dir3, XFS_DIR3_FT_DIR},
    sb::Sb,
    utils::{get_file_type, FileKind},
};

use bincode::{
    Decode,
    de::{Decoder, read::Reader},
    error::DecodeError
};
use byteorder::{BigEndian, ReadBytesExt};
use fuser::{FileAttr, FileType};
use libc::{c_int, ENOENT};

// pub type XfsDir2SfOff = [u8; 2];

#[derive(Debug, Clone)]
pub struct Dir2SfHdr {
    pub count: u8,
    pub i8count: u8,
    pub parent: XfsIno,
}

impl Dir2SfHdr {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir2SfHdr {
        let count = buf_reader.read_u8().unwrap();
        let i8count = buf_reader.read_u8().unwrap();

        let parent = if i8count > 0 {
            buf_reader.read_u64::<BigEndian>().unwrap()
        } else {
            buf_reader.read_u32::<BigEndian>().unwrap().into()
        };

        Dir2SfHdr {
            count,
            i8count,
            parent,
        }
    }
}

impl Decode for Dir2SfHdr {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let count = Decode::decode(decoder)?;
        let i8count = Decode::decode(decoder)?;
        let parent = if i8count > 0 {
            <u64 as Decode>::decode(decoder)?
        } else {
            <u32 as Decode>::decode(decoder)?.into()
        };
        Ok(Dir2SfHdr {
            count,
            i8count,
            parent,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Dir2SfEntry32 {
    pub namelen: u8,
    pub offset: u16,
    pub name: OsString,
    pub ftype: u8,
    pub inumber: u32,
}

impl Dir2SfEntry32 {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir2SfEntry32 {
        let namelen = buf_reader.read_u8().unwrap();

        let offset = buf_reader.read_u16::<BigEndian>().unwrap();

        let mut namebytes = vec![0u8; namelen.into()];
        buf_reader.read_exact(&mut namebytes).unwrap();
        let name = OsString::from_vec(namebytes);

        let ftype = buf_reader.read_u8().unwrap();

        let inumber = buf_reader.read_u32::<BigEndian>().unwrap();

        Dir2SfEntry32 {
            namelen,
            offset,
            name,
            ftype,
            inumber,
        }
    }
}

impl Decode for Dir2SfEntry32 {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let namelen: u8 = Decode::decode(decoder)?;
        let offset: u16 = Decode::decode(decoder)?;
        let mut namebytes = vec![0u8; namelen.into()];
        decoder.reader().read(&mut namebytes[..])?;
        let name = OsString::from_vec(namebytes);
        let ftype: u8 = Decode::decode(decoder)?;
        let inumber: u32 = Decode::decode(decoder)?;
        Ok(Dir2SfEntry32 {
            namelen,
            offset,
            name,
            ftype,
            inumber,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Dir2SfEntry64 {
    pub namelen: u8,
    pub offset: u16,
    pub name: OsString,
    pub ftype: u8,
    pub inumber: XfsIno,
}

impl Dir2SfEntry64 {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir2SfEntry64 {
        let namelen = buf_reader.read_u8().unwrap();

        let offset = buf_reader.read_u16::<BigEndian>().unwrap();

        let mut namebytes = vec![0u8; namelen.into()];
        buf_reader.read_exact(&mut namebytes).unwrap();
        let name = OsString::from_vec(namebytes);

        let ftype = buf_reader.read_u8().unwrap();

        let inumber = buf_reader.read_u64::<BigEndian>().unwrap();

        Dir2SfEntry64 {
            namelen,
            offset,
            name,
            ftype,
            inumber,
        }
    }

    pub fn new(name: &[u8], ftype: u8, offset: u16, inumber: XfsIno)
        -> Self
    {
        let namelen = name.len() as u8;
        let name = OsStr::from_bytes(name).to_owned();
        Self {
            namelen,
            offset,
            name,
            ftype,
            inumber
        }
    }
}

impl Decode for Dir2SfEntry64 {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let namelen: u8 = Decode::decode(decoder)?;
        let offset: u16 = Decode::decode(decoder)?;
        let mut namebytes = vec![0u8; namelen.into()];
        decoder.reader().read(&mut namebytes[..])?;
        let name = OsString::from_vec(namebytes);
        let ftype: u8 = Decode::decode(decoder)?;
        let inumber: XfsIno = Decode::decode(decoder)?;
        Ok(Dir2SfEntry64 {
            namelen,
            offset,
            name,
            ftype,
            inumber,
        })
    }
}

// Since xfs-fuse is a read-only implementation, we needn't worry about
// preserving the on-disk size of the inode.  We can just convert all of the
// entries into the 64-bit type.
impl From<Dir2SfEntry32> for Dir2SfEntry64 {
    fn from(e32: Dir2SfEntry32) -> Self {
        Self {
            namelen: e32.namelen,
            offset: e32.offset,
            name: e32.name,
            ftype: e32.ftype,
            inumber: e32.inumber.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Dir2Sf {
    pub hdr: Dir2SfHdr,
    pub list: Vec<Dir2SfEntry64>,
}

impl Dir2Sf {
    /// Set the inode of this directory.  Annoyingly, we need to know it, but it
    /// isn't stored on disk in this header.
    pub fn set_ino(&mut self, ino: XfsIno) {
        self.list[0].inumber = ino;
    }
}

impl Decode for Dir2Sf {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let hdr: Dir2SfHdr = Decode::decode(decoder)?;

        let mut list = Vec::<Dir2SfEntry64>::new();
        // Alone out of all the directory types, SF directories to not store the
        // "." and ".." entries on disk.  We must synthesize them here.
        list.push(Dir2SfEntry64::new(b".", XFS_DIR3_FT_DIR, 1, u64::MAX));
        list.push(Dir2SfEntry64::new(b"..", XFS_DIR3_FT_DIR, 2, hdr.parent));
        for _i in 0..hdr.count {
            if hdr.i8count > 0 {
                list.push(Decode::decode(decoder)?);
            } else {
                let e32: Dir2SfEntry32 = Decode::decode(decoder)?;
                list.push(e32.into());
            }
        }

        Ok(Dir2Sf { hdr, list })
    }
}

impl<R: bincode::de::read::Reader + BufRead + Seek> Dir3<R> for Dir2Sf {
    fn lookup(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        name: &OsStr,
    ) -> Result<(FileAttr, u64), c_int> {
        let mut inode: Option<XfsIno> = None;

        for entry in self.list.iter() {
            if entry.name == name {
                inode = Some(entry.inumber);
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
    ) -> Result<(XfsIno, i64, FileType, OsString), c_int> {
        for entry in self.list.iter() {
            if i64::from(entry.offset) <= offset {
                continue;
            }

            let ino = entry.inumber;

            let kind = get_file_type(FileKind::Type(entry.ftype))?;

            let name = entry.name.to_owned();

            return Ok((ino, entry.offset as i64, kind, name));
        }

        Err(-1)
    }
}
