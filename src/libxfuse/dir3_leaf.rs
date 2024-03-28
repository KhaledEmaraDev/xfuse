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
    cell::{Ref, RefCell},
    io::{BufRead, Seek, SeekFrom},
    ops::Deref
};

use bincode::{de::read::Reader, Decode};

use tracing::warn;

use super::bmbt_rec::BmbtRec;
use super::definitions::*;
use super::dir3::{Dir2LeafEntry, Dir3, Dir3LeafHdr, XfsDir2Dataptr};
use super::sb::Sb;

#[derive(Debug)]
struct Dir2LeafDisk {
    pub hdr: Dir3LeafHdr,
    pub ents: Vec<Dir2LeafEntry>,
    // bests: not needed for a read-only implementation of XFS
    // tail:  not needed for a read-only implementation of XFS
}

impl Dir2LeafDisk {
    pub fn from<T: BufRead + Seek>(
        buf_reader: &mut T,
        offset: u64,
        size: usize,
    ) -> Dir2LeafDisk {
        buf_reader.seek(SeekFrom::Start(offset)).unwrap();
        let mut raw = vec![0u8; size];
        buf_reader.read_exact(&mut raw).unwrap();
        let config = bincode::config::standard()
            .with_big_endian()
            .with_fixed_int_encoding();
        let reader = bincode::de::read::SliceReader::new(&raw[..]);
        let mut decoder = bincode::de::DecoderImpl::new(reader, config);
        let hdr = Dir3LeafHdr::decode(&mut decoder).unwrap();
        assert_eq!(hdr.info.magic, XFS_DIR3_LEAF1_MAGIC,
            "bad magic! expected {:#x} but found {:#x}", XFS_DIR3_LEAF1_MAGIC, hdr.info.magic);

        let ents = (0..hdr.count).map(|_| {
            Dir2LeafEntry::decode(&mut decoder).unwrap()
        }).collect::<Vec<_>>();

        Dir2LeafDisk {
            hdr,
            ents,
        }
    }
}

#[derive(Debug)]
pub struct Dir2Leaf {
    bmx: Vec<BmbtRec>,
    leaf: Dir2LeafDisk,
    /// A cache of the last extent and its starting block number read by lookup
    /// or readdir.
    block_cache: RefCell<(XfsFsblock, Vec<u8>)>
}

impl Dir2Leaf {
    pub fn from<T: bincode::de::read::Reader + BufRead + Seek>(
        buf_reader: &mut T,
        superblock: &Sb,
        bmx: &[BmbtRec],
    ) -> Dir2Leaf {
        let leaf_extent = bmx.last().unwrap();
        if leaf_extent.br_startblock != superblock.get_dir3_leaf_offset().into() {
            warn!("Leaf directory contains unexpected bmx entry {:?}", &leaf_extent);
        }
        let offset = superblock.fsb_to_offset(leaf_extent.br_startblock);

        let leaf_size = leaf_extent.br_blockcount as usize * superblock.sb_blocksize as usize;
        let leaf = Dir2LeafDisk::from(buf_reader, offset, leaf_size);
        assert_eq!(leaf.hdr.info.magic, XFS_DIR3_LEAF1_MAGIC);

        let dblksize: usize = 1 << (superblock.sb_blocklog + superblock.sb_dirblklog);
        let block_cache = RefCell::new((XfsFsblock::max_value(), Vec::with_capacity(dblksize)));

        Dir2Leaf {
            bmx: bmx.to_vec(),
            leaf,
            block_cache
        }
    }

    fn map_dblock(&self, dblock: XfsDablk) -> Result<XfsFsblock, i32> {
        let dblock = u64::from(dblock);

        let i = self.bmx.partition_point(|rec| rec.br_startoff <= dblock);
        let rec = &self.bmx[i - 1];
        if i == 0 || rec.br_startoff > dblock || rec.br_startoff + rec.br_blockcount <= dblock {
            Err(libc::ENOENT)
        } else {
            Ok(rec.br_startblock + dblock - rec.br_startoff)
        }
    }

    fn read_fsblock<'a, R>(&'a self, mut buf_reader: R, sb: &Sb, fsblock: XfsFsblock)
        -> Result<Box<dyn Deref<Target=[u8]> + 'a>, i32>
        where R: Reader + BufRead + Seek
    {
        let dblksize: usize = 1 << (sb.sb_blocklog + sb.sb_dirblklog);

        let mut cache_guard = self.block_cache.borrow_mut();
        if cache_guard.0 != fsblock || cache_guard.1.len() != dblksize {
            cache_guard.1.resize(dblksize, 0u8);
            buf_reader
                .seek(SeekFrom::Start(sb.fsb_to_offset(fsblock)))
                .unwrap();
            buf_reader.read_exact(&mut cache_guard.1).unwrap();
            cache_guard.0 = fsblock;
        }
        // Annoyingly, there's no function to downgrade a RefMut into a Ref.
        drop(cache_guard);
        let cache_guard = self.block_cache.borrow();
        Ok(Box::new(Ref::map(cache_guard, |v| v.1.as_ref())))
    }
}

impl Dir3 for Dir2Leaf {
    fn get_addresses<'a, R>(&'a self, _buf_reader: &'a RefCell<&'a mut R>, hash: XfsDahash)
        -> Box<dyn Iterator<Item=XfsDir2Dataptr> + 'a>
            where R: Reader + BufRead + Seek + 'a
    {
        let i = self.leaf.ents.partition_point(|ent| ent.hashval < hash);
        let l = self.leaf.ents.len();
        let j = (i..l).find(|x| self.leaf.ents[*x].hashval > hash).unwrap_or(l);
        Box::new(self.leaf.ents[i..j].iter().map(|ent| ent.address << 3))
    }

    fn read_dblock<'a, R>(&'a self, mut buf_reader: R, sb: &Sb, dblock: XfsDablk)
        -> Result<Box<dyn Deref<Target=[u8]> + 'a>, i32>
        where R: Reader + BufRead + Seek
    {
        let fsblock = self.map_dblock(dblock)?;
        self.read_fsblock(buf_reader.by_ref(), sb, fsblock)
    }
}
