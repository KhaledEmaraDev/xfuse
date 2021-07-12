use std::collections::HashMap;
use std::io::{BufRead, Seek, SeekFrom};
use std::mem;

use super::da_btree::hashname;
use super::definitions::*;
use super::dinode::Dinode;
use super::dir3::{
    Dir2DataEntry, Dir2DataUnused, Dir2LeafEntry, Dir3, Dir3DataHdr, XfsDir2Dataptr,
    XFS_DIR3_FT_DIR, XFS_DIR3_FT_REG_FILE,
};
use super::sb::Sb;

use byteorder::{BigEndian, ReadBytesExt};
use fuse::{FileAttr, FileType};
use libc::{c_int, mode_t, ENOENT, S_IFDIR, S_IFMT, S_IFREG};
use time::Timespec;

pub const XFS_DIR2_DATA_FD_COUNT: usize = 3;

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
pub struct Dir2BlockDisk {
    pub hdr: Dir3DataHdr,
    // pub u: Vec<Dir2DataUnion>,
    pub leaf: Vec<Dir2LeafEntry>,
    pub tail: Dir2BlockTail,
}

impl Dir2BlockDisk {
    pub fn from<T: BufRead + Seek>(buf_reader: &mut T, offset: u64, size: u32) -> Dir2BlockDisk {
        buf_reader.seek(SeekFrom::Start(offset)).unwrap();

        let hdr = Dir3DataHdr::from(buf_reader.by_ref());

        buf_reader
            .seek(SeekFrom::Start(
                offset + (size as u64) - (mem::size_of::<Dir2BlockTail>() as u64),
            ))
            .unwrap();

        let tail = Dir2BlockTail::from(buf_reader.by_ref());

        let data_end = offset + (size as u64)
            - (mem::size_of::<Dir2BlockTail>() as u64)
            - ((mem::size_of::<Dir2LeafEntry>() as u64) * (tail.count as u64));

        buf_reader.seek(SeekFrom::Start(data_end)).unwrap();

        let mut leaf = Vec::<Dir2LeafEntry>::new();
        for _i in 0..tail.count {
            let leaf_entry = Dir2LeafEntry::from(buf_reader.by_ref());
            leaf.push(leaf_entry);
        }

        Dir2BlockDisk { hdr, leaf, tail }
    }

    pub fn get_data_end(&self, offset: u64, directory_block_size: u32) -> u64 {
        offset + (directory_block_size as u64)
            - (mem::size_of::<Dir2BlockTail>() as u64)
            - ((mem::size_of::<Dir2LeafEntry>() as u64) * (self.tail.count as u64))
    }
}

#[derive(Debug)]
pub struct Dir2Block {
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
        let offset = start_block * (superblock.sb_blocksize as u64);
        let dir_blk_size = superblock.sb_blocksize * (1 << superblock.sb_dirblklog);

        let dir_disk = Dir2BlockDisk::from(buf_reader.by_ref(), offset, dir_blk_size);

        let data_end = dir_disk.get_data_end(offset, dir_blk_size);

        let mut hashes = HashMap::new();
        for leaf_entry in dir_disk.leaf {
            hashes.insert(leaf_entry.hashval, leaf_entry.address);
        }

        Dir2Block {
            offset,
            data_end,
            hashes,
        }
    }
}

impl Dir3 for Dir2Block {
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

            Ok((attr, dinode.di_core.di_gen.into()))
        } else {
            Err(ENOENT)
        }
    }

    fn next<T: BufRead + Seek>(
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

                let name = entry.name;

                return Ok((entry.inumber, entry.tag.into(), kind, name));
            } else {
                let length = Dir2DataEntry::get_length(buf_reader.by_ref());
                buf_reader.seek(SeekFrom::Current(length)).unwrap();

                next = true;
            }
        }

        Err(ENOENT)
    }
}
