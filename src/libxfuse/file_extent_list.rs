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
use std::io::{BufRead, Seek};

use bincode::de::read::Reader;

use super::{
    bmbt_rec::BmbtRec,
    definitions::{XfsFileoff, XfsFsblock, XfsFsize},
    file::File,
    volume::SUPERBLOCK
};

#[derive(Debug)]
pub struct FileExtentList {
    pub bmx: Vec<BmbtRec>,
    pub size: XfsFsize,
}

impl<R: BufRead + Reader + Seek> File<R> for FileExtentList {
    /// Return the extent, if any, that contains the given data block within the file.
    /// Return its starting position as an FSblock, and its length in file system block units
    fn get_extent(&self, _buf_reader: &mut R, block: XfsFileoff) -> (Option<XfsFsblock>, u64) {
        let sb = SUPERBLOCK.get().unwrap();
        match self.bmx.partition_point(|entry| entry.br_startoff <= block) {
            0 => {
                // A hole at the beginning of the file
                let hole_len = self.bmx.first()
                    .map(|b| b.br_startoff)
                    .unwrap_or((self.size as u64).div_ceil(sb.sb_blocksize.into()));
                (None, hole_len - block)
            },
            i => {
                let entry = &self.bmx[i - 1];
                let skip = block - entry.br_startoff;
                if entry.br_startoff + entry.br_blockcount > block {
                    (Some(entry.br_startblock + skip), entry.br_blockcount - skip)
                } else {
                    // It's a hole
                    let next_ex_start = self.bmx.get(i)
                        .map(|e| e.br_startoff)
                        .unwrap_or((self.size as u64).div_ceil(sb.sb_blocksize.into()));
                    let hole_len = next_ex_start - entry.br_startoff;
                    (None, hole_len - skip)
                }
            }
        }
    }

    fn lseek(&mut self, _buf_reader: &mut R, offset: u64, whence: i32) -> Result<u64, i32> {
        let sb = SUPERBLOCK.get().unwrap();

        let dblock = offset >> sb.sb_blocklog;
        match self.bmx.partition_point(|entry| entry.br_startoff <= dblock) {
            0 => {
                // A hole at the beginning of the file
                if whence == libc::SEEK_HOLE {
                    Ok(offset)
                } else {
                    self.bmx.first()
                        .map(|b| b.br_startoff << sb.sb_blocklog)
                        .ok_or(libc::ENXIO)
                }
            },
            i => {
                let cur_entry = &self.bmx[i - 1];
                let br_end = cur_entry.br_startoff + cur_entry.br_blockcount;
                if dblock < br_end {
                    // In a data region
                    if whence == libc::SEEK_HOLE {
                        // Scan for the next hole
                        for j in (i - 1)..self.bmx.len() - 1 {
                            let before = &self.bmx[j];
                            let after = &self.bmx[j + 1];
                            let br_end = before.br_startoff + before.br_blockcount;
                            if after.br_startoff > br_end {
                                return Ok(br_end << sb.sb_blocklog);
                            }
                        }
                        // Reached EOF without finding another hole.  Return the virtual hole at
                        // EOF
                        let entry = self.bmx.last().unwrap();
                        let br_end = entry.br_startoff + entry.br_blockcount;
                        Ok(br_end << sb.sb_blocklog)
                    } else {
                        Ok(offset)
                    }
                } else {
                    // In a hole
                    if whence == libc::SEEK_HOLE {
                        Ok(offset)
                    } else {
                        match self.bmx.get(i) {
                            Some(next_entry) => {
                                Ok(next_entry.br_startoff << sb.sb_blocklog)
                            },
                            None => Err(libc::ENXIO)
                        }
                    }
                }
            }
        }
    }

    fn size(&self) -> XfsFsize {
        self.size
    }
}
