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
    convert::TryInto,
    ffi::OsStr,
    io::{BufRead, Seek, SeekFrom},
};

use bincode::de::read::Reader;

use super::{
    attr::{Attr, AttrLeafblock},
    btree::{Btree, BtreeRoot},
    definitions::{XfsFileoff, XfsFsblock},
    da_btree::{hashname, XfsDa3Intnode},
    sb::Sb,
    utils::decode_from
};

#[derive(Debug)]
pub struct AttrBtree {
    pub btree: BtreeRoot,

    pub total_size: i64,
}

impl AttrBtree {
    // Attribute blocks always have the same size, so we don't need to return the extent length.
    // They also need to return a different errno.
    fn map_block<R: bincode::de::read::Reader + BufRead + Seek>(
        &self,
        buf_reader: &mut R,
        logical_block: XfsFileoff,
    ) -> Result<XfsFsblock, i32> {
        self.btree.map_block(buf_reader, logical_block)?.0.ok_or(libc::ENOATTR)
    }
}

impl<R: Reader + BufRead + Seek> Attr<R> for AttrBtree {
    fn get_total_size(&mut self, buf_reader: &mut R, super_block: &Sb) -> u32 {
        if self.total_size == -1 {
            let mut total_size: u32 = 0;

            // Read the first intermediate block of the btree
            let intermediate_blk = self.map_block(buf_reader.by_ref(), 0)
                .unwrap();
            buf_reader.seek(SeekFrom::Start(super_block.fsb_to_offset(intermediate_blk))).unwrap();

            let node = XfsDa3Intnode::from(buf_reader.by_ref());

            // Now read the first leaf block of the btree
            let lfblk0 = node.first_block(buf_reader.by_ref(), super_block, |block, reader| {
                self.map_block(reader.by_ref(), block.into()).unwrap()
            });
            let leaf_offset = super_block.fsb_to_offset(lfblk0);

            buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();

            let mut leaf: AttrLeafblock = decode_from(buf_reader.by_ref()).unwrap();
            total_size += leaf.get_total_size();

            while leaf.hdr.info.forw != 0 {
                let lfblk = self.map_block(buf_reader.by_ref(), leaf.hdr.info.forw.into())
                    .unwrap();
                let lfofs = super_block.fsb_to_offset(lfblk);
                buf_reader.seek(SeekFrom::Start(lfofs)).unwrap();
                leaf = decode_from(buf_reader.by_ref()).unwrap();
                total_size += leaf.get_total_size();
            }

            self.total_size = i64::from(total_size);
        }

        self.total_size.try_into().unwrap()
    }

    fn list(&mut self, buf_reader: &mut R, super_block: &Sb) -> Vec<u8> {
        let mut list: Vec<u8> =
            Vec::with_capacity(self.get_total_size(buf_reader.by_ref(), super_block) as usize);

        let blk = self.map_block(buf_reader.by_ref(), 0).unwrap();
        buf_reader.seek(SeekFrom::Start(super_block.fsb_to_offset(blk))).unwrap();

        let node = XfsDa3Intnode::from(buf_reader.by_ref());

        let blk = node.first_block(buf_reader.by_ref(), super_block, |block, reader| {
            self.map_block(reader.by_ref(), block.into()).unwrap()
        });
        let leaf_offset = super_block.fsb_to_offset(blk);

        buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();

        let mut leaf: AttrLeafblock = decode_from(buf_reader.by_ref()).unwrap();
        leaf.list(&mut list);

        while leaf.hdr.info.forw != 0 {
            let lfblk = self.map_block(buf_reader.by_ref(), leaf.hdr.info.forw.into())
                .unwrap();
            let lfofs = super_block.fsb_to_offset(lfblk);
            buf_reader.seek(SeekFrom::Start(lfofs)).unwrap();
            leaf = decode_from(buf_reader.by_ref()).unwrap();
            leaf.list(&mut list);
        }

        list
    }

    fn get(&self, buf_reader: &mut R, super_block: &Sb, name: &OsStr) -> Result<Vec<u8>, i32> {
        let hash = hashname(name);

        let blk = self.map_block(buf_reader.by_ref(), 0)?;
        buf_reader.seek(SeekFrom::Start(super_block.fsb_to_offset(blk))).unwrap();

        let node = XfsDa3Intnode::from(buf_reader.by_ref());

        let blk = node.lookup(buf_reader.by_ref(), super_block, hash, |block, reader| {
            self.map_block(reader.by_ref(), block.into()).unwrap()
        }).map_err(|e| if e == libc::ENOENT {libc::ENOATTR} else {e})?;
        let leaf_offset = super_block.fsb_to_offset(blk);

        buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();

        let leaf: AttrLeafblock = decode_from(buf_reader.by_ref()).unwrap();

        return leaf.get(
            buf_reader.by_ref(),
            hash,
            |block, reader| self.map_block(reader.by_ref(), block).unwrap(),
        );
    }
}
