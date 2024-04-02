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
    btree::{Btree, BtreeRoot},
    definitions::{XfsFileoff, XfsFsize, XfsFsblock},
    file::File,
    volume::SUPERBLOCK
};

#[derive(Debug)]
pub struct FileBtree {
    pub btree: BtreeRoot,
    pub size: XfsFsize,
}

impl<R: Reader + BufRead + Seek> File<R> for FileBtree {
    fn lseek(&mut self, buf_reader: &mut R, offset: u64, whence: i32) -> Result<u64, i32> {
        let sb = SUPERBLOCK.get().unwrap();

        let mut dblock = offset >> sb.sb_blocklog;
        match self.btree.map_block(buf_reader.by_ref(), dblock)? {
            (Some(_), None) => {
                unreachable!("Btree::map_block should never return None for the length of a data region");
            },
            (Some(_), Some(len)) => {
                // In a data region
                if whence == libc::SEEK_DATA {
                    Ok(offset)
                } else {
                    // Scan for the next hole
                    dblock += len;
                    loop {
                         match self.btree.map_block(buf_reader.by_ref(), dblock)? {
                             (Some(_fsblock), Some(len)) => {
                                 dblock += len;
                             },
                             (Some(_fsblock), None) => {
                                 unreachable!("Btree::map_block should never return \
                                        None for the length of a data region");
                             },
                             (None, _) => {
                                 return Ok(dblock << sb.sb_blocklog);
                             },
                         }
                    }
                }
            },
            (None, None) => {
                // A hole that extends to EOF
                if whence == libc::SEEK_DATA {
                    Err(libc::ENXIO)
                } else {
                    Ok(offset)
                }
            },
            (None, Some(len)) => {
                // A hole, followed by data
                if whence == libc::SEEK_DATA {
                    // It should be impossible to have two hole extents in a row.  But
                    // double-check.
                    debug_assert!(
                        self.btree.map_block(buf_reader.by_ref(), dblock + len).unwrap().0.is_some()
                    );
                    Ok(offset + (len << sb.sb_blocklog))
                } else {
                    Ok(offset)
                }
            },
         }
    }

    fn get_extent(&self, buf_reader: &mut R, block: XfsFileoff) -> (Option<XfsFsblock>, u64) {
        let sb = SUPERBLOCK.get().unwrap();
        let (blk, len) = self.btree.map_block(buf_reader.by_ref(), block).unwrap();
        let len = len.unwrap_or((self.size as u64).div_ceil(sb.sb_blocksize.into()));
        (blk, len)
    }

    fn size(&self) -> XfsFsize {
        self.size
    }
}
