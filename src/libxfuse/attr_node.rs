use std::{
    convert::TryInto,
    io::{BufRead, Seek, SeekFrom},
};

use super::{
    attr::{Attr, AttrLeafblock},
    bmbt_rec::BmbtRec,
    da_btree::{hashname, XfsDa3Intnode},
    definitions::{XfsFileoff, XfsFsblock},
    sb::Sb,
};

#[derive(Debug)]
pub struct AttrNode {
    pub bmx: Vec<BmbtRec>,
    pub node: XfsDa3Intnode,

    pub total_size: i64,
}

impl AttrNode {
    pub fn from<R: BufRead + Seek>(
        buf_reader: &mut R,
        superblock: &Sb,
        bmx: Vec<BmbtRec>,
    ) -> AttrNode {
        if let Some(rec) = bmx.first() {
            buf_reader
                .seek(SeekFrom::Start(
                    rec.br_startblock * u64::from(superblock.sb_blocksize),
                ))
                .unwrap();

            let node = XfsDa3Intnode::from(buf_reader.by_ref(), superblock);

            AttrNode {
                bmx,
                node,
                total_size: -1,
            }
        } else {
            panic!("Extent records missing!");
        }
    }

    pub fn map_logical_block_to_fs_block(&self, block: XfsFileoff) -> XfsFsblock {
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

impl<R: BufRead + Seek> Attr<R> for AttrNode {
    fn get_total_size(&mut self, buf_reader: &mut R, super_block: &Sb) -> u32 {
        if self.total_size != -1 {
            return self.total_size.try_into().unwrap();
        } else {
            let mut total_size: u32 = 0;

            let blk = self
                .node
                .first_block(buf_reader.by_ref(), &super_block, |block, _| {
                    self.map_logical_block_to_fs_block(block.into())
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
            self.total_size.try_into().unwrap()
        }
    }

    fn get_size(&self, buf_reader: &mut R, super_block: &Sb, name: &str) -> u32 {
        let hash = hashname(name);

        let node = XfsDa3Intnode::from(buf_reader.by_ref(), super_block);
        let blk = node.lookup(buf_reader.by_ref(), &super_block, hash, |block, _| {
            self.map_logical_block_to_fs_block(block.into())
        });
        let leaf_offset = blk * u64::from(super_block.sb_blocksize);

        buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();
        let leaf = AttrLeafblock::from(buf_reader.by_ref(), super_block);

        leaf.get_size(buf_reader.by_ref(), hash, leaf_offset)
    }

    fn list(&mut self, buf_reader: &mut R, super_block: &Sb) -> Vec<u8> {
        let mut list: Vec<u8> =
            Vec::with_capacity(self.get_total_size(buf_reader.by_ref(), &super_block) as usize);

        let blk = self
            .node
            .first_block(buf_reader.by_ref(), &super_block, |block, _| {
                self.map_logical_block_to_fs_block(block.into())
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

        let node = XfsDa3Intnode::from(buf_reader.by_ref(), super_block);
        let blk = node.lookup(buf_reader.by_ref(), &super_block, hash, |block, _| {
            self.map_logical_block_to_fs_block(block.into())
        });
        let leaf_offset = blk * u64::from(super_block.sb_blocksize);

        buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();
        let leaf = AttrLeafblock::from(buf_reader.by_ref(), super_block);

        leaf.get(
            buf_reader.by_ref(),
            super_block,
            hash,
            leaf_offset,
            |block, _| self.map_logical_block_to_fs_block(block),
        )
    }
}
