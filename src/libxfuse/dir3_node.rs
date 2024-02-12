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
use std::cell::RefCell;
use std::convert::TryInto;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, Seek, SeekFrom};

use super::bmbt_rec::BmbtRec;
use super::da_btree::{hashname, XfsDa3Intnode};
use super::definitions::*;
use super::dinode::Dinode;
use super::dir3::{Dir2DataEntry, Dir2DataUnused, Dir2LeafNDisk, Dir3, Dir3DataHdr};
use super::sb::Sb;
use super::utils::{decode, decode_from, get_file_type, FileKind};

use fuser::{FileAttr, FileType};
use libc::{c_int, EIO, ENOENT};

#[derive(Debug)]
pub struct Dir2Node {
    pub bmx: Vec<BmbtRec>,
    pub block_size: u32,
    /// An cache of the last extent and its starting block number read by lookup
    /// or readdir.
    block_cache: RefCell<Option<(u64, Vec<u8>)>>
}

impl Dir2Node {
    pub fn from(bmx: Vec<BmbtRec>, block_size: u32) -> Dir2Node {
        Dir2Node {
            bmx,
            block_size,
            block_cache: RefCell::new(None)
        }
    }

    pub fn map_dblock(&self, dblock: XfsFileoff) -> Option<&BmbtRec> {
        let mut res: Option<&BmbtRec> = None;
        for record in self.bmx.iter().rev() {
            if dblock >= record.br_startoff {
                res = Some(record);
                break;
            }
        }

        if let Some(res_some) = res {
            if dblock >= res_some.br_startoff + res_some.br_blockcount {
                res = None
            }
        }

        res
    }

    pub fn map_dblock_number(&self, dblock: XfsFileoff) -> XfsFsblock {
        for record in self.bmx.iter().rev() {
            if dblock >= record.br_startoff {
                return record.br_startblock + (dblock - record.br_startoff);
            }
        }

        panic!("Couldn't find the directory block");
    }
}

impl<R: bincode::de::read::Reader + BufRead + Seek> Dir3<R> for Dir2Node {
    fn lookup(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        name: &OsStr,
    ) -> Result<(FileAttr, u64), c_int> {
        let dblock = super_block.get_dir3_leaf_offset();
        let hash = hashname(name);

        let blk = {
            // NB: reading and decoding the XfsDa3Intnode could be cached in
            // Self.  but it won't be worthwhile unless we implement
            // ReaddirPlus.
            let bmbt_rec = if let Some(bmbt_rec_some) = self.map_dblock(dblock) {
                buf_reader
                    .seek(SeekFrom::Start(
                        (bmbt_rec_some.br_startblock) * (self.block_size as u64),
                    ))
                    .unwrap();
                bmbt_rec_some
            } else {
                return Err(ENOENT)
            };

            let extent_size = (bmbt_rec.br_blockcount) as usize * self.block_size as usize;
            let mut raw = vec![0u8; extent_size];
            buf_reader
                .seek(SeekFrom::Start(bmbt_rec.br_startblock * self.block_size as u64))
                .unwrap();
            buf_reader.read_exact(&mut raw).unwrap();

            let (node, _) = decode::<XfsDa3Intnode>(&raw[..]).map_err(|_| EIO)?;
            assert_eq!(node.hdr.info.magic, XFS_DA3_NODE_MAGIC);
            node.lookup(buf_reader.by_ref(), super_block, hash, |block, _| {
                self.map_dblock_number(block.into())
            })
        }?;

        let leaf_offset = blk * u64::from(super_block.sb_blocksize);
        buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();
        let leaf: Dir2LeafNDisk = decode_from(buf_reader.by_ref()).unwrap();
        leaf.sanity(super_block);

        let address = leaf.get_address(hash)? * 8;
        let idx = (address / super_block.sb_blocksize) as u64;
        let address = (address % super_block.sb_blocksize) as usize;

        let bmbt_rec = self.map_dblock(idx).ok_or(EIO)?;
        let blk = bmbt_rec.br_startblock + (idx - bmbt_rec.br_startoff);
        buf_reader
            .seek(SeekFrom::Start(blk * u64::from(super_block.sb_blocksize)))
            .unwrap();
        let mut leafraw = vec![0u8; bmbt_rec.br_blockcount as usize * self.block_size as usize];
        buf_reader.read_exact(&mut leafraw).unwrap();

        let entry: Dir2DataEntry = decode(&leafraw[address..]).unwrap().0;

        let dinode = Dinode::from(buf_reader.by_ref(), super_block, entry.inumber);

        let attr = dinode.di_core.stat(entry.inumber)?;

        Ok((attr, dinode.di_core.di_gen.into()))
    }

    fn next(
        &self,
        buf_reader: &mut R,
        sb: &Sb,
        // logical byte offset within the directory
        offset: i64,
    ) -> Result<(XfsIno, i64, FileType, OsString), c_int>
    {
        let mut offset: u64 = offset.try_into().unwrap();
        let block_size = self.block_size as u64;
        let mut next = offset == 0;

        while let Some(bmbt_rec) = self.map_dblock(offset / block_size) {
            // Byte offset within this extent
            let mut ex_offset = (offset - bmbt_rec.br_startoff * block_size) as usize;
            let extent_size = (bmbt_rec.br_blockcount) as usize * block_size as usize;

            let mut cache_guard = self.block_cache.borrow_mut();
            if cache_guard.is_none() || cache_guard.as_ref().unwrap().0 != bmbt_rec.br_startblock
            {
                let mut raw = vec![0u8; extent_size];
                buf_reader
                    .seek(SeekFrom::Start(bmbt_rec.br_startblock * block_size))
                    .unwrap();
                buf_reader.read_exact(&mut raw).unwrap();
                *cache_guard = Some((bmbt_rec.br_startblock, raw));
            }
            let raw = &cache_guard.as_ref().unwrap().1;

            while ex_offset < extent_size
            {
                // Byte offset within this directory block
                let dir_block_offset = offset % ((1 << sb.sb_dirblklog) * block_size);
                // Offset of this directory block within its extent
                let dboffset = offset - dir_block_offset;

                // If this is the start of the directory block, skip it.
                if dir_block_offset == 0 {
                    ex_offset += Dir3DataHdr::SIZE as usize;
                    offset += Dir3DataHdr::SIZE;
                }

                // Skip the next directory entry
                let freetag: u16 = decode(&raw[ex_offset..]).unwrap().0;
                if freetag == 0xffff {
                    let (_, l) = decode::<Dir2DataUnused>(&raw[ex_offset..])
                        .unwrap();
                    ex_offset += l;
                    offset += l as u64;
                } else if !next {
                    let length = Dir2DataEntry::get_length(&raw[ex_offset..]);
                    ex_offset += length as usize;
                    offset += length as u64;
                    next = true;
                } else {
                    let entry: Dir2DataEntry = decode(&raw[ex_offset..]).unwrap().0;
                    let kind = get_file_type(FileKind::Type(entry.ftype))?;
                    let name = entry.name;
                    let entry_offset = dboffset + entry.tag as u64;
                    return Ok((entry.inumber, entry_offset as i64, kind, name));
                }
            }
        }

        Err(ENOENT)
    }
}
