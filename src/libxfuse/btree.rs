use std::{io::prelude::*, mem};

use byteorder::{BigEndian, ReadBytesExt};
use num_traits::{PrimInt, Unsigned};

#[derive(Debug)]
pub struct BtreeBlock<T> {
    pub bb_magic: u32,
    pub bb_level: u16,
    pub bb_numrecs: u16,
    pub bb_leftsib: T,
    pub bb_rightsib: T,
}

impl<T: PrimInt + Unsigned> BtreeBlock<T> {
    pub fn from<R: BufRead>(buf_reader: &mut R) -> BtreeBlock<T> {
        let bb_magic = buf_reader.read_u32::<BigEndian>().unwrap();
        let bb_level = buf_reader.read_u16::<BigEndian>().unwrap();
        let bb_numrecs = buf_reader.read_u16::<BigEndian>().unwrap();

        let type_size = mem::size_of::<T>();
        let bb_leftsib = T::from(buf_reader.read_uint::<BigEndian>(type_size).unwrap()).unwrap();
        let bb_rightsib = T::from(buf_reader.read_uint::<BigEndian>(type_size).unwrap()).unwrap();

        BtreeBlock {
            bb_magic,
            bb_level,
            bb_numrecs,
            bb_leftsib,
            bb_rightsib,
        }
    }
}
