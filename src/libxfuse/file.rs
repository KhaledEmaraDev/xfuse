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

use bincode_next::de::read::Reader;

use super::{
    definitions::{XfsFileoff, XfsFsblock, XfsFsize},
    file_btree::FileBtree,
    file_extent_list::FileExtentList,
};

#[enum_dispatch::enum_dispatch]
pub trait File {
    /// Return the extent, if any, that contains the given data block within the file.
    /// Return its starting position as an FSblock, and its length in file system block units
    fn get_extent<R>(&self, buf_reader: &mut R, block: XfsFileoff) -> (Option<XfsFsblock>, u64)
    where
        R: BufRead + Reader + Seek;

    /// Like lseek(2), but only works for SEEK_HOLE and SEEK_DATA
    fn lseek<R>(&self, buf_reader: &mut R, offset: u64, whence: i32) -> Result<u64, i32>
    where
        R: BufRead + Reader + Seek;

    fn size(&self) -> XfsFsize;
}

#[derive(Debug)]
#[enum_dispatch::enum_dispatch(File)]
pub enum FileMetadata {
    Bmx(FileExtentList),
    Btree(FileBtree),
}
