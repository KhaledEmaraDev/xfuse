/**
 * BSD 2-Clause License
 *
 * Copyright (c) 2021, Khaled Emara
 * All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions are met:
 *
 * 1. Redistributions of source code must retain the above copyright notice, this
 *    list of conditions and the following disclaimer.
 *
 * 2. Redistributions in binary form must reproduce the above copyright notice,
 *    this list of conditions and the following disclaimer in the documentation
 *    and/or other materials provided with the distribution.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
 * AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
 * DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
 * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 * SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
 * CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
 * OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
 * OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
 */
use std::io::prelude::*;

use super::definitions::*;

use bincode::{
    Decode,
    de::Decoder,
    error::DecodeError
};
use byteorder::{BigEndian, ReadBytesExt};
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;

#[derive(Debug, FromPrimitive, Clone)]
pub enum XfsExntst {
    Norm,
    Unwritten,
    Invalid,
}

#[derive(Debug, Clone)]
pub struct BmbtRec {
    pub br_startoff: XfsFileoff,
    pub br_startblock: XfsFsblock,
    pub br_blockcount: XfsFilblks,
    pub br_state: XfsExntst,
}

impl BmbtRec {
    pub const SIZE: usize = 16;

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

impl Decode for BmbtRec {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let br: u128 = Decode::decode(decoder)?;

        let br_blockcount = (br & ((1 << 21) - 1)) as u64;
        let br = br >> 21;

        let br_startblock = (br & ((1 << 52) - 1)) as u64;
        let br = br >> 52;

        let br_startoff = (br & ((1 << 54) - 1)) as u64;
        let br = br >> 54;

        let br_state = XfsExntst::from_u8((br & 1) as u8).unwrap();

        Ok(BmbtRec {
            br_startoff,
            br_startblock,
            br_blockcount,
            br_state,
        })
    }
}
