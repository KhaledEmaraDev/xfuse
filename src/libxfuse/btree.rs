use std::{io::prelude::*, mem};

use byteorder::{BigEndian, LittleEndian, ReadBytesExt};
use num_traits::{PrimInt, Unsigned};
use uuid::Uuid;

#[derive(Debug)]
pub struct BtreeBlock<T: PrimInt + Unsigned> {
    pub bb_magic: u32,
    pub bb_level: u16,
    pub bb_numrecs: u16,
    pub bb_leftsib: T,
    pub bb_rightsib: T,
    pub bb_blkno: u64,
    pub bb_lsn: u64,
    pub bb_uuid: Uuid,
    pub bb_owner: u64,
    pub bb_crc: u32,
    pub bb_pad: u32,
}

impl<T: PrimInt + Unsigned> BtreeBlock<T> {
    pub fn from<R: BufRead>(buf_reader: &mut R) -> BtreeBlock<T> {
        let bb_magic = buf_reader.read_u32::<BigEndian>().unwrap();
        let bb_level = buf_reader.read_u16::<BigEndian>().unwrap();
        let bb_numrecs = buf_reader.read_u16::<BigEndian>().unwrap();

        let type_size = mem::size_of::<T>();
        let bb_leftsib = T::from(buf_reader.read_uint::<BigEndian>(type_size).unwrap()).unwrap();
        let bb_rightsib = T::from(buf_reader.read_uint::<BigEndian>(type_size).unwrap()).unwrap();

        let bb_blkno = buf_reader.read_u64::<BigEndian>().unwrap();
        let bb_lsn = buf_reader.read_u64::<BigEndian>().unwrap();
        let bb_uuid = Uuid::from_u128(buf_reader.read_u128::<BigEndian>().unwrap());
        let bb_owner = buf_reader.read_u64::<BigEndian>().unwrap();
        let bb_crc = buf_reader.read_u32::<LittleEndian>().unwrap();
        let bb_pad = buf_reader.read_u32::<BigEndian>().unwrap();

        BtreeBlock {
            bb_magic,
            bb_level,
            bb_numrecs,
            bb_leftsib,
            bb_rightsib,
            bb_blkno,
            bb_lsn,
            bb_uuid,
            bb_owner,
            bb_crc,
            bb_pad,
        }
    }
}
