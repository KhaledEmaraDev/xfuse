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
use std::cmp::Ordering;
use std::io::{BufRead, Seek, SeekFrom};
use std::mem;
use std::time::{Duration, UNIX_EPOCH};

use super::S_IFMT;
use super::bmbt_rec::BmbtRec;
use super::btree::{BmbtKey, BmdrBlock, XfsBmbtBlock, XfsBmbtPtr};
use super::da_btree::{hashname, XfsDa3NodeEntry, XfsDa3NodeHdr};
use super::definitions::*;
use super::dinode::Dinode;
use super::dir3::{Dir2DataEntry, Dir2DataUnused, Dir2LeafEntry, Dir3, Dir3DataHdr, Dir3LeafHdr};
use super::sb::Sb;
use super::utils::{get_file_type, FileKind};

use byteorder::{BigEndian, ReadBytesExt};
use fuser::{FileAttr, FileType};
use libc::{c_int, ENOENT};

#[derive(Debug)]
pub struct Dir2Btree {
    pub bmbt: BmdrBlock,
    pub keys: Vec<BmbtKey>,
    pub pointers: Vec<XfsBmbtPtr>,
    pub block_size: u32,
}

impl Dir2Btree {
    pub fn from(
        bmbt: BmdrBlock,
        keys: Vec<BmbtKey>,
        pointers: Vec<XfsBmbtPtr>,
        block_size: u32,
    ) -> Dir2Btree {
        Dir2Btree {
            bmbt,
            keys,
            pointers,
            block_size,
        }
    }

    pub fn map_dblock<R: BufRead + Seek>(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        dblock: XfsDablk,
    ) -> (Option<XfsBmbtBlock>, Option<BmbtRec>) {
        let mut bmbt: Option<XfsBmbtBlock> = None;
        let mut bmbt_rec: Option<BmbtRec> = None;
        let mut bmbt_block_offset = 0;

        for (i, BmbtKey { br_startoff: key }) in self.keys.iter().enumerate().rev() {
            if dblock as u64 >= *key {
                bmbt_block_offset = self.pointers[i] * (self.block_size as u64);
                buf_reader.seek(SeekFrom::Start(bmbt_block_offset)).unwrap();

                bmbt = Some(XfsBmbtBlock::from(buf_reader.by_ref(), super_block))
            }
        }

        while let Some(bmbt_some) = &bmbt {
            if bmbt_some.bb_level == 0 {
                break;
            }

            let mut l: i64 = 0;
            let mut r: i64 = (bmbt_some.bb_numrecs - 1) as i64;

            let mut predecessor = 0;

            while l <= r {
                let m = (l + r) / 2;

                buf_reader
                    .seek(SeekFrom::Start(
                        bmbt_block_offset
                            + (mem::size_of::<XfsBmbtBlock>() as u64)
                            + ((m as u64) * (mem::size_of::<BmbtKey>() as u64)),
                    ))
                    .unwrap();
                let key = BmbtKey::from(buf_reader.by_ref()).br_startoff;

                match key.cmp(&dblock.into()) {
                    Ordering::Greater => {
                        r = m - 1;
                    }
                    Ordering::Less => {
                        l = m + 1;
                        predecessor = m;
                    }
                    Ordering::Equal => {
                        predecessor = m;
                        break;
                    }
                }
            }

            buf_reader
                .seek(SeekFrom::Start(
                    bmbt_block_offset
                        + (mem::size_of::<XfsBmbtBlock>() as u64)
                        + ((bmbt_some.bb_numrecs as u64) * (mem::size_of::<BmbtKey>() as u64))
                        + ((predecessor as u64) * (mem::size_of::<XfsBmbtPtr>() as u64)),
                ))
                .unwrap();
            let pointer = buf_reader.read_u64::<BigEndian>().unwrap();

            bmbt_block_offset = pointer * (self.block_size as u64);
            buf_reader.seek(SeekFrom::Start(bmbt_block_offset)).unwrap();
            bmbt = Some(XfsBmbtBlock::from(buf_reader.by_ref(), super_block));
        }

        if let Some(bmbt_some) = &bmbt {
            let mut l: i64 = 0;
            let mut r: i64 = (bmbt_some.bb_numrecs - 1) as i64;

            let mut predecessor = 0;

            while l <= r {
                let m = (l + r) / 2;

                buf_reader
                    .seek(SeekFrom::Start(
                        bmbt_block_offset
                            + (mem::size_of::<XfsBmbtBlock>() as u64)
                            + ((m as u64) * (mem::size_of::<BmbtRec>() as u64)),
                    ))
                    .unwrap();
                let key = BmbtRec::from(buf_reader.by_ref()).br_startoff;

                match key.cmp(&dblock.into()) {
                    Ordering::Greater => {
                        r = m - 1;
                    }
                    Ordering::Less => {
                        l = m + 1;
                        predecessor = m;
                    }
                    Ordering::Equal => {
                        predecessor = m;
                        break;
                    }
                }
            }

            buf_reader
                .seek(SeekFrom::Start(
                    bmbt_block_offset
                        + (mem::size_of::<XfsBmbtBlock>() as u64)
                        + ((predecessor as u64) * (mem::size_of::<BmbtRec>() as u64)),
                ))
                .unwrap();
            bmbt_rec = Some(BmbtRec::from(buf_reader.by_ref()));
        }

        (bmbt, bmbt_rec)
    }
}

impl<R: BufRead + Seek> Dir3<R> for Dir2Btree {
    fn lookup(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        name: &str,
    ) -> Result<(FileAttr, u64), c_int> {
        let idx = super_block.get_dir3_leaf_offset();
        let hash = hashname(name);

        let (_, bmbt_rec) = self.map_dblock(buf_reader.by_ref(), super_block, idx as u32);
        let mut hdr: Option<XfsDa3NodeHdr>;

        if let Some(bmbt_rec_some) = &bmbt_rec {
            buf_reader
                .seek(SeekFrom::Start(
                    (bmbt_rec_some.br_startblock) * (self.block_size as u64),
                ))
                .unwrap();

            hdr = Some(XfsDa3NodeHdr::from(buf_reader.by_ref(), super_block));

            while let Some(hdr_some) = &hdr {
                loop {
                    let entry = XfsDa3NodeEntry::from(buf_reader.by_ref());
                    if entry.hashval > hash {
                        let (_, bmbt_rec) =
                            self.map_dblock(buf_reader.by_ref(), super_block, entry.before);

                        if let Some(bmbt_rec_some) = &bmbt_rec {
                            buf_reader
                                .seek(SeekFrom::Start(
                                    (bmbt_rec_some.br_startblock) * (self.block_size as u64),
                                ))
                                .unwrap();

                            break;
                        } else {
                            return Err(ENOENT);
                        }
                    }
                }

                if hdr_some.level == 1 {
                    break;
                } else {
                    hdr = Some(XfsDa3NodeHdr::from(buf_reader.by_ref(), super_block));
                }
            }
        } else {
            return Err(ENOENT);
        }

        let hdr = Dir3LeafHdr::from(buf_reader.by_ref(), super_block);

        for _i in 0..hdr.count {
            let entry = Dir2LeafEntry::from(buf_reader.by_ref());

            if entry.hashval == hash {
                let address = (entry.address as u64) * 8;
                let idx = (address / (self.block_size as u64)) as usize;
                let address = address % (self.block_size as u64);

                let (_, bmbt_rec) = self.map_dblock(buf_reader.by_ref(), super_block, idx as u32);

                if let Some(bmbt_rec_some) = &bmbt_rec {
                    buf_reader
                        .seek(SeekFrom::Start(
                            (bmbt_rec_some.br_startblock) * (self.block_size as u64),
                        ))
                        .unwrap();

                    buf_reader.seek(SeekFrom::Current(address as i64)).unwrap();

                    let entry = Dir2DataEntry::from(buf_reader.by_ref());

                    let dinode = Dinode::from(buf_reader.by_ref(), super_block, entry.inumber);

                    let kind = get_file_type(FileKind::Mode(dinode.di_core.di_mode))?;

                    let attr = FileAttr {
                        ino: entry.inumber,
                        size: dinode.di_core.di_size as u64,
                        blocks: dinode.di_core.di_nblocks,
                        atime: UNIX_EPOCH + Duration::new(
                            dinode.di_core.di_atime.t_sec as u64,
                            dinode.di_core.di_atime.t_nsec,
                        ),
                        mtime: UNIX_EPOCH + Duration::new(
                            dinode.di_core.di_mtime.t_sec as u64,
                            dinode.di_core.di_mtime.t_nsec
                        ),
                        ctime: UNIX_EPOCH + Duration::new(
                            dinode.di_core.di_ctime.t_sec as u64,
                            dinode.di_core.di_ctime.t_nsec
                        ),
                        crtime: UNIX_EPOCH,
                        kind,
                        perm: dinode.di_core.di_mode & !S_IFMT,
                        nlink: dinode.di_core.di_nlink,
                        uid: dinode.di_core.di_uid,
                        gid: dinode.di_core.di_gid,
                        rdev: 0,
                        blksize: 0,
                        flags: 0,
                    };

                    return Ok((attr, dinode.di_core.di_gen.into()));
                } else {
                    return Err(ENOENT);
                };
            }
        }

        Err(ENOENT)
    }

    fn next(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        offset: i64,
    ) -> Result<(XfsIno, i64, FileType, String), c_int> {
        let offset = offset as u64;
        let idx = offset >> (64 - 48); // tags take 16-bits
        let offset = offset & ((1 << (64 - 48)) - 1);

        let mut next = offset == 0;
        let mut offset = if offset == 0 {
            mem::size_of::<Dir3DataHdr>() as u64
        } else {
            offset
        };

        let (mut bmbt, mut bmbt_rec) =
            self.map_dblock(buf_reader.by_ref(), super_block, idx as u32);
        let mut bmbt_block_offset;
        let mut bmbt_rec_idx;

        if let Some(bmbt_rec_some) = &bmbt_rec {
            bmbt_block_offset = buf_reader.stream_position().unwrap();
            bmbt_rec_idx = idx - bmbt_rec_some.br_startoff;
        } else {
            return Err(ENOENT);
        }

        while let Some(bmbt_some) = &bmbt {
            while let Some(bmbt_rec_some) = &bmbt_rec {
                while bmbt_rec_idx < bmbt_rec_some.br_blockcount {
                    buf_reader
                        .seek(SeekFrom::Start(
                            (bmbt_rec_some.br_startblock + bmbt_rec_idx) * (self.block_size as u64),
                        ))
                        .unwrap();

                    buf_reader.seek(SeekFrom::Current(offset as i64)).unwrap();

                    while buf_reader.stream_position().unwrap()
                        < ((bmbt_rec_some.br_startblock + bmbt_rec_idx + 1)
                            * (self.block_size as u64))
                    {
                        let freetag = buf_reader.read_u16::<BigEndian>().unwrap();
                        buf_reader.seek(SeekFrom::Current(-2)).unwrap();

                        if freetag == 0xffff {
                            Dir2DataUnused::from(buf_reader.by_ref());
                        } else if next {
                            let entry = Dir2DataEntry::from(buf_reader.by_ref());

                            let kind = get_file_type(FileKind::Type(entry.ftype))?;

                            let tag = ((bmbt_rec_some.br_startoff + bmbt_rec_idx)
                                & 0xFFFFFFFFFFFF0000)
                                | (entry.tag as u64);

                            let name = entry.name;

                            return Ok((entry.inumber, tag as i64, kind, name));
                        } else {
                            let length = Dir2DataEntry::get_length(buf_reader.by_ref());
                            buf_reader.seek(SeekFrom::Current(length)).unwrap();

                            next = true;
                        }
                    }

                    bmbt_rec_idx += 1;

                    offset = mem::size_of::<Dir3DataHdr>() as u64;
                }

                if bmbt_block_offset + (mem::size_of::<BmbtRec>() as u64) > (self.block_size as u64)
                {
                    break;
                } else {
                    bmbt_rec = Some(BmbtRec::from(buf_reader.by_ref()));

                    bmbt_rec_idx = 0;

                    offset = mem::size_of::<Dir3DataHdr>() as u64;
                }
            }

            if bmbt_some.bb_rightsib == 0 {
                break;
            } else {
                bmbt_block_offset = bmbt_some.bb_rightsib * (self.block_size as u64);
                buf_reader.seek(SeekFrom::Start(bmbt_block_offset)).unwrap();
                bmbt = Some(XfsBmbtBlock::from(buf_reader.by_ref(), super_block));

                bmbt_rec = Some(BmbtRec::from(buf_reader.by_ref()));

                bmbt_rec_idx = 0;

                offset = mem::size_of::<Dir3DataHdr>() as u64;
            }
        }

        Err(ENOENT)
    }
}
