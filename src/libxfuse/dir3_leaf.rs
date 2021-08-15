use std::io::{BufRead, Seek, SeekFrom};
use std::mem;

use super::bmbt_rec::BmbtRec;
use super::da_btree::hashname;
use super::definitions::*;
use super::dinode::Dinode;
use super::dir3::{Dir2Data, Dir2DataEntry, Dir2DataUnused, Dir2LeafDisk, Dir3, Dir3DataHdr};
use super::sb::Sb;
use super::utils::{get_file_type, FileKind};

use byteorder::{BigEndian, ReadBytesExt};
use fuse::{FileAttr, FileType};
use libc::{c_int, ENOENT, S_IFMT};
use time::Timespec;

#[derive(Debug)]
pub struct Dir2Leaf {
    pub entries: Vec<Dir2Data>,
    pub leaf: Dir2LeafDisk,
    pub entry_size: u32,
}

impl Dir2Leaf {
    pub fn from<T: BufRead + Seek>(
        buf_reader: &mut T,
        superblock: &Sb,
        bmx: &[BmbtRec],
    ) -> Dir2Leaf {
        let mut entries = Vec::<Dir2Data>::new();
        for record in bmx.iter().take(bmx.len() - 1) {
            let entry = Dir2Data::from(buf_reader.by_ref(), superblock, record.br_startblock);
            entries.push(entry);
        }

        let leaf_extent = bmx.last().unwrap();
        let offset = leaf_extent.br_startblock * (superblock.sb_blocksize as u64);
        let entry_size = superblock.sb_blocksize * (1 << superblock.sb_dirblklog);

        let leaf = Dir2LeafDisk::from(buf_reader, superblock, offset, entry_size);

        Dir2Leaf {
            entries,
            leaf,
            entry_size,
        }
    }
}

impl<R: BufRead + Seek> Dir3<R> for Dir2Leaf {
    fn lookup(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        name: &str,
    ) -> Result<(FileAttr, u64), c_int> {
        let hash = hashname(name);

        let address = self.leaf.get_address(hash)? * 8;
        let idx = (address / self.entry_size) as usize;
        let address = address % self.entry_size;

        if idx >= self.entries.len() {
            return Err(ENOENT);
        }
        let entry: &Dir2Data = &self.entries[idx];

        buf_reader
            .seek(SeekFrom::Start(entry.offset + (address as u64)))
            .unwrap();
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
        let mut idx: usize = (offset >> (64 - 8)) as usize; // In V5 Inodes can contain up to 21 Extents
        let offset = offset & ((1 << (64 - 8)) - 1);

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
                } else if next {
                    let entry = Dir2DataEntry::from(buf_reader.by_ref());

                    let kind = get_file_type(FileKind::Type(entry.ftype))?;

                    let name = entry.name;

                    let tag = ((idx as u64) << (64 - 8)) | (entry.tag as u64);

                    return Ok((entry.inumber, tag as i64, kind, name));
                } else {
                    Dir2DataEntry::from(buf_reader.by_ref());

                    next = true;
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

        Err(ENOENT)
    }
}
