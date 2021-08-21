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
use std::{
    convert::TryInto,
    io::{BufRead, Seek, SeekFrom},
};

use super::{
    attr::{Attr, AttrLeafblock},
    btree::Btree,
    da_btree::{hashname, XfsDa3Intnode},
    sb::Sb,
};

#[derive(Debug)]
pub struct AttrBtree {
    pub btree: Btree,

    pub total_size: i64,
}

impl<R: BufRead + Seek> Attr<R> for AttrBtree {
    fn get_total_size(&mut self, buf_reader: &mut R, super_block: &Sb) -> u32 {
        if self.total_size == -1 {
            let mut total_size: u32 = 0;

            let blk = self.btree.map_block(buf_reader.by_ref(), &super_block, 0);
            buf_reader
                .seek(SeekFrom::Start(blk * u64::from(super_block.sb_blocksize)))
                .unwrap();

            let node = XfsDa3Intnode::from(buf_reader.by_ref(), super_block);

            let blk = node.first_block(buf_reader.by_ref(), &super_block, |block, reader| {
                self.btree
                    .map_block(reader.by_ref(), &super_block, block.into())
            });
            let leaf_offset = blk * u64::from(super_block.sb_blocksize);

            buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();

            let mut node = AttrLeafblock::from(buf_reader.by_ref(), super_block);
            total_size += node.get_total_size(buf_reader.by_ref(), leaf_offset);

            while node.hdr.info.forw != 0 {
                node = AttrLeafblock::from(buf_reader.by_ref(), super_block);
                total_size += node.get_total_size(buf_reader.by_ref(), leaf_offset);
            }

            self.total_size = i64::from(total_size);
        }

        self.total_size.try_into().unwrap()
    }

    fn get_size(&self, buf_reader: &mut R, super_block: &Sb, name: &str) -> u32 {
        let hash = hashname(name);

        let blk = self.btree.map_block(buf_reader.by_ref(), &super_block, 0);
        buf_reader
            .seek(SeekFrom::Start(blk * u64::from(super_block.sb_blocksize)))
            .unwrap();

        let node = XfsDa3Intnode::from(buf_reader.by_ref(), super_block);

        let blk = node.lookup(buf_reader.by_ref(), &super_block, hash, |block, reader| {
            self.btree
                .map_block(reader.by_ref(), &super_block, block.into())
        });
        let leaf_offset = blk * u64::from(super_block.sb_blocksize);

        buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();

        let leaf = AttrLeafblock::from(buf_reader.by_ref(), super_block);

        leaf.get_size(buf_reader.by_ref(), hash, leaf_offset)
    }

    fn list(&mut self, buf_reader: &mut R, super_block: &Sb) -> Vec<u8> {
        let mut list: Vec<u8> =
            Vec::with_capacity(self.get_total_size(buf_reader.by_ref(), &super_block) as usize);

        let blk = self.btree.map_block(buf_reader.by_ref(), &super_block, 0);
        buf_reader
            .seek(SeekFrom::Start(blk * u64::from(super_block.sb_blocksize)))
            .unwrap();

        let node = XfsDa3Intnode::from(buf_reader.by_ref(), super_block);

        let blk = node.first_block(buf_reader.by_ref(), &super_block, |block, reader| {
            self.btree
                .map_block(reader.by_ref(), &super_block, block.into())
        });
        let leaf_offset = blk * u64::from(super_block.sb_blocksize);

        buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();

        let mut leaf = AttrLeafblock::from(buf_reader.by_ref(), super_block);
        leaf.list(buf_reader.by_ref(), &mut list, leaf_offset);

        while leaf.hdr.info.forw != 0 {
            leaf = AttrLeafblock::from(buf_reader.by_ref(), super_block);
            leaf.list(buf_reader.by_ref(), &mut list, leaf_offset);
        }

        list
    }

    fn get(&self, buf_reader: &mut R, super_block: &Sb, name: &str) -> Vec<u8> {
        let hash = hashname(name);

        let blk = self.btree.map_block(buf_reader.by_ref(), &super_block, 0);
        buf_reader
            .seek(SeekFrom::Start(blk * u64::from(super_block.sb_blocksize)))
            .unwrap();

        let node = XfsDa3Intnode::from(buf_reader.by_ref(), super_block);

        let blk = node.lookup(buf_reader.by_ref(), &super_block, hash, |block, reader| {
            self.btree
                .map_block(reader.by_ref(), &super_block, block.into())
        });
        let leaf_offset = blk * u64::from(super_block.sb_blocksize);

        buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();

        let leaf = AttrLeafblock::from(buf_reader.by_ref(), super_block);

        leaf.get(
            buf_reader.by_ref(),
            super_block,
            hash,
            leaf_offset,
            |block, reader| self.btree.map_block(reader.by_ref(), &super_block, block),
        )
    }
}
