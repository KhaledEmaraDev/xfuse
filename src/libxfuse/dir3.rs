use std::io::{BufRead, Seek, SeekFrom};

use super::definitions::*;
use super::sb::Sb;

use byteorder::{BigEndian, ReadBytesExt};
use fuse::{FileAttr, FileType};
use libc::c_int;
use uuid::Uuid;

pub type XfsDir2DataOff = u16;
pub type XfsDir2Dataptr = u32;

pub const XFS_DIR2_DATA_FD_COUNT: usize = 3;

pub const XFS_DIR3_FT_UNKNOWN: u8 = 0;
pub const XFS_DIR3_FT_REG_FILE: u8 = 1;
pub const XFS_DIR3_FT_DIR: u8 = 2;
pub const XFS_DIR3_FT_CHRDEV: u8 = 3;
pub const XFS_DIR3_FT_BLKDEV: u8 = 4;
pub const XFS_DIR3_FT_FIFO: u8 = 5;
pub const XFS_DIR3_FT_SOCK: u8 = 6;
pub const XFS_DIR3_FT_SYMLINK: u8 = 7;
pub const XFS_DIR3_FT_WHT: u8 = 8;

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

pub trait Dir3 {
    fn lookup<T: BufRead + Seek>(
        &self,
        buf_reader: &mut T,
        super_block: &Sb,
        name: &str,
    ) -> Result<(FileAttr, u64), c_int>;

    fn next<T: BufRead + Seek>(
        &self,
        buf_reader: &mut T,
        offset: i64,
    ) -> Result<(XfsIno, i64, FileType, String), c_int>;
}
