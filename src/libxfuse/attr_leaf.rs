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
    io::{BufRead, Seek},
};

use bincode::de::read::Reader;

use super::{
    attr::{Attr, AttrLeafblock},
    bmbt_rec::BmbtRec,
    da_btree::hashname,
    definitions::{XfsDablk, XfsFileoff, XfsFsblock},
    sb::Sb,
};


#[derive(Debug)]
pub struct AttrLeaf {
    pub bmx: Vec<BmbtRec>,
    pub leaf: AttrLeafblock,
    pub total_size: i64,
}

impl AttrLeaf {
    fn map_logical_block_to_actual_block(&self, block: XfsDablk) -> XfsFsblock {
        for entry in self.bmx.iter().rev() {
            if XfsFileoff::from(block) >= entry.br_startoff {
                return entry.br_startblock + (XfsFileoff::from(block) - entry.br_startoff);
            }
        }

        panic!("Couldn't find logical block!");
    }
}

impl<R: BufRead + Reader + Seek> Attr<R> for AttrLeaf {
    fn get_total_size(&mut self, _buf_reader: &mut R, _super_block: &Sb) -> u32 {
        if self.total_size != -1 {
            self.total_size.try_into().unwrap()
        } else {
            self.total_size = i64::from(self.leaf.get_total_size());
            self.total_size as u32
        }
    }

    fn list(&mut self, buf_reader: &mut R, super_block: &Sb) -> Vec<u8> {
        let mut list: Vec<u8> =
            Vec::with_capacity(self.get_total_size(buf_reader.by_ref(), super_block) as usize);

        self.leaf.list(&mut list);

        list
    }

    fn get(&self, buf_reader: &mut R, _super_block: &Sb, name: &OsStr) -> Result<Vec<u8>, i32> {
        let hash = hashname(name);

        self.leaf.get(
            buf_reader.by_ref(),
            hash,
            |block, _| self.map_logical_block_to_actual_block(block),
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::libxfuse::{
        attr::{AttrLeafHdr, AttrLeafblock},
        attr_leaf::AttrLeaf,
        bmbt_rec::BmbtRec,
        da_btree::XfsDa3Blkinfo,
    };

    #[test]
    fn attr_leaf_block_mapping() {
        let bmx: Vec<BmbtRec> = vec![
            BmbtRec {
                br_startoff: 0,
                br_startblock: 20,
                br_blockcount: 2,
            },
            BmbtRec {
                br_startoff: 2,
                br_startblock: 30,
                br_blockcount: 3,
            },
            BmbtRec {
                br_startoff: 5,
                br_startblock: 40,
                br_blockcount: 2,
            },
        ];

        let leaf = AttrLeafblock {
            hdr: AttrLeafHdr {
                info: XfsDa3Blkinfo {
                    forw: 0,
                    magic: 0,
                },
                count: 0,
            },
            entries: vec![],
            names: vec![],
        };

        let attr = AttrLeaf {
            bmx,
            leaf,
            total_size: 0,
        };

        assert_eq!(attr.map_logical_block_to_actual_block(6), 41);
    }
}
