use std::io::BufRead;

use super::definitions::*;

use byteorder::{BigEndian, ReadBytesExt};
use uuid::Uuid;

macro_rules! rol32 {
    ($x:expr, $y:expr) => {
        ((($x) << ($y)) | (($x) >> (32 - ($y))))
    };
}

pub fn hashname(name: &str) -> XfsDahash {
    let name = name.as_bytes();
    let mut namelen = name.len();
    let mut hash: XfsDahash = 0;

    let mut i: usize = 0;
    while namelen >= 4 {
        hash = ((name[i] as u32) << 21)
            ^ ((name[i + 1] as u32) << 14)
            ^ ((name[i + 2] as u32) << 7)
            ^ (name[i + 3] as u32)
            ^ rol32!(hash, 7 * 4);

        namelen -= 4;
        i += 4;
    }

    match namelen {
        3 => {
            ((name[i] as u32) << 14)
                ^ ((name[i + 1] as u32) << 7)
                ^ (name[i + 2] as u32)
                ^ rol32!(hash, 7 * 3)
        }
        2 => ((name[i] as u32) << 7) ^ (name[i + 1] as u32) ^ rol32!(hash, 7 * 2),
        1 => (name[i] as u32) ^ rol32!(hash, 7),
        _ => hash,
    }
}

#[derive(Debug)]
pub struct XfsDa3Blkinfo {
    pub forw: u32,
    pub back: u32,
    pub magic: u16,
    pub pad: u16,

    pub crc: u32,
    pub blkno: u64,
    pub lsn: u64,
    pub uuid: Uuid,
    pub owner: u64,
}

impl XfsDa3Blkinfo {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> XfsDa3Blkinfo {
        let forw = buf_reader.read_u32::<BigEndian>().unwrap();
        let back = buf_reader.read_u32::<BigEndian>().unwrap();
        let magic = buf_reader.read_u16::<BigEndian>().unwrap();
        let pad = buf_reader.read_u16::<BigEndian>().unwrap();

        let crc = buf_reader.read_u32::<BigEndian>().unwrap();
        let blkno = buf_reader.read_u64::<BigEndian>().unwrap();
        let lsn = buf_reader.read_u64::<BigEndian>().unwrap();
        let uuid = Uuid::from_u128(buf_reader.read_u128::<BigEndian>().unwrap());
        let owner = buf_reader.read_u64::<BigEndian>().unwrap();

        XfsDa3Blkinfo {
            forw,
            back,
            magic,
            pad,
            crc,
            blkno,
            lsn,
            uuid,
            owner,
        }
    }
}

#[derive(Debug)]
pub struct XfsDa3NodeHdr {
    pub info: XfsDa3Blkinfo,
    pub count: u16,
    pub level: u16,
    pub pad32: u32,
}

impl XfsDa3NodeHdr {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> XfsDa3NodeHdr {
        let info = XfsDa3Blkinfo::from(buf_reader.by_ref());
        let count = buf_reader.read_u16::<BigEndian>().unwrap();
        let level = buf_reader.read_u16::<BigEndian>().unwrap();
        let pad32 = buf_reader.read_u32::<BigEndian>().unwrap();

        XfsDa3NodeHdr {
            info,
            count,
            level,
            pad32,
        }
    }
}

#[derive(Debug)]
pub struct XfsDa3NodeEntry {
    pub hashval: XfsDahash,
    pub before: XfsDablk,
}

impl XfsDa3NodeEntry {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> XfsDa3NodeEntry {
        let hashval = buf_reader.read_u32::<BigEndian>().unwrap();
        let before = buf_reader.read_u32::<BigEndian>().unwrap();

        XfsDa3NodeEntry { hashval, before }
    }
}

#[derive(Debug)]
pub struct XfsDa3Intnode {
    pub hdr: XfsDa3NodeHdr,
    pub btree: Vec<XfsDa3NodeEntry>,
}

impl XfsDa3Intnode {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> XfsDa3Intnode {
        let hdr = XfsDa3NodeHdr::from(buf_reader.by_ref());

        let mut btree = Vec::<XfsDa3NodeEntry>::new();
        for _i in 0..hdr.count {
            btree.push(XfsDa3NodeEntry::from(buf_reader.by_ref()))
        }

        XfsDa3Intnode { hdr, btree }
    }
}
