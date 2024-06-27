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
    ffi::CString,
    io::{BufRead, Seek, SeekFrom},
};

use bincode::{de::read::Reader, Decode};

use super::{
    bmbt_rec::Bmx,
    definitions::XFS_SYMLINK_MAGIC,
    sb::Sb,
    utils::{decode_from, Uuid},
};

#[derive(Clone, Copy, Debug, Decode)]
pub struct DsymlinkHdr {
    sl_magic:  u32,
    sl_offset: u32,
    sl_bytes:  u32,
    _sl_crc:   u32,
    _sl_uuid:  Uuid,
    _sl_owner: u64,
    _sl_blkno: u64,
    _sl_lsn:   u64,
}

#[derive(Debug)]
pub struct SymlinkExtents;

impl SymlinkExtents {
    pub fn get_target<T: BufRead + Reader + Seek>(
        buf_reader: &mut T,
        bmx: &Bmx,
        superblock: &Sb,
    ) -> CString {
        let mut data = Vec::<u8>::with_capacity(1024);

        let mut dblock = 0;
        loop {
            let (ofsb, oblocks) = bmx.get_extent(dblock);
            if ofsb.is_none() {
                break;
            }
            let fsb = ofsb.unwrap();
            let blocks = oblocks.unwrap();
            buf_reader
                .seek(SeekFrom::Start(superblock.fsb_to_offset(fsb)))
                .unwrap();

            let bytes = match superblock.version() {
                5 => {
                    let hdr: DsymlinkHdr = decode_from(buf_reader.by_ref()).unwrap();
                    assert_eq!(XFS_SYMLINK_MAGIC, hdr.sl_magic);

                    buf_reader
                        .seek(SeekFrom::Current(hdr.sl_offset as i64))
                        .unwrap();
                    hdr.sl_bytes as usize
                }
                4 => {
                    // Version 4 file systems do not have the DsymlinkHdr
                    (blocks as usize) << superblock.sb_blocklog
                }
                _ => unimplemented!(),
            };

            let oldlen = data.len();
            data.resize(oldlen + bytes, 0);
            buf_reader.read_exact(&mut data[oldlen..]).unwrap();
            dblock += blocks;
        }

        match CString::new(data) {
            Ok(s) => s,
            Err(ne) => {
                // A V4 file system does not store the length of the symlink target, so we must
                // infer it by the presence of a NUL byte.
                debug_assert_eq!(superblock.version(), 4);
                let p = ne.nul_position();
                let mut v = ne.into_vec();
                v.truncate(p);
                CString::new(v).unwrap()
            }
        }
    }
}
