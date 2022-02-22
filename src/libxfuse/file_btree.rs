/**
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
                .map_block(buf_reader.by_ref(), super_block, logical_block as u64);
            buf_reader
                .seek(SeekFrom::Start(blk * u64::from(self.block_size)))
                .unwrap();
            buf_reader.seek(SeekFrom::Current(block_offset)).unwrap();

            let size_to_read = min(remaining_size, (self.block_size as i64) - block_offset);

            let mut buf = vec![0u8; size_to_read as usize];
            buf_reader.read_exact(&mut buf).unwrap();
            data.extend_from_slice(&buf);

            remaining_size -= size_to_read;
            logical_block += 1;
            block_offset = 0;
        }

        data
    }
}
