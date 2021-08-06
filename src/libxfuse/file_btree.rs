use std::{
    cmp::min,
    io::{BufRead, Seek, SeekFrom},
};

use super::{btree::Btree, definitions::XfsFsize, file::File, sb::Sb};

#[derive(Debug)]
pub struct FileBtree {
    pub btree: Btree,
    pub size: XfsFsize,
    pub block_size: u32,
}

impl<R: BufRead + Seek> File<R> for FileBtree {
    fn read(&mut self, buf_reader: &mut R, super_block: &Sb, offset: i64, size: u32) -> Vec<u8> {
        let mut data = Vec::<u8>::with_capacity(size as usize);

        let mut remaining_size = min(size as i64, self.size - offset);

        if remaining_size < 0 {
            panic!("Offset is too large!");
        }

        let mut logical_block = offset / i64::from(self.block_size);
        let mut block_offset = offset % i64::from(self.block_size);

        while remaining_size > 0 {
            let blk = self
                .btree
                .map_block(buf_reader.by_ref(), &super_block, logical_block as u64);
            buf_reader
                .seek(SeekFrom::Start(blk * u64::from(self.block_size)))
                .unwrap();
            buf_reader.seek(SeekFrom::Current(block_offset)).unwrap();

            let size_to_read = min(remaining_size, (self.block_size as i64) - block_offset);

            let mut buf = vec![0u8; size_to_read as usize];
            buf_reader.read_exact(&mut buf).unwrap();
            data.extend_from_slice(&mut buf);

            remaining_size -= size_to_read;
            logical_block += 1;
            block_offset = 0;
        }

        data
    }
}
