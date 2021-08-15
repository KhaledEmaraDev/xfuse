use std::io::{BufRead, Seek, SeekFrom};
use std::mem;

use super::bmbt_rec::BmbtRec;
use super::da_btree::{hashname, XfsDa3Intnode};
use super::definitions::*;
use super::dinode::Dinode;
use super::dir3::{Dir2DataEntry, Dir2DataUnused, Dir2LeafNDisk, Dir3, Dir3BlkHdr, Dir3DataHdr};
use super::sb::Sb;
use super::utils::{get_file_type, FileKind};

use byteorder::{BigEndian, ReadBytesExt};
use fuse::{FileAttr, FileType};
use libc::{c_int, ENOENT, S_IFMT};
use time::Timespec;

#[derive(Debug)]
pub struct Dir3FreeHdr {
    pub hdr: Dir3BlkHdr,
    pub firstdb: i32,
    pub nvalid: i32,
    pub nused: i32,
    pub pad: i32,
}

impl Dir3FreeHdr {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir3FreeHdr {
        let hdr = Dir3BlkHdr::from(buf_reader);
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
    pub fn from<T: BufRead + Seek>(buf_reader: &mut T, offset: u64, size: u32) -> Dir3Free {
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

impl<R: BufRead + Seek> Dir3<R> for Dir2Node {
    fn lookup(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        name: &str,
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
            let blk = node.lookup(buf_reader.by_ref(), &super_block, hash, |block, _| {
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
            atime: Timespec {
                sec: dinode.di_core.di_atime.t_sec as i64,
                nsec: dinode.di_core.di_atime.t_nsec,
            },
            mtime: Timespec {
                sec: dinode.di_core.di_mtime.t_sec as i64,
                nsec: dinode.di_core.di_mtime.t_nsec,
            },
            ctime: Timespec {
                sec: dinode.di_core.di_ctime.t_sec as i64,
                nsec: dinode.di_core.di_ctime.t_nsec,
            },
            crtime: Timespec { sec: 0, nsec: 0 },
            kind,
            perm: dinode.di_core.di_mode & (!(S_IFMT as u16)),
            nlink: dinode.di_core.di_nlink,
            uid: dinode.di_core.di_uid,
            gid: dinode.di_core.di_gid,
            rdev: 0,
            flags: 0,
        };

        Ok((attr, dinode.di_core.di_gen.into()))
    }

    fn next(
        &self,
        buf_reader: &mut R,
        _super_block: &Sb,
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

        let mut bmbt_rec = self.map_dblock(idx);
        let mut bmbt_rec_idx;

        if let Some(bmbt_rec_some) = &bmbt_rec {
            bmbt_rec_idx = idx - bmbt_rec_some.br_startoff;
        } else {
            return Err(ENOENT);
        }

        while let Some(bmbt_rec_some) = &bmbt_rec {
            while bmbt_rec_idx < bmbt_rec_some.br_blockcount {
                buf_reader
                    .seek(SeekFrom::Start(
                        (bmbt_rec_some.br_startblock + bmbt_rec_idx) * (self.block_size as u64),
                    ))
                    .unwrap();

                buf_reader.seek(SeekFrom::Current(offset as i64)).unwrap();

                while buf_reader.stream_position().unwrap()
                    < ((bmbt_rec_some.br_startblock + bmbt_rec_idx + 1) * (self.block_size as u64))
                {
                    let freetag = buf_reader.read_u16::<BigEndian>().unwrap();
                    buf_reader.seek(SeekFrom::Current(-2)).unwrap();

                    if freetag == 0xffff {
                        Dir2DataUnused::from(buf_reader.by_ref());
                    } else if next {
                        let entry = Dir2DataEntry::from(buf_reader.by_ref());

                        let kind = get_file_type(FileKind::Type(entry.ftype))?;

                        let name = entry.name;

                        let tag = ((bmbt_rec_some.br_startoff + bmbt_rec_idx) << (64 - 48))
                            | (entry.tag as u64);

                        return Ok((entry.inumber, tag as i64, kind, name));
                    } else {
                        Dir2DataEntry::from(buf_reader.by_ref());

                        next = true;
                    }
                }

                bmbt_rec_idx += 1;

                offset = mem::size_of::<Dir3DataHdr>() as u64;
            }

            bmbt_rec = self.map_dblock(bmbt_rec_some.br_startoff + bmbt_rec_idx);

            bmbt_rec_idx = 0;

            offset = mem::size_of::<Dir3DataHdr>() as u64;
        }

        Err(ENOENT)
    }
}
