use std::collections::HashMap;
use std::io::{BufRead, Seek, SeekFrom};
use std::mem;

use super::da_btree::hashname;
use super::definitions::*;
use super::dinode::Dinode;
use super::dir2::{Dir2, XfsDir2DataOff, XfsDir2Dataptr, XFS_DIR3_FT_DIR, XFS_DIR3_FT_REG_FILE};
use super::sb::Sb;

use byteorder::{BigEndian, ReadBytesExt};
use fuse::{FileAttr, FileType};
use libc::{c_int, mode_t, ENOENT, S_IFDIR, S_IFMT, S_IFREG};
use time::Timespec;
use uuid::Uuid;

pub const XFS_DIR2_DATA_FD_COUNT: usize = 3;

#[derive(Debug)]
pub struct Dir2DataEntry {
    pub inumber: XfsIno,
    pub namelen: u8,
    pub name: String,
    pub ftype: u8,
    pub tag: XfsDir2DataOff,
}

impl Dir2DataEntry {
    pub fn from<T: BufRead + Seek>(buf_reader: &mut T) -> Dir2DataEntry {
        let inumber = buf_reader.read_u64::<BigEndian>().unwrap();
        let namelen = buf_reader.read_u8().unwrap();

        let mut name = String::new();
        for _i in 0..namelen {
            name.push(buf_reader.read_u8().unwrap() as char);
        }

        let ftype = buf_reader.read_u8().unwrap();

        let pad_off = (((buf_reader.stream_position().unwrap() + 2 + 8 - 1) / 8) * 8)
            - (buf_reader.stream_position().unwrap() + 2);
        buf_reader.seek(SeekFrom::Current(pad_off as i64)).unwrap();

        let tag = buf_reader.read_u16::<BigEndian>().unwrap();

        Dir2DataEntry {
            inumber,
            namelen,
            name,
            ftype,
            tag,
        }
    }
}

#[derive(Debug)]
pub struct Dir2DataUnused {
    pub freetag: u16,
    pub length: XfsDir2DataOff,
    pub tag: XfsDir2DataOff,
}

impl Dir2DataUnused {
    pub fn from<T: BufRead + Seek>(buf_reader: &mut T) -> Dir2DataUnused {
        let freetag = buf_reader.read_u16::<BigEndian>().unwrap();
        let length = buf_reader.read_u16::<BigEndian>().unwrap();

        buf_reader
            .seek(SeekFrom::Current((length - 6) as i64))
            .unwrap();

        let tag = buf_reader.read_u16::<BigEndian>().unwrap();

        Dir2DataUnused {
            freetag,
            length,
            tag,
        }
    }
}

#[derive(Debug)]
pub enum Dir2DataUnion {
    Entry(Dir2DataEntry),
    Unused(Dir2DataUnused),
}

#[derive(Debug, Clone, Copy)]
pub struct Dir2DataFree {
    pub offset: XfsDir2DataOff,
    pub length: XfsDir2DataOff,
}

impl Dir2DataFree {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir2DataFree {
        let offset = buf_reader.read_u16::<BigEndian>().unwrap();
        let length = buf_reader.read_u16::<BigEndian>().unwrap();

        Dir2DataFree { offset, length }
    }
}

#[derive(Debug)]
pub struct Dir3DataHdr {
    pub hdr: Dir3BlkHdr,
    pub best_free: [Dir2DataFree; XFS_DIR2_DATA_FD_COUNT],
    pub pad: u32,
}

impl Dir3DataHdr {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir3DataHdr {
        let hdr = Dir3BlkHdr::from(buf_reader.by_ref());

        let mut best_free = [Dir2DataFree {
            offset: 0,
            length: 0,
        }; XFS_DIR2_DATA_FD_COUNT];
        for i in 0..XFS_DIR2_DATA_FD_COUNT {
            best_free[i] = Dir2DataFree::from(buf_reader.by_ref());
        }

        let pad = buf_reader.read_u32::<BigEndian>().unwrap();

        Dir3DataHdr {
            hdr,
            best_free,
            pad,
        }
    }
}

#[derive(Debug)]
pub struct Dir3BlkHdr {
    pub magic: u32,
    pub crc: u32,
    pub blkno: u64,
    pub lsn: u64,
    pub uuid: Uuid,
    pub owner: u64,
}

impl Dir3BlkHdr {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir3BlkHdr {
        let magic = buf_reader.read_u32::<BigEndian>().unwrap();
        let crc = buf_reader.read_u32::<BigEndian>().unwrap();
        let blkno = buf_reader.read_u64::<BigEndian>().unwrap();
        let lsn = buf_reader.read_u64::<BigEndian>().unwrap();
        let uuid = Uuid::from_u128(buf_reader.read_u128::<BigEndian>().unwrap());
        let owner = buf_reader.read_u64::<BigEndian>().unwrap();

        Dir3BlkHdr {
            magic,
            crc,
            blkno,
            lsn,
            uuid,
            owner,
        }
    }
}

#[derive(Debug)]
pub struct Dir2LeafEntry {
    pub hashval: XfsDahash,
    pub address: XfsDir2Dataptr,
}

impl Dir2LeafEntry {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir2LeafEntry {
        let hashval = buf_reader.read_u32::<BigEndian>().unwrap();
        let address = buf_reader.read_u32::<BigEndian>().unwrap();

        Dir2LeafEntry { hashval, address }
    }
}

#[derive(Debug)]
pub struct Dir2BlockTail {
    pub count: u32,
    pub stale: u32,
}

impl Dir2BlockTail {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir2BlockTail {
        let count = buf_reader.read_u32::<BigEndian>().unwrap();
        let stale = buf_reader.read_u32::<BigEndian>().unwrap();

        Dir2BlockTail { count, stale }
    }
}

#[derive(Debug)]
pub struct Dir2Block {
    pub hdr: Dir3DataHdr,
    // pub u: Vec<Dir2DataUnion>,
    pub leaf: Vec<Dir2LeafEntry>,
    pub tail: Dir2BlockTail,

    pub offset: u64,
    pub data_end: u64,
    pub hashes: HashMap<XfsDahash, XfsDir2Dataptr>,
}

impl Dir2Block {
    pub fn from<T: BufRead + Seek>(
        buf_reader: &mut T,
        superblock: &Sb,
        start_block: u64,
    ) -> Dir2Block {
        let off = start_block * (superblock.sb_blocksize as u64);
        buf_reader.seek(SeekFrom::Start(off as u64)).unwrap();

        let dir_blk_size = superblock.sb_blocksize * (1 << superblock.sb_dirblklog);
        buf_reader
            .seek(SeekFrom::Current(
                (dir_blk_size as i64) - (mem::size_of::<Dir2BlockTail>() as i64),
            ))
            .unwrap();

        let tail = Dir2BlockTail::from(buf_reader.by_ref());

        let data_end = off + (dir_blk_size as u64)
            - (mem::size_of::<Dir2BlockTail>() as u64)
            - ((mem::size_of::<Dir2LeafEntry>() as u64) * (tail.count as u64));

        buf_reader.seek(SeekFrom::Start(data_end)).unwrap();

        let mut hashes = HashMap::new();

        let mut leaf = Vec::<Dir2LeafEntry>::new();
        for _i in 0..tail.count {
            let leaf_entry = Dir2LeafEntry::from(buf_reader.by_ref());
            hashes.insert(leaf_entry.hashval, leaf_entry.address);
            leaf.push(leaf_entry);
        }

        buf_reader.seek(SeekFrom::Start(off as u64)).unwrap();

        let hdr = Dir3DataHdr::from(buf_reader.by_ref());

        if hdr.hdr.magic != XFS_DIR3_BLOCK_MAGIC {
            panic!("Superblock magic number is invalid");
        }

        Dir2Block {
            hdr,
            leaf,
            tail,
            offset: off,
            data_end,
            hashes,
        }
    }
}

impl Dir2 for Dir2Block {
    fn lookup<T: BufRead + Seek>(
        &self,
        buf_reader: &mut T,
        super_block: &Sb,
        name: &str,
    ) -> Result<(FileAttr, u64), c_int> {
        let hash = hashname(name);
        if let Some(address) = self.hashes.get(&hash) {
            let address = (*address as u64) * 8;

            buf_reader
                .seek(SeekFrom::Start(self.offset + address))
                .unwrap();
            let entry = Dir2DataEntry::from(buf_reader.by_ref());

            let dinode = Dinode::from(buf_reader.by_ref(), &super_block, entry.inumber);

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

            Ok((attr, dinode.di_core.di_gen.into()))
        } else {
            return Err(ENOENT);
        }
    }

    fn iterate<T: BufRead + Seek>(
        &self,
        buf_reader: &mut T,
        offset: i64,
    ) -> Result<(XfsIno, i64, FileType, String), c_int> {
        let mut next = offset == 0;
        let offset = if offset == 0 {
            mem::size_of::<Dir3DataHdr>() as i64
        } else {
            offset
        };
        buf_reader
            .seek(SeekFrom::Start(self.offset + (offset as u64)))
            .unwrap();

        while buf_reader.stream_position().unwrap() < self.data_end {
            let freetag = buf_reader.read_u16::<BigEndian>().unwrap();
            buf_reader.seek(SeekFrom::Current(-2)).unwrap();

            if freetag == 0xffff {
                Dir2DataUnused::from(buf_reader.by_ref());
            } else {
                let entry = Dir2DataEntry::from(buf_reader.by_ref());

                if next {
                    let kind = match entry.ftype {
                        XFS_DIR3_FT_REG_FILE => FileType::RegularFile,
                        XFS_DIR3_FT_DIR => FileType::Directory,
                        _ => {
                            println!("Type Error");
                            return Err(ENOENT);
                        }
                    };

                    let name = String::from(entry.name);

                    return Ok((entry.inumber, entry.tag.into(), kind, name));
                } else {
                    next = true;
                }
            }
        }

        return Err(ENOENT);
    }
}
