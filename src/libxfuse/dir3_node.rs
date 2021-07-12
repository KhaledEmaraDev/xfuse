use std::io::{BufRead, Seek, SeekFrom};
use std::mem;

use super::bmbt_rec::BmbtRec;
use super::da_btree::{hashname, XfsDa3NodeEntry, XfsDa3NodeHdr};
use super::definitions::*;
use super::dinode::Dinode;
use super::dir3::{
    Dir2DataEntry, Dir2DataUnused, Dir2LeafEntry, Dir3, Dir3BlkHdr, Dir3DataHdr, Dir3LeafHdr,
    XFS_DIR3_FT_DIR, XFS_DIR3_FT_REG_FILE,
};
use super::sb::Sb;

use byteorder::{BigEndian, ReadBytesExt};
use fuse::{FileAttr, FileType};
use libc::{c_int, mode_t, ENOENT, S_IFDIR, S_IFMT, S_IFREG};
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

    pub fn map_dblock(&self, dblock: XfsDablk) -> Option<&BmbtRec> {
        for record in self.bmx.iter() {
            if record.br_startoff == dblock as u64 {
                return Some(record);
            }
        }

        None
    }
}

impl Dir3 for Dir2Node {
    fn lookup<T: BufRead + Seek>(
        &self,
        buf_reader: &mut T,
        super_block: &Sb,
        name: &str,
    ) -> Result<(FileAttr, u64), c_int> {
        let dblock = super_block.get_dir3_leaf_offset();
        let hash = hashname(name);

        let bmbt_rec = self.map_dblock(dblock as u32);
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
        buf_reader.seek(SeekFrom::Current(-8)).unwrap();

        if magic.unwrap() == XFS_DA3_NODE_MAGIC {
            let mut hdr = XfsDa3NodeHdr::from(buf_reader.by_ref());

            loop {
                loop {
                    let entry = XfsDa3NodeEntry::from(buf_reader.by_ref());

                    if entry.hashval > hash {
                        let bmbt_rec = self.map_dblock(entry.before);

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

                if hdr.level == 1 {
                    break;
                } else {
                    hdr = XfsDa3NodeHdr::from(buf_reader.by_ref());
                }
            }
        }

        let hdr = Dir3LeafHdr::from(buf_reader.by_ref());

        for _i in 0..hdr.count {
            let entry = Dir2LeafEntry::from(buf_reader.by_ref());

            if entry.hashval == hash {
                let address = (entry.address as u64) * 8;
                let idx = address / (self.block_size as u64);
                let address = address % (self.block_size as u64);

                let bmbt_rec = self.map_dblock(idx as u32);

                if let Some(bmbt_rec_some) = &bmbt_rec {
                    buf_reader
                        .seek(SeekFrom::Start(
                            (bmbt_rec_some.br_startblock) * (self.block_size as u64),
                        ))
                        .unwrap();

                    buf_reader.seek(SeekFrom::Current(address as i64)).unwrap();

                    let entry = Dir2DataEntry::from(buf_reader.by_ref());

                    let dinode = Dinode::from(buf_reader.by_ref(), super_block, entry.inumber);

                    let kind = match (dinode.di_core.di_mode as mode_t) & S_IFMT {
                        S_IFREG => FileType::RegularFile,
                        S_IFDIR => FileType::Directory,
                        _ => {
                            return Err(ENOENT);
                        }
                    };

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

                    return Ok((attr, dinode.di_core.di_gen.into()));
                } else {
                    return Err(ENOENT);
                };
            }
        }

        Err(ENOENT)
    }

    fn next<T: BufRead + Seek>(
        &self,
        buf_reader: &mut T,
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

        let mut bmbt_rec = self.map_dblock(idx as u32);
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

                        let kind = match entry.ftype {
                            XFS_DIR3_FT_REG_FILE => FileType::RegularFile,
                            XFS_DIR3_FT_DIR => FileType::Directory,
                            _ => {
                                println!("Type Error");
                                return Err(ENOENT);
                            }
                        };

                        let tag = ((bmbt_rec_some.br_startoff + bmbt_rec_idx) & 0xFFFFFFFFFFFF0000)
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

            bmbt_rec = self.map_dblock((bmbt_rec_some.br_startoff + bmbt_rec_idx) as u32);

            bmbt_rec_idx = 0;

            offset = mem::size_of::<Dir3DataHdr>() as u64;
        }

        Err(ENOENT)
    }
}
