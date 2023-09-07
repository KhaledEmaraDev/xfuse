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
use std::convert::TryInto;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, Seek, SeekFrom};
use std::mem;
use std::time::{Duration, UNIX_EPOCH};

use super::S_IFMT;
use super::bmbt_rec::BmbtRec;
use super::da_btree::{hashname, XfsDa3Intnode};
use super::definitions::*;
use super::dinode::Dinode;
use super::dir3::{Dir2DataEntry, Dir2DataUnused, Dir2LeafNDisk, Dir3, Dir3BlkHdr, Dir3DataHdr};
use super::sb::Sb;
use super::utils::{decode_from, get_file_type, FileKind};

use byteorder::{BigEndian, ReadBytesExt};
use fuser::{FileAttr, FileType};
use libc::{c_int, ENOENT};

#[derive(Debug)]
pub struct Dir3FreeHdr {
    pub hdr: Dir3BlkHdr,
    pub firstdb: i32,
    pub nvalid: i32,
    pub nused: i32,
    pub pad: i32,
}

impl Dir3FreeHdr {
    pub fn from<T: bincode::de::read::Reader + BufRead>(buf_reader: &mut T) -> Dir3FreeHdr {
        let hdr = decode_from(buf_reader.by_ref()).unwrap();
        let firstdb = buf_reader.read_i32::<BigEndian>().unwrap();
        let nvalid = buf_reader.read_i32::<BigEndian>().unwrap();
        let nused = buf_reader.read_i32::<BigEndian>().unwrap();
        let pad = buf_reader.read_i32::<BigEndian>().unwrap();

        Dir3FreeHdr {
            hdr,
            firstdb,
            nvalid,
            nused,
            pad,
        }
    }
}

#[derive(Debug)]
pub struct Dir3Free {
    pub hdr: Dir3FreeHdr,
    pub bests: Vec<u16>,
}

impl Dir3Free {
    pub fn from<T: bincode::de::read::Reader + BufRead + Seek>(buf_reader: &mut T, offset: u64, size: u32) -> Dir3Free {
        buf_reader.seek(SeekFrom::Start(offset)).unwrap();

        let hdr = Dir3FreeHdr::from(buf_reader);

        let data_end =
            offset + (size as u64) - ((mem::size_of::<u16>() as u64) * (hdr.nvalid as u64));
        buf_reader.seek(SeekFrom::Start(data_end)).unwrap();

        let mut bests = Vec::<u16>::new();
        for _i in 0..hdr.nvalid {
            bests.push(buf_reader.read_u16::<BigEndian>().unwrap());
        }

        Dir3Free { hdr, bests }
    }
}

#[derive(Debug)]
pub struct Dir2Node {
    pub bmx: Vec<BmbtRec>,
    pub block_size: u32,
}

impl Dir2Node {
    pub fn from(bmx: Vec<BmbtRec>, block_size: u32) -> Dir2Node {
        Dir2Node { bmx, block_size }
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

        let bmbt_rec = self.map_dblock(dblock);
        if let Some(bmbt_rec_some) = bmbt_rec {
            buf_reader
                .seek(SeekFrom::Start(
                    (bmbt_rec_some.br_startblock) * (self.block_size as u64),
                ))
                .unwrap();
        } else {
            return Err(ENOENT);
        }

        buf_reader.seek(SeekFrom::Current(8)).unwrap();
        let magic = buf_reader.read_u16::<BigEndian>();
        buf_reader.seek(SeekFrom::Current(-10)).unwrap();

        if magic.unwrap() == XFS_DA3_NODE_MAGIC {
            let node = XfsDa3Intnode::from(buf_reader.by_ref(), super_block);
            let blk = node.lookup(buf_reader.by_ref(), super_block, hash, |block, _| {
                self.map_dblock_number(block.into())
            });

            let leaf_offset = blk * u64::from(super_block.sb_blocksize);

            buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();
        }

        let leaf = Dir2LeafNDisk::from(buf_reader.by_ref(), super_block);

        let address = leaf.get_address(hash)? * 8;
        let idx = (address / super_block.sb_blocksize) as usize;
        let address = address % super_block.sb_blocksize;

        let blk = self.map_dblock_number(idx as u64);
        buf_reader
            .seek(SeekFrom::Start(blk * u64::from(super_block.sb_blocksize)))
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
                dinode.di_core.di_mtime.t_nsec,
            ),
            ctime: UNIX_EPOCH + Duration::new(
                dinode.di_core.di_ctime.t_sec as u64,
                dinode.di_core.di_ctime.t_nsec,
            ),
            crtime: UNIX_EPOCH,
            kind,
            perm: dinode.di_core.di_mode & !S_IFMT,
            nlink: dinode.di_core.di_nlink,
            uid: dinode.di_core.di_uid,
            gid: dinode.di_core.di_gid,
            rdev: 0,
            blksize: 16384,
            flags: 0,
        };

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
            let ex_offset = offset - bmbt_rec.br_startoff * block_size;

            buf_reader.seek(
                SeekFrom::Start(bmbt_rec.br_startblock * block_size + ex_offset)
            ).unwrap();

            while buf_reader.stream_position().unwrap() <
                (bmbt_rec.br_startblock + bmbt_rec.br_blockcount) * block_size
            {
                // Byte offset within this directory block
                let dir_block_offset = offset % ((1 << sb.sb_dirblklog) * block_size);
                // Offset of this directory block within its extent
                let dboffset = offset - dir_block_offset;

                // If this is the start of the directory block, skip it.
                if dir_block_offset == 0 {
                    buf_reader.seek(SeekFrom::Current(Dir3DataHdr::SIZE as i64))
                        .unwrap();
                    offset += Dir3DataHdr::SIZE;
                }

                // Skip the next directory entry
                let freetag = buf_reader.read_u16::<BigEndian>().unwrap();
                buf_reader.seek(SeekFrom::Current(-2)).unwrap();
                if freetag == 0xffff {
                    let unused = Dir2DataUnused::from(buf_reader.by_ref());
                    offset += u64::from(unused.length);
                } else if !next {
                    let length = Dir2DataEntry::get_length_from_reader(buf_reader.by_ref());
                    buf_reader.seek(SeekFrom::Current(length)).unwrap();
                    offset += length as u64;
                    next = true;
                } else {
                    let entry = Dir2DataEntry::from(buf_reader.by_ref());
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
