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
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, Seek, SeekFrom};
use std::mem;

use super::da_btree::hashname;
use super::definitions::*;
use super::dinode::Dinode;
use super::dir3::{
    Dir2DataEntry, Dir2DataUnused, Dir2LeafEntry, Dir3, Dir3DataHdr, XfsDir2Dataptr,
};
use super::sb::Sb;
use super::utils::{decode, get_file_type, FileKind};

use bincode::Decode;
use fuser::{FileAttr, FileType};
use libc::{c_int, ENOENT};

pub const XFS_DIR2_DATA_FD_COUNT: usize = 3;

#[derive(Debug, Decode)]
pub struct Dir2BlockTail {
    pub count: u32,
    pub stale: u32,
}

impl Dir2BlockTail {
    /// On-disk size in bytes
    pub const SIZE: usize = 8;
}

#[derive(Debug)]
pub struct Dir2BlockDisk {
    pub hdr: Dir3DataHdr,
    // pub u: Vec<Dir2DataUnion>,
    pub leaf: Vec<Dir2LeafEntry>,
    pub tail: Dir2BlockTail,
    raw: Vec<u8>,
}

impl Dir2BlockDisk {
    pub fn from<T: BufRead + Seek>(buf_reader: &mut T, offset: u64, size: u32) -> Dir2BlockDisk {
        buf_reader.seek(SeekFrom::Start(offset)).unwrap();
        let mut raw = vec![0u8; size as usize];
        buf_reader.read_exact(&mut raw).unwrap();

        let hdr: Dir3DataHdr = decode(&raw[..]).unwrap().0;

        let tail_offset = (size as usize) - Dir2BlockTail::SIZE;
        let tail: Dir2BlockTail = decode(&raw[tail_offset..]).unwrap().0;

        let mut leaf_offset = tail_offset - Dir2LeafEntry::SIZE * tail.count as usize;

        let mut leaf = Vec::with_capacity(tail.count as usize);
        for _i in 0..tail.count {
            leaf.push(decode(&raw[leaf_offset..]).unwrap().0);
            leaf_offset += Dir2LeafEntry::SIZE;
        }

        Dir2BlockDisk { hdr, leaf, tail, raw }
    }

    /// get the length of the raw data region
    pub fn get_data_len(&self, directory_block_size: u32) -> u64 {
        (directory_block_size as u64)
            - (mem::size_of::<Dir2BlockTail>() as u64)
            - ((mem::size_of::<Dir2LeafEntry>() as u64) * (self.tail.count as u64))
    }
}

#[derive(Debug)]
pub struct Dir2Block {
    pub offset: u64,
    pub hashes: HashMap<XfsDahash, XfsDir2Dataptr>,
    raw: Box<[u8]>,
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

        let data_len = dir_disk.get_data_len(dir_blk_size);
        assert!(data_len as usize <= dir_disk.raw.len());
        let mut raw = dir_disk.raw;
        raw.truncate(data_len as usize);

        let mut hashes = HashMap::new();
        for leaf_entry in dir_disk.leaf {
            hashes.insert(leaf_entry.hashval, leaf_entry.address);
        }

        Dir2Block {
            offset,
            hashes,
            raw: raw.into()
        }
    }
}

impl<R: bincode::de::read::Reader + BufRead + Seek> Dir3<R> for Dir2Block {
    fn lookup(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        name: &OsStr,
    ) -> Result<(FileAttr, u64), c_int> {
        let hash = hashname(name);
        if let Some(address) = self.hashes.get(&hash) {
            let address = (*address as usize) * 8;

            let entry: Dir2DataEntry = decode(&self.raw[address..]).unwrap().0;

            let dinode = Dinode::from(buf_reader.by_ref(), super_block, entry.inumber);

            let attr = dinode.di_core.stat(entry.inumber)?;

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
        let mut next = offset == 0;
        let mut offset = if offset == 0 {
            mem::size_of::<Dir3DataHdr>()
        } else {
            offset as usize
        };

        while offset < self.raw.len() {
            let freetag: u16 = decode(&self.raw[offset..]).unwrap().0;

            if freetag == 0xffff {
                let (_, length) = decode::<Dir2DataUnused>(&self.raw[offset..])
                    .unwrap();
                offset += length;
            } else if next {
                let entry: Dir2DataEntry = decode(&self.raw[offset..]).unwrap().0;

                let kind = get_file_type(FileKind::Type(entry.ftype))?;

                let name = entry.name;

                return Ok((entry.inumber, entry.tag.into(), kind, name));
            } else {
                let length = Dir2DataEntry::get_length(&self.raw[offset..]);
                offset += length as usize;

                next = true;
            }
        }

        Err(ENOENT)
    }
}
