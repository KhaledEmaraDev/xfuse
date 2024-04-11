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
    cell::{Ref, RefCell},
    collections::{BTreeMap, btree_map::Entry},
    io::{BufRead, Seek, SeekFrom},
    ops::Deref
};

use bincode::de::read::Reader;

use super::bmbt_rec::Bmx;
use super::definitions::*;
use super::dir3::{Dir3, NodeLikeDir, XfsDir2Dataptr};
use super::sb::Sb;

#[derive(Debug)]
pub struct Dir2Node {
    pub bmx: Bmx,
    /// A cache of directory blocks, indexed by directory block number
    blocks: RefCell<BTreeMap<XfsDablk, Vec<u8>>>
}

impl Dir2Node {
    pub fn from(bmx: Bmx) -> Dir2Node {
        let blocks = Default::default();
        Dir2Node {
            bmx,
            blocks
        }
    }

    /// Read one directory block and return a reference to its data.
    fn read_fsblock<R>(&self, mut buf_reader: R, sb: &Sb, fsblock: XfsFsblock)
        -> Result<Vec<u8>, i32>
        where R: Reader + BufRead + Seek
    {
        let dblksize: usize = 1 << (sb.sb_blocklog + sb.sb_dirblklog);
        let mut buf = vec![0; dblksize];

        buf_reader
            .seek(SeekFrom::Start(sb.fsb_to_offset(fsblock)))
            .unwrap();
        buf_reader.read_exact(&mut buf).unwrap();
        Ok(buf)
    }
}

impl Dir3 for Dir2Node {
    fn get_addresses<'a, R>(&'a self, buf_reader: &'a RefCell<&'a mut R>, hash: XfsDahash)
        -> Box<dyn Iterator<Item=XfsDir2Dataptr> + 'a>
            where R: Reader + BufRead + Seek + 'a
    {
        NodeLikeDir::get_addresses(self, buf_reader, hash)
    }

    fn read_dblock<'a, R>(&'a self, mut buf_reader: R, sb: &Sb, dblock: XfsDablk)
        -> Result<Box<dyn Deref<Target=[u8]> + 'a>, i32>
        where R: Reader + BufRead + Seek
    {
        let mut cache_guard = self.blocks.borrow_mut();
        let entry = cache_guard.entry(dblock);
        if matches!(entry, Entry::Vacant(_)) {
            let fsblock = self.map_dblock(buf_reader.by_ref(), dblock)?;
            let buf = self.read_fsblock(buf_reader.by_ref(), sb, fsblock)?;
            entry.or_insert(buf);
        }
        // Annoyingly, there's no function to downgrade a RefMut into a Ref.
        drop(cache_guard);
        let cache_guard = self.blocks.borrow();
        Ok(Box::new(Ref::map(cache_guard, |v| &v[&dblock][..])))
    }
}

impl NodeLikeDir for Dir2Node {
    fn map_dblock<R: Reader + BufRead + Seek>(
        &self,
        _buf_reader: &mut R,
        dblock: XfsDablk,
    ) -> Result<XfsFsblock, i32> {
        self.bmx.map_dblock(dblock).ok_or(libc::ENOENT)
    }

}
