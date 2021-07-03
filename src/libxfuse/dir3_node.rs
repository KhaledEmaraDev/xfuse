use std::collections::HashMap;
use std::io::{BufRead, Seek, SeekFrom};
use std::mem;

use super::bmbt_rec::BmbtRec;
use super::da_btree::{hashname, XfsDa3Intnode};
use super::definitions::*;
use super::dinode::Dinode;
use super::dir3::{
    Dir2Data, Dir2DataEntry, Dir2DataUnused, Dir2LeafDisk, Dir3, Dir3BlkHdr, Dir3DataHdr,
    XfsDir2Dataptr, XFS_DIR3_FT_DIR, XFS_DIR3_FT_REG_FILE,
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
    pub entries: Vec<Dir2Data>,
    pub hashes: HashMap<XfsDahash, XfsDir2Dataptr>,
    pub entry_size: u32,

    pub node: XfsDa3Intnode,
    pub free: Dir3Free,
}

impl Dir2Node {
    pub fn from<T: BufRead + Seek>(
        buf_reader: &mut T,
        superblock: &Sb,
        bmx: &Vec<BmbtRec>,
    ) -> Dir2Node {
        let mut i: usize = 0;
        let mut entries = Vec::<Dir2Data>::new();
        while i < bmx.len() && bmx[i].br_startoff < superblock.get_dir3_leaf_offset() {
            let entry = Dir2Data::from(buf_reader.by_ref(), &superblock, bmx[i].br_startblock);
            entries.push(entry);

            i += 1;
        }

        let entry_size = superblock.sb_blocksize * (1 << superblock.sb_dirblklog);
        let mut node: Option<XfsDa3Intnode> = None;

        let mut hashes = HashMap::new();

        while i < bmx.len() && bmx[i].br_startoff < superblock.get_dir3_free_offset() {
            let offset = bmx[i].br_startblock * (superblock.sb_blocksize as u64);
            buf_reader.seek(SeekFrom::Start(offset as u64)).unwrap();

            buf_reader.seek(SeekFrom::Current(8)).unwrap();
            let magic = buf_reader.read_u16::<BigEndian>().unwrap();
            buf_reader.seek(SeekFrom::Current(-8)).unwrap();

            match magic {
                XFS_DA3_NODE_MAGIC => {
                    node = Some(XfsDa3Intnode::from(buf_reader));
                }
                XFS_DIR3_LEAFN_MAGIC => {
                    let leaf = Dir2LeafDisk::from(buf_reader, offset, entry_size);

                    for leaf_entry in leaf.ents {
                        hashes.insert(leaf_entry.hashval, leaf_entry.address);
                    }
                }
                _ => {
                    panic!("Magic number is invalid");
                }
            }

            i += 1;
        }

        let mut free: Option<Dir3Free> = None;

        if i < bmx.len() {
            let offset = bmx[i].br_startblock * (superblock.sb_blocksize as u64);
            buf_reader.seek(SeekFrom::Start(offset as u64)).unwrap();

            free = Some(Dir3Free::from(buf_reader, offset, entry_size));
        }

        Dir2Node {
            entries,
            hashes,
            entry_size,
            node: node.unwrap(),
            free: free.unwrap(),
        }
    }
}

impl Dir3 for Dir2Node {
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
