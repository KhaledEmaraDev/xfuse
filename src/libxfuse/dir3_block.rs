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
use std::{
    cell::RefCell,
    io::{BufRead, Seek, SeekFrom},
    ops::Deref
};

use super::definitions::*;
use super::dir3::{Dir2LeafEntry, Dir3, Dir2DataHdr, Dir3DataHdr, XfsDir2Dataptr};
use super::sb::Sb;
use super::utils::decode;

use bincode::{Decode, de::read::Reader};

#[derive(Debug, Decode)]
pub struct Dir2BlockTail {
    count: u32,
    _stale: u32,
}

impl Dir2BlockTail {
    /// On-disk size in bytes
    pub const SIZE: usize = 8;
}

#[derive(Debug)]
pub struct Dir2BlockDisk {
    pub leaf: Vec<Dir2LeafEntry>,
    pub tail: Dir2BlockTail,
    raw: Vec<u8>,
}

impl Dir2BlockDisk {
    pub fn new<T>(buf_reader: &mut T, offset: u64, size: u32) -> Dir2BlockDisk
        where T: BufRead + Seek
    {
        buf_reader.seek(SeekFrom::Start(offset)).unwrap();
        let mut raw = vec![0u8; size as usize];
        buf_reader.read_exact(&mut raw).unwrap();

        let magic: u32 = decode(&raw[..]).unwrap().0;
        match magic {
            XFS_DIR2_BLOCK_MAGIC => {
                let hdr: Dir2DataHdr = decode(&raw[..]).unwrap().0;
                assert_eq!(hdr.magic, XFS_DIR2_BLOCK_MAGIC);
            },
            XFS_DIR3_BLOCK_MAGIC => {
                let hdr: Dir3DataHdr = decode(&raw[..]).unwrap().0;
                assert_eq!(hdr.hdr.magic, XFS_DIR3_BLOCK_MAGIC);
            },
            _ => panic!("Unknown magic number for block directory {:#x}", magic)
        }

        let tail_offset = (size as usize) - Dir2BlockTail::SIZE;
        let tail: Dir2BlockTail = decode(&raw[tail_offset..]).unwrap().0;

        let mut leaf_offset = tail_offset - Dir2LeafEntry::SIZE * tail.count as usize;

        let mut leaf = Vec::with_capacity(tail.count as usize);
        for _i in 0..tail.count {
            leaf.push(decode(&raw[leaf_offset..]).unwrap().0);
            leaf_offset += Dir2LeafEntry::SIZE;
        }

        Dir2BlockDisk { leaf, tail, raw }
    }

    /// get the length of the raw data region
    fn get_data_len(&self, directory_block_size: u32) -> u64 {
        directory_block_size as u64
            - Dir2BlockTail::SIZE as u64
            - Dir2LeafEntry::SIZE as u64 * (self.tail.count as u64)
    }
}

#[derive(Debug)]
pub struct Dir2Block {
    ents: Vec<Dir2LeafEntry>,
    raw: Box<[u8]>,
}

impl Dir2Block {
    pub fn new<T: BufRead + Seek>(
        buf_reader: &mut T,
        superblock: &Sb,
        start_block: u64,
    ) -> Dir2Block {
        let offset = superblock.fsb_to_offset(start_block);
        let dir_blk_size = superblock.sb_blocksize << superblock.sb_dirblklog;

        let dir_disk = Dir2BlockDisk::new(buf_reader.by_ref(), offset, dir_blk_size);

        let data_len = dir_disk.get_data_len(dir_blk_size);
        assert!(data_len as usize <= dir_disk.raw.len());
        let mut raw = dir_disk.raw;
        raw.truncate(data_len as usize);

        Dir2Block {
            raw: raw.into(),
            ents: dir_disk.leaf
        }
    }
}

impl Dir3 for Dir2Block {
    fn get_addresses<'a, R>(&'a self, _buf_reader: &'a RefCell<&'a mut R>, hash: XfsDahash)
        -> Box<dyn Iterator<Item=XfsDir2Dataptr> + 'a>
            where R: Reader + BufRead + Seek + 'a
    {
        let i = self.ents.partition_point(|ent| ent.hashval < hash);
        let l = self.ents.len();
        let j = (i..l).find(|x| self.ents[*x].hashval > hash).unwrap_or(l);
        Box::new(self.ents[i..j].iter().map(|ent| ent.address << 3))
    }

    fn read_dblock<'a, R>(&'a self, _buf_reader: R, _sb: &Sb, dblock: XfsDablk)
        -> Result<Box<dyn Deref<Target=[u8]> + 'a>, i32>
        where R: Reader + BufRead + Seek
    {
        assert_eq!(dblock, 0);
        Ok(Box::new(&self.raw[..]))
    }
}
