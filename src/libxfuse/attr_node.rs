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
    bmbt_rec::BmbtRec,
    da_btree::{hashname, XfsDa3Intnode},
    definitions::{XfsFileoff, XfsFsblock},
    sb::Sb,
    utils::decode_from
};

#[derive(Debug)]
pub struct AttrNode {
    pub bmx: Vec<BmbtRec>,
    pub node: XfsDa3Intnode,

    pub total_size: i64,
}

impl AttrNode {
    fn map_logical_block_to_fs_block(&self, block: XfsFileoff) -> XfsFsblock {
        for entry in self.bmx.iter().rev() {
            if block >= entry.br_startoff {
                return entry.br_startblock + (block - entry.br_startoff);
            }
        }

        panic!("Couldn't find logical block!");
    }

    // fn traverse_level_for_size<R: BufRead + Seek>(
    //     &mut self,
    //     buf_reader: &mut R,
    //     super_block: &Sb,
    //     hdr: XfsDa3NodeHdr,
    // ) -> u32 {
    //     let mut size: u32 = 0;

    //     let mut btree = Vec::<XfsDa3NodeEntry>::with_capacity(hdr.count as usize);
    //     for _i in 0..hdr.count {
    //         btree.push(XfsDa3NodeEntry::from(buf_reader.by_ref()))
    //     }

    //     if hdr.level == 1 {
    //         for entry in btree.iter() {
    //             let blk = self.map_logical_block_to_fs_block(u64::from(entry.before));
    //             let leaf_offset = blk * u64::from(super_block.sb_blocksize);
    //             buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();

    //             let mut leaf = AttrLeafblock::from(buf_reader.by_ref());

    //             size += leaf.get_total_size(buf_reader.by_ref(), leaf_offset);
    //         }
    //     } else {
    //         for entry in btree.iter() {
    //             let blk = self.map_logical_block_to_fs_block(u64::from(entry.before));
    //             buf_reader
    //                 .seek(SeekFrom::Start(blk * u64::from(super_block.sb_blocksize)))
    //                 .unwrap();

    //             let hdr = XfsDa3NodeHdr::from(buf_reader.by_ref());

    //             size += self.traverse_level_for_size(buf_reader.by_ref(), super_block, hdr);
    //         }
    //     }

    //     size
    // }

    // fn traverse_level_for_names<R: BufRead + Seek>(
    //     &mut self,
    //     buf_reader: &mut R,
    //     super_block: &Sb,
    //     hdr: XfsDa3NodeHdr,
    //     list: &mut Vec<u8>,
    // ) {
    //     let mut btree = Vec::<XfsDa3NodeEntry>::with_capacity(hdr.count as usize);
    //     for _i in 0..hdr.count {
    //         btree.push(XfsDa3NodeEntry::from(buf_reader.by_ref()))
    //     }

    //     if hdr.level == 1 {
    //         for entry in btree.iter() {
    //             let blk = self.map_logical_block_to_fs_block(u64::from(entry.before));
    //             let leaf_offset = blk * u64::from(super_block.sb_blocksize);
    //             buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();

    //             let mut leaf = AttrLeafblock::from(buf_reader.by_ref());

    //             leaf.list(buf_reader.by_ref(), list, leaf_offset);
    //         }
    //     } else {
    //         for entry in btree.iter() {
    //             let blk = self.map_logical_block_to_fs_block(u64::from(entry.before));
    //             buf_reader
    //                 .seek(SeekFrom::Start(blk * u64::from(super_block.sb_blocksize)))
    //                 .unwrap();

    //             let hdr = XfsDa3NodeHdr::from(buf_reader.by_ref());

    //             self.traverse_level_for_names(buf_reader.by_ref(), super_block, hdr, list);
    //         }
    //     }
    // }
}

impl<R: Reader + BufRead + Seek> Attr<R> for AttrNode {
    fn get_total_size(&mut self, buf_reader: &mut R, super_block: &Sb) -> u32 {
        if self.total_size == -1 {
            let mut total_size: u32 = 0;

            let blk = self
                .node
                .first_block(buf_reader.by_ref(), super_block, |block, _| {
                    self.map_logical_block_to_fs_block(block.into())
                });
            let leaf_offset = blk * u64::from(super_block.sb_blocksize);

            buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();

            let mut node: AttrLeafblock = decode_from(buf_reader.by_ref()).unwrap();
            total_size += node.get_total_size();

            while node.hdr.info.forw != 0 {
                let lfblk = self.map_logical_block_to_fs_block(node.hdr.info.forw.into());
                let lfofs = lfblk * u64::from(super_block.sb_blocksize);
                buf_reader.seek(SeekFrom::Start(lfofs)).unwrap();
                node = decode_from(buf_reader.by_ref()).unwrap();
                total_size += node.get_total_size();
            }

            self.total_size = i64::from(total_size);
        }

        self.total_size.try_into().unwrap()
    }

    fn get_size(&self, buf_reader: &mut R, super_block: &Sb, name: &OsStr) -> Result<u32, libc::c_int> {
        let blocksize = u64::from(super_block.sb_blocksize);
        let hash = hashname(name);

        let blk = self.node.lookup(buf_reader.by_ref(), super_block, hash, |block, _| {
            self.map_logical_block_to_fs_block(block.into())
        }).map_err(|e| {
            if e == libc::ENOENT {
                libc::ENOATTR
            } else {
                e
            }
        })?;
        let leaf_offset = blk * blocksize;

        buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();
        loop {
            let leaf: AttrLeafblock = decode_from(buf_reader.by_ref()).unwrap();

            match leaf.get_size(hash) {
                Ok(l) => return Ok(l),
                Err(libc::ENOATTR) if leaf.entries.last().map(|e| e.hashval) == Some(hash) => {
                    let forw = leaf.hdr.info.forw.into();
                    let next_leaf_fsblock = self.map_logical_block_to_fs_block(forw);
                    buf_reader.seek(SeekFrom::Start(next_leaf_fsblock * blocksize)).unwrap();
                    continue;
                },
                Err(e) => return Err(e)
            }
        }
    }

    fn list(&mut self, buf_reader: &mut R, super_block: &Sb) -> Vec<u8> {
        let mut list: Vec<u8> =
            Vec::with_capacity(self.get_total_size(buf_reader.by_ref(), super_block) as usize);

        let blk = self
            .node
            .first_block(buf_reader.by_ref(), super_block, |block, _| {
                self.map_logical_block_to_fs_block(block.into())
            });
        let leaf_offset = blk * u64::from(super_block.sb_blocksize);

        buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();

        let mut leaf: AttrLeafblock = decode_from(buf_reader.by_ref()).unwrap();
        leaf.list(&mut list);

        while leaf.hdr.info.forw != 0 {
            let lfblk = self.map_logical_block_to_fs_block(leaf.hdr.info.forw.into());
            let lfofs = lfblk * u64::from(super_block.sb_blocksize);
            buf_reader.seek(SeekFrom::Start(lfofs)).unwrap();
            leaf = decode_from(buf_reader.by_ref()).unwrap();
            leaf.list(&mut list);
        }

        list
    }

    fn get(&self, buf_reader: &mut R, super_block: &Sb, name: &OsStr) -> Result<Vec<u8>, i32> {
        let hash = hashname(name);

        let blk = self.node.lookup(buf_reader.by_ref(), super_block, hash, |block, _| {
            self.map_logical_block_to_fs_block(block.into())
        })?;
        let leaf_offset = blk * u64::from(super_block.sb_blocksize);

        buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();
        let leaf: AttrLeafblock = decode_from(buf_reader.by_ref()).unwrap();

        Ok(leaf.get(
            buf_reader.by_ref(),
            hash,
            |block, _| self.map_logical_block_to_fs_block(block),
        ))
    }
}
