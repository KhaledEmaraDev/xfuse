use std::io::prelude::*;

use super::definitions::*;

use byteorder::{BigEndian, ReadBytesExt};
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;

#[derive(Debug, FromPrimitive, Clone)]
pub enum XfsExntst {
    XfsExtNorm,
    XfsExtUnwritten,
    XfsExtInvalid,
}

#[derive(Debug, Clone)]
pub struct BmbtRec {
    pub br_startoff: XfsFileoff,
    pub br_startblock: XfsFsblock,
    pub br_blockcount: XfsFilblks,
    pub br_state: XfsExntst,
}

impl BmbtRec {
    pub fn from<T: BufRead>(buf_reader: &mut T) -> BmbtRec {
        let br = buf_reader.read_u128::<BigEndian>().unwrap();

        let br_blockcount = (br & ((1 << 21) - 1)) as u64;
        let br = br >> 21;

        let br_startblock = (br & ((1 << 52) - 1)) as u64;
        let br = br >> 52;

        let br_startoff = (br & ((1 << 54) - 1)) as u64;
        let br = br >> 54;

        let br_state = XfsExntst::from_u8((br & 1) as u8).unwrap();

        BmbtRec {
            br_startoff,
            br_startblock,
            br_blockcount,
            br_state,
        }
    }
}
