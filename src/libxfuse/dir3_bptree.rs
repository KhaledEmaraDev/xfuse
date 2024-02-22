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
    cell::RefCell,
    convert::TryInto,
    ffi::{OsStr, OsString},
    io::{BufRead, Seek, SeekFrom},
};

use super::btree::{BmbtKey, BmdrBlock, Btree, BtreeRoot, XfsBmbtPtr};
use super::da_btree::{hashname, XfsDa3Intnode};
use super::definitions::*;
use super::dinode::Dinode;
use super::dir3::{Dir2DataEntry, Dir2DataUnused, Dir3, Dir3DataHdr, Dir2LeafNDisk};
use super::sb::Sb;
use super::utils::{decode, decode_from, get_file_type, FileKind};
use super::volume::SUPERBLOCK;

use fuser::{FileAttr, FileType};
use libc::c_int;

#[derive(Debug)]
pub struct Dir2Btree{
    root: BtreeRoot,
    /// A cache of the last extent and its starting block number read by lookup
    /// or readdir.
    block_cache: RefCell<Option<(u64, Vec<u8>)>>
}

impl Dir2Btree {
    pub fn from(
        bmbt: BmdrBlock,
        keys: Vec<BmbtKey>,
        pointers: Vec<XfsBmbtPtr>,
    ) -> Self {
        let block_cache = RefCell::new(None);
        let root = BtreeRoot::new(bmbt, keys, pointers);
        Self{root, block_cache}
    }
}

impl<R: bincode::de::read::Reader + BufRead + Seek> Dir3<R> for Dir2Btree {
    fn lookup(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        name: &OsStr,
    ) -> Result<(FileAttr, u64), c_int> {
        let blocksize: u64 = SUPERBLOCK.get().unwrap().sb_blocksize.into();
        let dblksize: u64 = blocksize * (1 << super_block.sb_dirblklog);
        let idx = super_block.get_dir3_leaf_offset();
        let hash = hashname(name);

        let fsblock = self.root.map_block(buf_reader, super_block, idx)?;

        let mut raw = vec![0u8; dblksize as usize];
        buf_reader.seek(SeekFrom::Start(fsblock * blocksize)).unwrap();
        buf_reader.read_exact(&mut raw).unwrap();
        let (node, _) = decode::<XfsDa3Intnode>(&raw[..]).map_err(|_| libc::EIO)?;
        assert_eq!(node.hdr.info.magic, XFS_DA3_NODE_MAGIC);
        let blk: XfsFsblock = node.lookup(buf_reader.by_ref(), super_block, hash, |block, br| {
            self.root.map_block(br, super_block, block.into()).unwrap()
        })?;

        buf_reader.seek(SeekFrom::Start(blk * blocksize)).unwrap();
        let leaf: Dir2LeafNDisk = decode_from(buf_reader.by_ref()).unwrap();
        leaf.sanity(super_block);

        for collision_resolver in 0.. {
            let address = leaf.get_address(hash, collision_resolver)? * 8;
            let leaf_dblock = u64::from(address) / blocksize;
            let address = (u64::from(address) % blocksize) as usize;

            let leaf_fs_block = self.root.map_block(buf_reader, super_block, leaf_dblock)?;
            buf_reader
                .seek(SeekFrom::Start(leaf_fs_block * blocksize))
                .unwrap();
            let mut leafraw = vec![0u8; dblksize as usize];
            buf_reader.read_exact(&mut leafraw).unwrap();

            let entry: Dir2DataEntry = decode(&leafraw[address..]).unwrap().0;
            if entry.name != name {
                // There was a probably hash collision in the directory.  This happens frequently,
                // since the hash is only 32 bits.
                continue;
            }

            let dinode = Dinode::from(buf_reader.by_ref(), super_block, entry.inumber);

            let attr = dinode.di_core.stat(entry.inumber)?;

            return Ok((attr, dinode.di_core.di_gen.into()));
        }
        Err(libc::ENOENT)
    }

    fn next(
        &self,
        buf_reader: &mut R,
        sb: &Sb,
        offset: i64,
    ) -> Result<(XfsIno, i64, FileType, OsString), c_int> {
        let blocksize: u64 = sb.sb_blocksize.into();
        let dblksize: u64 = blocksize * (1 << sb.sb_dirblklog);
        let mut offset: u64 = offset.try_into().unwrap();
        let mut next = offset == 0;

        loop {
            // Byte offset within this directory block
            let dir_block_offset = offset % ((1 << sb.sb_dirblklog) * blocksize);
            // Offset of this directory block within the directory
            let doffset = offset - dir_block_offset;

            let lblock = (offset / blocksize) & !((1u64 << sb.sb_dirblklog) - 1);
            let fsblock = self.root.map_block(buf_reader, sb, lblock)?;

            let mut cache_guard = self.block_cache.borrow_mut();
            if cache_guard.is_none() || cache_guard.as_ref().unwrap().0 != fsblock
            {
                let mut raw = vec![0u8; dblksize as usize];
                buf_reader
                    .seek(SeekFrom::Start(fsblock * blocksize))
                    .unwrap();
                buf_reader.read_exact(&mut raw).unwrap();
                *cache_guard = Some((fsblock, raw));
            }
            let raw = &cache_guard.as_ref().unwrap().1;

            let mut blk_offset = if offset % dblksize > 0 {
                (offset % dblksize) as usize
            } else {
                Dir3DataHdr::SIZE as usize
            };
            while blk_offset < dblksize as usize {
                let freetag: u16 = decode(&raw[blk_offset..]).unwrap().0;
                if freetag == 0xffff {
                    let (_, length) = decode::<Dir2DataUnused>(&raw[blk_offset..])
                        .unwrap();
                    offset += length as u64;
                    blk_offset += length;
                } else if !next {
                    let length = Dir2DataEntry::get_length(&raw[blk_offset..]);
                    blk_offset += length as usize;
                    offset += length as u64;
                    next = true;
                } else {
                    let (entry, _l)= decode::<Dir2DataEntry >(&raw[blk_offset..]).unwrap();
                    let kind = get_file_type(FileKind::Type(entry.ftype))?;
                    let name = entry.name;
                    let entry_offset = doffset + entry.tag as u64;
                    return Ok((entry.inumber, entry_offset as i64, kind, name));
                }
            }
        }
    }
}
