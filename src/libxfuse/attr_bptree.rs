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
    collections::{BTreeMap, btree_map::Entry},
    convert::TryInto,
    ffi::OsStr,
    io::{BufRead, Seek, SeekFrom},
};

use bincode::de::read::Reader;

use super::{
    attr::{Attr, AttrLeafblock},
    btree::{Btree, BtreeRoot},
    definitions::{XfsDablk, XfsFsblock},
    da_btree::{hashname, XfsDa3Intnode},
    sb::Sb,
    utils::decode_from
};

#[derive(Debug)]
pub struct AttrBtree {
    pub btree: BtreeRoot,
    pub total_size: i64,
    /// A cache of leaf blocks, indexed by directory block number
    leaves: RefCell<BTreeMap<XfsDablk, AttrLeafblock>>
}

impl AttrBtree {
    pub fn new(btree: BtreeRoot) -> Self {
        Self {
            btree,
            total_size: -1,
            leaves: Default::default()
        }
    }

    // Attribute blocks always have the same size, so we don't need to return the extent length.
    // They also need to return a different errno.
    fn map_dblock<R: bincode::de::read::Reader + BufRead + Seek>(
        &self,
        buf_reader: &mut R,
        logical_block: XfsDablk,
    ) -> Result<XfsFsblock, i32> {
        self.btree.map_block(buf_reader, logical_block.into())?.0.ok_or(libc::ENOATTR)
    }

    /// Read the AttrLeafblock located at the given directory block number
    fn read_leaf<'a, R>(&'a self, buf_reader: &mut R, sb: &Sb, dblock: XfsDablk)
        -> Result<impl std::ops::Deref<Target=AttrLeafblock> + 'a, i32>
        where R: Reader + BufRead + Seek
    {
        let mut cache_guard = self.leaves.borrow_mut();
        let entry = cache_guard.entry(dblock);
        if matches!(entry, Entry::Vacant(_)) {
            let fsblock = self.map_dblock(buf_reader.by_ref(), dblock)?;
            let leaf_offset = sb.fsb_to_offset(fsblock);
            buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();
            let node: AttrLeafblock = decode_from(buf_reader.by_ref()).unwrap();
            entry.or_insert(node);
        }
        // Annoyingly, there's no function to downgrade a RefMut into a Ref.
        drop(cache_guard);
        let cache_guard = self.leaves.borrow();
        Ok(std::cell::Ref::map(cache_guard, |v| &v[&dblock]))
    }
}

impl Attr for AttrBtree {
    fn get_total_size<R: Reader + BufRead + Seek>(&mut self, buf_reader: &mut R, super_block: &Sb) -> u32 {
        if self.total_size == -1 {
            let mut total_size: u32 = 0;

            // Read the first intermediate block of the btree
            let intermediate_blk = self.map_dblock(buf_reader.by_ref(), 0)
                .unwrap();
            buf_reader.seek(SeekFrom::Start(super_block.fsb_to_offset(intermediate_blk))).unwrap();

            let node = XfsDa3Intnode::from(buf_reader.by_ref());

            // Now read the first leaf block of the btree
            let mut dablk = node.first_block(buf_reader.by_ref(), super_block, |block, reader| {
                self.map_dblock(reader.by_ref(), block).unwrap()
            });
            while dablk != 0 {
                let leaf = self.read_leaf(buf_reader.by_ref(), super_block, dablk).unwrap();
                total_size += leaf.get_total_size();
                dablk = leaf.hdr.info.forw;
            }

            self.total_size = i64::from(total_size);
        }

        self.total_size.try_into().unwrap()
    }

    fn list<R: Reader + BufRead + Seek>(&mut self, buf_reader: &mut R, super_block: &Sb) -> Vec<u8> {
        let mut list: Vec<u8> =
            Vec::with_capacity(self.get_total_size(buf_reader.by_ref(), super_block) as usize);

        let blk = self.map_dblock(buf_reader.by_ref(), 0).unwrap();
        buf_reader.seek(SeekFrom::Start(super_block.fsb_to_offset(blk))).unwrap();

        let node = XfsDa3Intnode::from(buf_reader.by_ref());

        let mut dablk = node.first_block(buf_reader.by_ref(), super_block, |block, reader| {
            self.map_dblock(reader.by_ref(), block).unwrap()
        });
        while dablk != 0 {
            let leaf = self.read_leaf(buf_reader.by_ref(), super_block, dablk).unwrap();
            (*leaf).list(&mut list);
            dablk = leaf.hdr.info.forw;
        }

        list
    }

    fn get<R>(&mut self, buf_reader: &mut R, super_block: &Sb, name: &OsStr) -> Result<Vec<u8>, i32>
        where R: Reader + BufRead + Seek
    {
        let hash = hashname(name);

        let blk = self.map_dblock(buf_reader.by_ref(), 0)?;
        buf_reader.seek(SeekFrom::Start(super_block.fsb_to_offset(blk))).unwrap();

        let node = XfsDa3Intnode::from(buf_reader.by_ref());

        let dablk = node.lookup(buf_reader.by_ref(), super_block, hash, |block, reader| {
            self.map_dblock(reader.by_ref(), block).unwrap()
        }).map_err(|e| if e == libc::ENOENT {libc::ENOATTR} else {e})?;
        let leaf = self.read_leaf(buf_reader.by_ref(), super_block, dablk)?;

        return leaf.get(
            buf_reader.by_ref(),
            hash,
            |block, reader| self.map_dblock(reader.by_ref(), block).unwrap(),
        );
    }
}
