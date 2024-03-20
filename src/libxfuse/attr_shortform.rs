/*
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
use std::ffi::OsStr;
use std::io::{BufRead, Seek};
use std::os::unix::ffi::OsStrExt;

use super::{
    attr::{get_namespace_from_flags, get_namespace_size_from_flags, Attr},
    sb::Sb,
};

use bincode::{
    Decode,
    de::{Decoder, read::Reader},
    error::DecodeError
};

#[derive(Debug, Clone, Decode)]
pub struct AttrSfHdr {
    pub totsize: u16,
    pub count: u8,
    _padding: u8,
}

#[derive(Debug, Clone)]
pub struct AttrSfEntry {
    pub namelen: u8,
    pub valuelen: u8,
    pub flags: u8,
    pub nameval: Vec<u8>,
}

impl Decode for AttrSfEntry {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let namelen: u8 = Decode::decode(decoder)?;
        let valuelen: u8 = Decode::decode(decoder)?;
        let flags: u8 = Decode::decode(decoder)?;
        let mut nameval = vec![0u8; usize::from(namelen + valuelen)];
        decoder.reader().read(&mut nameval[..])?;

        Ok(AttrSfEntry {
            namelen,
            valuelen,
            flags,
            nameval
        })
    }
}

#[derive(Debug, Clone)]
pub struct AttrShortform {
    pub hdr: AttrSfHdr,
    pub list: Vec<AttrSfEntry>,

    pub total_size: u32,
}

impl Decode for AttrShortform {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let hdr: AttrSfHdr = Decode::decode(decoder)?;

        let mut list = Vec::<AttrSfEntry>::new();
        let mut total_size: u32 = 0;

        for _ in 0..hdr.count {
            let entry: AttrSfEntry = Decode::decode(decoder)?;
            total_size += get_namespace_size_from_flags(entry.flags) + u32::from(entry.namelen) + 1;
            list.push(entry);
        }

        Ok(AttrShortform {
            hdr,
            list,
            total_size
        })
    }
}

impl<R: BufRead + Seek> Attr<R> for AttrShortform {
    fn get_total_size(&mut self, _buf_reader: &mut R, _super_block: &Sb) -> u32 {
        self.total_size
    }

    fn list(&mut self, buf_reader: &mut R, super_block: &Sb) -> Vec<u8> {
        let mut list: Vec<u8> =
            Vec::with_capacity(self.get_total_size(buf_reader.by_ref(), super_block) as usize);

        for entry in self.list.iter() {
            list.extend_from_slice(get_namespace_from_flags(entry.flags));
            let namelen = entry.namelen as usize;
            list.extend_from_slice(&entry.nameval[0..namelen]);
            list.push(0)
        }

        list
    }

    fn get(&self, _buf_reader: &mut R, _super_block: &Sb, name: &OsStr) -> Result<Vec<u8>, i32> {
        for entry in &self.list {
            let entry_name = entry.nameval[0..(entry.namelen as usize)].to_vec();

            if name.as_bytes().to_vec() == entry_name {
                let namelen = entry.namelen as usize;

                return Ok(entry.nameval[namelen..].to_vec());
            }
        }

        Err(libc::ENOATTR)
    }
}
