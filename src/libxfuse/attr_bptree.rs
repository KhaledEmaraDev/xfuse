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
        if self.total_size != -1 {
            return self.total_size.try_into().unwrap();
        } else {
            let mut total_size: u32 = 0;

            let blk = self.btree.map_block(buf_reader.by_ref(), &super_block, 0);
            buf_reader
                .seek(SeekFrom::Start(blk * u64::from(super_block.sb_blocksize)))
                .unwrap();

            let node = XfsDa3Intnode::from(buf_reader.by_ref());

            let blk = node.first_block(buf_reader.by_ref(), &super_block, |block, reader| {
                self.btree
                    .map_block(reader.by_ref(), &super_block, block.into())
            });
            let leaf_offset = blk * u64::from(super_block.sb_blocksize);

            buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();

            let mut node = AttrLeafblock::from(buf_reader.by_ref());
            total_size += node.get_total_size(buf_reader.by_ref(), leaf_offset);

            while node.hdr.info.forw != 0 {
                node = AttrLeafblock::from(buf_reader.by_ref());
                total_size += node.get_total_size(buf_reader.by_ref(), leaf_offset);
            }

            self.total_size = i64::from(total_size);
            self.total_size.try_into().unwrap()
        }
    }

    fn get_size(&self, buf_reader: &mut R, super_block: &Sb, name: &str) -> u32 {
        let hash = hashname(name);

        let blk = self.btree.map_block(buf_reader.by_ref(), &super_block, 0);
        buf_reader
            .seek(SeekFrom::Start(blk * u64::from(super_block.sb_blocksize)))
            .unwrap();

        let node = XfsDa3Intnode::from(buf_reader.by_ref());

        let blk = node.lookup(buf_reader.by_ref(), &super_block, hash, |block, reader| {
            self.btree
                .map_block(reader.by_ref(), &super_block, block.into())
        });
        let leaf_offset = blk * u64::from(super_block.sb_blocksize);

        buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();

        let leaf = AttrLeafblock::from(buf_reader.by_ref());

        leaf.get_size(buf_reader.by_ref(), hash, leaf_offset)
    }

    fn list(&mut self, buf_reader: &mut R, super_block: &Sb) -> Vec<u8> {
        let mut list: Vec<u8> =
            Vec::with_capacity(self.get_total_size(buf_reader.by_ref(), &super_block) as usize);

        let blk = self.btree.map_block(buf_reader.by_ref(), &super_block, 0);
        buf_reader
            .seek(SeekFrom::Start(blk * u64::from(super_block.sb_blocksize)))
            .unwrap();

        let node = XfsDa3Intnode::from(buf_reader.by_ref());

        let blk = node.first_block(buf_reader.by_ref(), &super_block, |block, reader| {
            self.btree
                .map_block(reader.by_ref(), &super_block, block.into())
        });
        let leaf_offset = blk * u64::from(super_block.sb_blocksize);

        buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();

        let mut leaf = AttrLeafblock::from(buf_reader.by_ref());
        leaf.list(buf_reader.by_ref(), &mut list, leaf_offset);

        while leaf.hdr.info.forw != 0 {
            leaf = AttrLeafblock::from(buf_reader.by_ref());
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

        let node = XfsDa3Intnode::from(buf_reader.by_ref());

        let blk = node.lookup(buf_reader.by_ref(), &super_block, hash, |block, reader| {
            self.btree
                .map_block(reader.by_ref(), &super_block, block.into())
        });
        let leaf_offset = blk * u64::from(super_block.sb_blocksize);

        buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();

        let leaf = AttrLeafblock::from(buf_reader.by_ref());

        leaf.get(
            buf_reader.by_ref(),
            super_block,
            hash,
            leaf_offset,
            |block, reader| {
                self.btree
                    .map_block(reader.by_ref(), &super_block, block.into())
            },
        )
    }
}
