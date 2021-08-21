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
