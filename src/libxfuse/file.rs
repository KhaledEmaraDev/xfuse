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
    cmp::min,
    io::{BufRead, Seek, SeekFrom}
};

use bincode::de::read::Reader;

use super::{
    definitions::{XfsFileoff, XfsFsblock, XfsFsize},
    volume::SUPERBLOCK,
};

pub trait File<R: BufRead + Reader + Seek> {
    /// Return the extent, if any, that contains the given data block within the file.
    /// Return its starting position as an FSblock, and its length in file system block units
    fn get_extent(&self, buf_reader: &mut R, block: XfsFileoff) -> (Option<XfsFsblock>, u64);

    fn read(&mut self, buf_reader: &mut R, offset: i64, size: u32) -> Vec<u8> {
        let sb = SUPERBLOCK.get().unwrap();
        let mut data = Vec::<u8>::with_capacity(size as usize);

        let mut remaining_size = min(size as i64, self.size() - offset);

        assert!(remaining_size >= 0, "Offset is too large!");

        let mut logical_block = offset / i64::from(sb.sb_blocksize);
        let mut block_offset = offset % i64::from(sb.sb_blocksize);

        while remaining_size > 0 {
            let (blk, blocks) = self.get_extent(buf_reader.by_ref(), logical_block as u64);
            let z = min(remaining_size, (blocks as i64 * sb.sb_blocksize as i64) - block_offset);
            let oldlen = data.len();
            data.resize(oldlen + z as usize, 0u8);
            if let Some(blk) = blk {
                buf_reader
                    .seek(SeekFrom::Start(sb.fsb_to_offset(blk) + block_offset as u64))
                    .unwrap();

                buf_reader.read_exact(&mut data[oldlen..]).unwrap();
            } else {
                // A hole
            }
            logical_block += blocks as i64;
            remaining_size -= z;
            block_offset = 0;
        }

        data
    }

    fn size(&self) -> XfsFsize;
}
