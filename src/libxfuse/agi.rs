use std::io::prelude::*;

use super::definitions::*;

use byteorder::{BigEndian, ReadBytesExt};

#[derive(Debug)]
pub struct Agi {
    pub agi_magicnum: u32,
    pub agi_versionnum: u32,
    pub agi_seqno: u32,
    pub agi_length: u32,
    pub agi_count: u32,
    pub agi_root: u32,
    pub agi_level: u32,
    pub agi_freecount: u32,
    pub agi_newino: u32,
    pub agi_dirino: u32,
    pub agi_unlinked: [u32; 64],
}

impl Agi {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> Agi {
        let agi_magicnum = buf_reader.read_u32::<BigEndian>().unwrap();
        if agi_magicnum != XFS_AGI_MAGIC {
            panic!("Agi magic number is invalid");
        }

        let agi_versionnum = buf_reader.read_u32::<BigEndian>().unwrap();
        let agi_seqno = buf_reader.read_u32::<BigEndian>().unwrap();
        let agi_length = buf_reader.read_u32::<BigEndian>().unwrap();
        let agi_count = buf_reader.read_u32::<BigEndian>().unwrap();
        let agi_root = buf_reader.read_u32::<BigEndian>().unwrap();
        let agi_level = buf_reader.read_u32::<BigEndian>().unwrap();
        let agi_freecount = buf_reader.read_u32::<BigEndian>().unwrap();
        let agi_newino = buf_reader.read_u32::<BigEndian>().unwrap();
        let agi_dirino = buf_reader.read_u32::<BigEndian>().unwrap();

        let mut agi_unlinked = [0u32; 64];
        for item in agi_unlinked.iter_mut() {
            *item = buf_reader.read_u32::<BigEndian>().unwrap();
        }

        Agi {
            agi_magicnum,
            agi_versionnum,
            agi_seqno,
            agi_length,
            agi_count,
            agi_root,
            agi_level,
            agi_freecount,
            agi_newino,
            agi_dirino,
            agi_unlinked,
        }
    }
}
