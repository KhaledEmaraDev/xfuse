use std::io::BufRead;

use byteorder::{BigEndian, ReadBytesExt};

pub type XfsDir2DataOff = u64;
pub type XfsDir2Dataptr = u32;
// pub type XfsDir2SfOff = [u8; 2];

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
pub enum XfsDir2Inou {
    XfsDir2Ino8(u64),
    XfsDir2Ino4(u32),
}

#[derive(Debug)]
pub struct Dir2SfHdr {
    pub count: u8,
    pub i8count: u8,
    pub parent: XfsDir2Inou,
}

impl Dir2SfHdr {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir2SfHdr {
        let count = buf_reader.read_u8().unwrap();
        let i8count = buf_reader.read_u8().unwrap();

        let parent = if i8count > 0 {
            XfsDir2Inou::XfsDir2Ino8(buf_reader.read_u64::<BigEndian>().unwrap())
        } else {
            XfsDir2Inou::XfsDir2Ino4(buf_reader.read_u32::<BigEndian>().unwrap())
        };

        Dir2SfHdr {
            count,
            i8count,
            parent,
        }
    }
}

#[derive(Debug)]
pub struct Dir2SfEntry {
    pub namelen: u8,
    pub offset: u16,
    pub name: String,
    pub ftype: u8,
    pub inumber: XfsDir2Inou,
}

impl Dir2SfEntry {
    pub fn from<T: BufRead>(buf_reader: &mut T, i8count: u8) -> Dir2SfEntry {
        let namelen = buf_reader.read_u8().unwrap();

        let offset = buf_reader.read_u16::<BigEndian>().unwrap();

        let mut name = String::new();
        for _i in 0..namelen {
            name.push(buf_reader.read_u8().unwrap() as char);
        }

        let ftype = buf_reader.read_u8().unwrap();

        let inumber = if i8count > 0 {
            XfsDir2Inou::XfsDir2Ino8(buf_reader.read_u64::<BigEndian>().unwrap())
        } else {
            XfsDir2Inou::XfsDir2Ino4(buf_reader.read_u32::<BigEndian>().unwrap())
        };

        Dir2SfEntry {
            namelen,
            offset,
            name,
            ftype,
            inumber,
        }
    }
}

#[derive(Debug)]
pub struct Dir2Sf {
    pub hdr: Dir2SfHdr,
    pub list: Vec<Dir2SfEntry>,
}

impl Dir2Sf {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Dir2Sf {
        let hdr = Dir2SfHdr::from(buf_reader.by_ref());

        let mut list = Vec::<Dir2SfEntry>::new();
        for _i in 0..hdr.count {
            list.push(Dir2SfEntry::from(buf_reader.by_ref(), hdr.i8count))
        }

        Dir2Sf { hdr, list }
    }
}
