use std::cmp::Ordering;
use std::io::{BufRead, Seek, SeekFrom};
use std::mem;

use super::bmbt_rec::BmbtRec;
use super::btree::BtreeBlock;
use super::definitions::*;
use super::dir3::{
    Dir2DataEntry, Dir2DataUnused, Dir3, Dir3DataHdr, XFS_DIR3_FT_DIR, XFS_DIR3_FT_REG_FILE,
};
use super::sb::Sb;

use byteorder::{BigEndian, ReadBytesExt};
use fuse::{FileAttr, FileType};
use libc::{c_int, ENOENT};

#[derive(Debug, Clone)]
pub struct BmdrBlock {
    pub bb_level: u16,
    pub bb_numrecs: u16,
}

impl BmdrBlock {
    pub fn from<R: BufRead>(buf_reader: &mut R) -> BmdrBlock {
        let bb_level = buf_reader.read_u16::<BigEndian>().unwrap();
        let bb_numrecs = buf_reader.read_u16::<BigEndian>().unwrap();

        BmdrBlock {
            bb_level,
            bb_numrecs,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BmbtKey {
    pub br_startoff: XfsFileoff,
}

impl BmbtKey {
    pub fn from<R: BufRead>(buf_reader: &mut R) -> BmbtKey {
        let br_startoff = buf_reader.read_u64::<BigEndian>().unwrap();

        BmbtKey { br_startoff }
    }
}

pub type XfsBmbtPtr = XfsFsblock;
pub type XfsBmdrPtr = XfsFsblock;
pub type XfsBmbtBlock = BtreeBlock<u64>;

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
}

impl Dir3 for Dir2Btree {
    fn lookup<T: BufRead + Seek>(
        &self,
        _buf_reader: &mut T,
        _super_block: &Sb,
        _name: &str,
    ) -> Result<(FileAttr, u64), c_int> {
        todo!();
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

        let mut bmbt: Option<XfsBmbtBlock> = None;
        let mut bmbt_rec: Option<BmbtRec> = None;
        let mut bmbt_block_offset = 0;
        let mut bmbt_rec_idx = 0;

        for (i, BmbtKey { br_startoff: key }) in self.keys.iter().rev().enumerate() {
            if idx >= *key {
                let i = self.keys.len() - 1 - i;

                bmbt_block_offset = self.pointers[i] * (self.block_size as u64);
                buf_reader.seek(SeekFrom::Start(bmbt_block_offset)).unwrap();

                bmbt = Some(XfsBmbtBlock::from(buf_reader.by_ref()))
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

                match key.cmp(&idx) {
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
            bmbt = Some(XfsBmbtBlock::from(buf_reader.by_ref()));
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

                match key.cmp(&idx) {
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

            if let Some(bmbt_rec_some) = &bmbt_rec {
                bmbt_block_offset = buf_reader.stream_position().unwrap();
                bmbt_rec_idx = idx - bmbt_rec_some.br_startoff;
            } else {
                return Err(ENOENT);
            }
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

                            let kind = match entry.ftype {
                                XFS_DIR3_FT_REG_FILE => FileType::RegularFile,
                                XFS_DIR3_FT_DIR => FileType::Directory,
                                _ => {
                                    println!("Type Error");
                                    return Err(ENOENT);
                                }
                            };

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
                bmbt = Some(XfsBmbtBlock::from(buf_reader.by_ref()));

                bmbt_rec = Some(BmbtRec::from(buf_reader.by_ref()));

                bmbt_rec_idx = 0;

                offset = mem::size_of::<Dir3DataHdr>() as u64;
            }
        }

        Err(ENOENT)
    }
}
