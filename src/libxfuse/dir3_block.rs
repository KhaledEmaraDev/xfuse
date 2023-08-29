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
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, Seek, SeekFrom};
use std::mem;
use std::time::{Duration, UNIX_EPOCH};

use super::S_IFMT;
use super::da_btree::hashname;
use super::definitions::*;
use super::dinode::Dinode;
use super::dir3::{
    Dir2DataEntry, Dir2DataUnused, Dir2LeafEntry, Dir3, Dir3DataHdr, XfsDir2Dataptr,
};
use super::sb::Sb;
use super::utils::{get_file_type, FileKind};

use byteorder::{BigEndian, ReadBytesExt};
use fuser::{FileAttr, FileType};
use libc::{c_int, ENOENT};

pub const XFS_DIR2_DATA_FD_COUNT: usize = 3;

#[derive(Debug)]
pub struct Dir2BlockTail {
    pub count: u32,
    pub stale: u32,
}

impl Dir2BlockTail {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir2BlockTail {
        let count = buf_reader.read_u32::<BigEndian>().unwrap();
        let stale = buf_reader.read_u32::<BigEndian>().unwrap();

        Dir2BlockTail { count, stale }
    }
}

#[derive(Debug)]
pub struct Dir2BlockDisk {
    pub hdr: Dir3DataHdr,
    // pub u: Vec<Dir2DataUnion>,
    pub leaf: Vec<Dir2LeafEntry>,
    pub tail: Dir2BlockTail,
}

impl Dir2BlockDisk {
    pub fn from<T: BufRead + Seek>(buf_reader: &mut T, offset: u64, size: u32) -> Dir2BlockDisk {
        buf_reader.seek(SeekFrom::Start(offset)).unwrap();

        let hdr = Dir3DataHdr::from(buf_reader.by_ref());

        buf_reader
            .seek(SeekFrom::Start(
                offset + (size as u64) - (mem::size_of::<Dir2BlockTail>() as u64),
            ))
            .unwrap();

        let tail = Dir2BlockTail::from(buf_reader.by_ref());

        let data_end = offset + (size as u64)
            - (mem::size_of::<Dir2BlockTail>() as u64)
            - ((mem::size_of::<Dir2LeafEntry>() as u64) * (tail.count as u64));

        buf_reader.seek(SeekFrom::Start(data_end)).unwrap();

        let mut leaf = Vec::<Dir2LeafEntry>::new();
        for _i in 0..tail.count {
            let leaf_entry = Dir2LeafEntry::from(buf_reader.by_ref());
            leaf.push(leaf_entry);
        }

        Dir2BlockDisk { hdr, leaf, tail }
    }

    pub fn get_data_end(&self, offset: u64, directory_block_size: u32) -> u64 {
        offset + (directory_block_size as u64)
            - (mem::size_of::<Dir2BlockTail>() as u64)
            - ((mem::size_of::<Dir2LeafEntry>() as u64) * (self.tail.count as u64))
    }
}

#[derive(Debug)]
pub struct Dir2Block {
    pub offset: u64,
    pub data_end: u64,
    pub hashes: HashMap<XfsDahash, XfsDir2Dataptr>,
}

impl Dir2Block {
    pub fn from<T: BufRead + Seek>(
        buf_reader: &mut T,
        superblock: &Sb,
        start_block: u64,
    ) -> Dir2Block {
        let offset = start_block * (superblock.sb_blocksize as u64);
        let dir_blk_size = superblock.sb_blocksize * (1 << superblock.sb_dirblklog);

        let dir_disk = Dir2BlockDisk::from(buf_reader.by_ref(), offset, dir_blk_size);

        let data_end = dir_disk.get_data_end(offset, dir_blk_size);

        let mut hashes = HashMap::new();
        for leaf_entry in dir_disk.leaf {
            hashes.insert(leaf_entry.hashval, leaf_entry.address);
        }

        Dir2Block {
            offset,
            data_end,
            hashes,
        }
    }
}

impl<R: BufRead + Seek> Dir3<R> for Dir2Block {
    fn lookup(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        name: &OsStr,
    ) -> Result<(FileAttr, u64), c_int> {
        let hash = hashname(name);
        if let Some(address) = self.hashes.get(&hash) {
            let address = (*address as u64) * 8;

            buf_reader
                .seek(SeekFrom::Start(self.offset + address))
                .unwrap();
            let entry = Dir2DataEntry::from(buf_reader.by_ref());

            let dinode = Dinode::from(buf_reader.by_ref(), super_block, entry.inumber);

            let kind = get_file_type(FileKind::Mode(dinode.di_core.di_mode))?;

            let attr = FileAttr {
                ino: entry.inumber,
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
        buf_reader: &mut R,
        _super_block: &Sb,
        offset: i64,
    ) -> Result<(XfsIno, i64, FileType, OsString), c_int> {
        let mut next = offset == 0;
        let offset = if offset == 0 {
            mem::size_of::<Dir3DataHdr>() as i64
        } else {
            offset
        };
        buf_reader
            .seek(SeekFrom::Start(self.offset + (offset as u64)))
            .unwrap();

        while buf_reader.stream_position().unwrap() < self.data_end {
            let freetag = buf_reader.read_u16::<BigEndian>().unwrap();
            buf_reader.seek(SeekFrom::Current(-2)).unwrap();

            if freetag == 0xffff {
                Dir2DataUnused::from(buf_reader.by_ref());
            } else if next {
                let entry = Dir2DataEntry::from(buf_reader.by_ref());

                let kind = get_file_type(FileKind::Type(entry.ftype))?;

                let name = entry.name;

                return Ok((entry.inumber, entry.tag.into(), kind, name));
            } else {
                let length = Dir2DataEntry::get_length(buf_reader.by_ref());
                buf_reader.seek(SeekFrom::Current(length)).unwrap();

                next = true;
            }
        }

        Err(ENOENT)
    }
}
