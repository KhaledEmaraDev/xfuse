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
    btree: BtreeRoot,
    total_size: i64,
    node: XfsDa3Intnode,
    /// A cache of leaf blocks, indexed by directory block number
    leaves: RefCell<BTreeMap<XfsDablk, AttrLeafblock>>
}

impl AttrBtree {
    pub fn new<R>(buf_reader: &mut R, sb: &Sb, btree: BtreeRoot) -> Self
        where R: bincode::de::read::Reader + BufRead + Seek
    {
        let fsblk = btree.map_block(buf_reader.by_ref(), 0).unwrap().0.unwrap();
        buf_reader.seek(SeekFrom::Start(sb.fsb_to_offset(fsblk))).unwrap();

        let node = XfsDa3Intnode::from(buf_reader.by_ref());

        Self {
            btree,
            total_size: -1,
            node,
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
        -> Result<impl std::ops::DerefMut<Target=AttrLeafblock> + 'a, i32>
        where R: Reader + BufRead + Seek
    {
        let mut cache_guard = self.leaves.borrow_mut();
        let entry = cache_guard.entry(dblock);
        if matches!(entry, Entry::Vacant(_)) {
            let fsblock = self.map_dblock(buf_reader.by_ref(), dblock)?;
            let leaf_offset = sb.fsb_to_offset(fsblock);
            buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();
            let leaf: AttrLeafblock = decode_from(buf_reader.by_ref()).unwrap();
            entry.or_insert(leaf);
        }
        Ok(std::cell::RefMut::map(cache_guard, |v| v.get_mut(&dblock).unwrap()))
    }
}

impl Attr for AttrBtree {
    fn get_total_size<R: Reader + BufRead + Seek>(&mut self, buf_reader: &mut R, super_block: &Sb) -> u32 {
        if self.total_size == -1 {
            let mut total_size: u32 = 0;

            // Now read the first leaf block of the btree
            let mut dablk = self.node.first_block(buf_reader.by_ref(), super_block, |block, reader| {
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

        let mut dablk = self.node.first_block(buf_reader.by_ref(), super_block, |block, reader| {
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

        let dablk = self.node.lookup(buf_reader.by_ref(), super_block, hash, |block, reader| {
            self.map_dblock(reader.by_ref(), block).unwrap()
        }).map_err(|e| if e == libc::ENOENT {libc::ENOATTR} else {e})?;
        let mut leaf = self.read_leaf(buf_reader.by_ref(), super_block, dablk)?;

        leaf.get(
            buf_reader.by_ref(),
            hash,
            |block, reader| self.map_dblock(reader.by_ref(), block).unwrap(),
        ).map(Vec::from)
    }
}
