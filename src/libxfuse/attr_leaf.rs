use std::{
    convert::TryInto,
    io::{BufRead, Seek, SeekFrom},
};

use super::{
    attr::{Attr, AttrLeafblock},
    bmbt_rec::BmbtRec,
    da_btree::hashname,
    definitions::{XfsFileoff, XfsFsblock},
    sb::Sb,
};

#[derive(Debug)]
pub struct AttrLeaf {
    pub bmx: Vec<BmbtRec>,
    pub leaf: AttrLeafblock,

    pub leaf_offset: u64,
    pub total_size: i64,
}

impl AttrLeaf {
    pub fn from<R: BufRead + Seek>(
        buf_reader: &mut R,
        superblock: &Sb,
        bmx: Vec<BmbtRec>,
    ) -> AttrLeaf {
        if let Some(rec) = bmx.first() {
            let leaf_offset = rec.br_startblock * u64::from(superblock.sb_blocksize);
            buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();

            let leaf = AttrLeafblock::from(buf_reader.by_ref());

            AttrLeaf {
                bmx,
                leaf,
                leaf_offset,
                total_size: -1,
            }
        } else {
            panic!("Extent records missing!");
        }
    }

    pub fn map_logical_block_to_actual_block(&self, block: XfsFileoff) -> XfsFsblock {
        for entry in self.bmx.iter().rev() {
            if block >= entry.br_startoff {
                return entry.br_startblock + (block - entry.br_startoff);
            }
        }

        panic!("Couldn't find logical block!");
    }
}

impl<R: BufRead + Seek> Attr<R> for AttrLeaf {
    fn get_total_size(&mut self, buf_reader: &mut R, _super_block: &Sb) -> u32 {
        if self.total_size != -1 {
            return self.total_size.try_into().unwrap();
        } else {
            self.total_size = i64::from(
                self.leaf
                    .get_total_size(buf_reader.by_ref(), self.leaf_offset),
            );
            self.total_size as u32
        }
    }

    fn get_size(&self, buf_reader: &mut R, _super_block: &Sb, name: &str) -> u32 {
        let hash = hashname(name);

        self.leaf
            .get_size(buf_reader.by_ref(), hash, self.leaf_offset)
    }

    fn list(&mut self, buf_reader: &mut R, super_block: &Sb) -> Vec<u8> {
        let mut list: Vec<u8> =
            Vec::with_capacity(self.get_total_size(buf_reader.by_ref(), &super_block) as usize);

        self.leaf
            .list(buf_reader.by_ref(), &mut list, self.leaf_offset);

        list
    }

    fn get(&self, buf_reader: &mut R, super_block: &Sb, name: &str) -> Vec<u8> {
        let hash = hashname(name);

        self.leaf.get(
            buf_reader.by_ref(),
            super_block,
            hash,
            self.leaf_offset,
            |block, _| self.map_logical_block_to_actual_block(block),
        )
    }
}
