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
use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, Seek, SeekFrom};
use std::mem;
use std::time::{Duration, UNIX_EPOCH};

use super::S_IFMT;
use super::bmbt_rec::BmbtRec;
use super::da_btree::hashname;
use super::definitions::*;
use super::dinode::Dinode;
use super::dir3::{Dir2Data, Dir2DataEntry, Dir2DataUnused, Dir2LeafDisk, Dir3, Dir3DataHdr};
use super::sb::Sb;
use super::utils::{decode, get_file_type, FileKind};

use fuser::{FileAttr, FileType};
use libc::{c_int, ENOENT};

#[derive(Debug)]
pub struct Dir2Leaf {
    pub entries: Vec<Dir2Data>,
    pub leaf: Dir2LeafDisk,
    pub entry_size: u32,
    /// An cache of the last block and its index read by lookup or readdir.
    block_cache: RefCell<Option<(usize, Vec<u8>)>>
}

impl Dir2Leaf {
    pub fn from<T: bincode::de::read::Reader + BufRead + Seek>(
        buf_reader: &mut T,
        superblock: &Sb,
        bmx: &[BmbtRec],
    ) -> Dir2Leaf {
        let mut entries = Vec::<Dir2Data>::new();
        for record in bmx.iter().take(bmx.len() - 1) {
            let entry = Dir2Data::from(buf_reader.by_ref(), superblock, record.br_startblock);
            entries.push(entry);
        }

        let leaf_extent = bmx.last().unwrap();
        let offset = superblock.fsb_to_offset(leaf_extent.br_startblock);
        let entry_size = superblock.sb_blocksize * (1 << superblock.sb_dirblklog);

        let leaf = Dir2LeafDisk::from(buf_reader, offset, entry_size as usize);
        assert_eq!(leaf.hdr.info.magic, XFS_DIR3_LEAF1_MAGIC);

        Dir2Leaf {
            entries,
            leaf,
            entry_size,
            block_cache: RefCell::new(None)
        }
    }
}

impl<R: bincode::de::read::Reader + BufRead + Seek> Dir3<R> for Dir2Leaf {
    fn lookup(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        name: &OsStr,
    ) -> Result<(FileAttr, u64), c_int> {
        let hash = hashname(name);

        let address = self.leaf.get_address(hash)? * 8;
        let idx = (address / self.entry_size) as usize;
        let address = (address % self.entry_size) as usize;

        if idx >= self.entries.len() {
            return Err(ENOENT);
        }
        let entry: &Dir2Data = &self.entries[idx];

        let mut cache_guard = self.block_cache.borrow_mut();
        if cache_guard.is_none() || cache_guard.as_ref().unwrap().0 != idx {
            let mut raw = vec![0u8; self.entry_size as usize];
            buf_reader
                .seek(SeekFrom::Start(entry.offset))
                .unwrap();
            buf_reader.read_exact(&mut raw).unwrap();
            *cache_guard = Some((idx, raw));
        }
        let raw = &cache_guard.as_ref().unwrap().1;

        let entry: Dir2DataEntry = decode(&raw[address..]).unwrap().0;

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
    }

    fn next(
        &self,
        buf_reader: &mut R,
        _super_block: &Sb,
        offset: i64,
    ) -> Result<(XfsIno, i64, FileType, OsString), c_int> {
        let offset = offset as u64;
        // In V5 Inodes can contain up to 21 Extents
        let mut idx: usize = (offset >> (64 - 8)) as usize;
        if idx >= self.entries.len() {
            return Err(ENOENT);
        }
        let mut entry: &Dir2Data = &self.entries[idx];

        let mut offset = (offset & ((1 << (64 - 8)) - 1)) as usize;

        let mut next = offset == 0;
        loop {
            offset = if offset == 0 {
                mem::size_of::<Dir3DataHdr>()
            } else {
                offset
            };

            let mut cache_guard = self.block_cache.borrow_mut();
            if cache_guard.is_none() || cache_guard.as_ref().unwrap().0 != idx {
                let mut raw = vec![0u8; self.entry_size as usize];
                buf_reader
                    .seek(SeekFrom::Start(entry.offset))
                    .unwrap();
                buf_reader.read_exact(&mut raw).unwrap();
                *cache_guard = Some((idx, raw));
            }
            let raw = &cache_guard.as_ref().unwrap().1;

            while offset < raw.len()
            {
                let freetag: u16 = decode(&raw[offset..]).unwrap().0;

                if freetag == 0xffff {
                    let (_, length) = decode::<Dir2DataUnused>(&raw[offset..])
                        .unwrap();
                    offset += length;
                } else if next {
                    let entry: Dir2DataEntry = decode(&raw[offset..]).unwrap().0;

                    let kind = get_file_type(FileKind::Type(entry.ftype))?;

                    let name = entry.name;

                    let tag = ((idx as u64) << (64 - 8)) | (entry.tag as u64);

                    return Ok((entry.inumber, tag as i64, kind, name));
                } else {
                    let length = Dir2DataEntry::get_length(&raw[offset..]);
                    offset += length as usize;

                    next = true;
                }
            }

            idx += 1;

            if idx >= self.entries.len() {
                break;
            }
            entry = &self.entries[idx];

            offset = 0;
        }

        Err(ENOENT)
    }
}
