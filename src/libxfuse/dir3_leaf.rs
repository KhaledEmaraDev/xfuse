use std::collections::HashMap;
use std::io::{BufRead, Seek, SeekFrom};
use std::mem;

use super::bmbt_rec::BmbtRec;
use super::da_btree::{hashname, XfsDa3Blkinfo};
use super::definitions::*;
use super::dinode::Dinode;
use super::dir3::{
    Dir2DataEntry, Dir2DataUnused, Dir2LeafEntry, Dir3, Dir3DataHdr, XfsDir2DataOff,
    XfsDir2Dataptr, XFS_DIR3_FT_DIR, XFS_DIR3_FT_REG_FILE,
};
use super::sb::Sb;

use byteorder::{BigEndian, ReadBytesExt};
use fuse::{FileAttr, FileType};
use libc::{c_int, mode_t, ENOENT, S_IFDIR, S_IFMT, S_IFREG};
use time::Timespec;

// pub const XFS_DIR2_LEAF_OFFSET: u64 = (32 * 1024 * 1024 * 1024) / (4096);

#[derive(Debug)]
pub struct Dir3LeafHdr {
    pub info: XfsDa3Blkinfo,
    pub count: u16,
    pub stale: u16,
    pub pad: u32,
}

impl Dir3LeafHdr {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir3LeafHdr {
        let info = XfsDa3Blkinfo::from(buf_reader);
        let count = buf_reader.read_u16::<BigEndian>().unwrap();
        let stale = buf_reader.read_u16::<BigEndian>().unwrap();
        let pad = buf_reader.read_u32::<BigEndian>().unwrap();

        Dir3LeafHdr {
            info,
            count,
            stale,
            pad,
        }
    }
}

#[derive(Debug)]
pub struct Dir2Data {
    pub hdr: Dir3DataHdr,

    pub offset: u64,
}

impl Dir2Data {
    pub fn from<T: BufRead + Seek>(
        buf_reader: &mut T,
        superblock: &Sb,
        start_block: u64,
    ) -> Dir2Data {
        let offset = start_block * (superblock.sb_blocksize as u64);
        buf_reader.seek(SeekFrom::Start(offset as u64)).unwrap();

        let hdr = Dir3DataHdr::from(buf_reader.by_ref());

        Dir2Data { hdr, offset }
    }
}

#[derive(Debug)]
pub struct Dir2LeafTail {
    pub bestcount: u32,
}

impl Dir2LeafTail {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir2LeafTail {
        let bestcount = buf_reader.read_u32::<BigEndian>().unwrap();

        Dir2LeafTail { bestcount }
    }
}

#[derive(Debug)]
pub struct Dir2LeafDisk {
    pub hdr: Dir3LeafHdr,
    pub ents: Vec<Dir2LeafEntry>,
    pub bests: Vec<XfsDir2DataOff>,
    pub tail: Dir2LeafTail,
}

impl Dir2LeafDisk {
    pub fn from<T: BufRead + Seek>(buf_reader: &mut T, offset: u64, size: u32) -> Dir2LeafDisk {
        buf_reader.seek(SeekFrom::Start(offset)).unwrap();

        let hdr = Dir3LeafHdr::from(buf_reader.by_ref());

        let mut ents = Vec::<Dir2LeafEntry>::new();
        for _i in 0..hdr.count {
            let leaf_entry = Dir2LeafEntry::from(buf_reader.by_ref());
            ents.push(leaf_entry);
        }

        buf_reader
            .seek(SeekFrom::Start(
                offset + (size as u64) - (mem::size_of::<Dir2LeafTail>() as u64),
            ))
            .unwrap();

        let tail = Dir2LeafTail::from(buf_reader.by_ref());

        let data_end = offset + (size as u64)
            - (mem::size_of::<Dir2LeafTail>() as u64)
            - ((mem::size_of::<XfsDir2DataOff>() as u64) * (tail.bestcount as u64));
        buf_reader.seek(SeekFrom::Start(data_end)).unwrap();

        let mut bests = Vec::<XfsDir2DataOff>::new();
        for _i in 0..tail.bestcount {
            bests.push(buf_reader.read_u16::<BigEndian>().unwrap());
        }

        Dir2LeafDisk {
            hdr,
            ents,
            bests,
            tail,
        }
    }
}

#[derive(Debug)]
pub struct Dir2Leaf {
    pub entries: Vec<Dir2Data>,
    pub hashes: HashMap<XfsDahash, XfsDir2Dataptr>,
    pub entry_size: u32,
}

impl Dir2Leaf {
    pub fn from<T: BufRead + Seek>(
        buf_reader: &mut T,
        superblock: &Sb,
        bmx: &Vec<BmbtRec>,
    ) -> Dir2Leaf {
        let mut entries = Vec::<Dir2Data>::new();
        for i in 0..bmx.len() - 1 {
            let entry = Dir2Data::from(buf_reader.by_ref(), &superblock, bmx[i].br_startblock);
            entries.push(entry);
        }

        let leaf_extent = bmx.last().unwrap();
        let offset = leaf_extent.br_startblock * (superblock.sb_blocksize as u64);
        let entry_size = superblock.sb_blocksize * (1 << superblock.sb_dirblklog);

        let dir_disk = Dir2LeafDisk::from(buf_reader, offset, entry_size);

        let mut hashes = HashMap::new();
        for leaf_entry in dir_disk.ents {
            hashes.insert(leaf_entry.hashval, leaf_entry.address);
        }

        Dir2Leaf {
            entries,
            hashes,
            entry_size,
        }
    }
}

impl Dir3 for Dir2Leaf {
    fn lookup<T: BufRead + Seek>(
        &self,
        buf_reader: &mut T,
        super_block: &Sb,
        name: &str,
    ) -> Result<(FileAttr, u64), c_int> {
        let hash = hashname(name);
        if let Some(address) = self.hashes.get(&hash) {
            let address = (*address as u64) * 8;
            let idx = (address / (self.entry_size as u64)) as usize;
            let address = address % (self.entry_size as u64);

            if idx >= self.entries.len() {
                return Err(ENOENT);
            }
            let entry: &Dir2Data = &self.entries[idx];

            buf_reader
                .seek(SeekFrom::Start(entry.offset + address))
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

    fn next<T: BufRead + Seek>(
        &self,
        buf_reader: &mut T,
        offset: i64,
    ) -> Result<(XfsIno, i64, FileType, String), c_int> {
        let offset = offset as u64;
        let mut idx: usize = (offset >> (64 - 5)) as usize; // In V5 Inodes can contain up to 21 Extents
        let offset = offset & ((1 << (64 - 5)) - 1);

        let mut next = offset == 0;
        let offset = if offset == 0 {
            mem::size_of::<Dir3DataHdr>() as u64
        } else {
            offset
        };

        if idx >= self.entries.len() {
            return Err(ENOENT);
        }
        let mut entry: &Dir2Data = &self.entries[idx];

        buf_reader
            .seek(SeekFrom::Start(entry.offset + (offset as u64)))
            .unwrap();

        loop {
            while buf_reader.stream_position().unwrap() < (entry.offset + (self.entry_size as u64))
            {
                let freetag = buf_reader.read_u16::<BigEndian>().unwrap();
                buf_reader.seek(SeekFrom::Current(-2)).unwrap();

                if freetag == 0xffff {
                    Dir2DataUnused::from(buf_reader.by_ref());
                } else {
                    let entry = Dir2DataEntry::from(buf_reader.by_ref());

                    if next {
                        let tag = ((idx << (64 - 5)) as u64) | (entry.tag as u64);

                        let kind = match entry.ftype {
                            XFS_DIR3_FT_REG_FILE => FileType::RegularFile,
                            XFS_DIR3_FT_DIR => FileType::Directory,
                            _ => {
                                println!("Type Error");
                                return Err(ENOENT);
                            }
                        };

                        let name = String::from(entry.name);

                        return Ok((entry.inumber, tag as i64, kind, name));
                    } else {
                        next = true;
                    }
                }
            }

            idx += 1;

            if idx >= self.entries.len() {
                break;
            }
            entry = &self.entries[idx];

            buf_reader
                .seek(SeekFrom::Start(
                    entry.offset + (mem::size_of::<Dir3DataHdr>() as u64),
                ))
                .unwrap();
        }

        return Err(ENOENT);
    }
}
