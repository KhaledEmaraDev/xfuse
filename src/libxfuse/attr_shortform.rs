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
use std::{
    ffi::OsStr,
    io::{BufRead, Seek},
    os::unix::ffi::OsStrExt,
};

use bincode_next::{
    de::{read::Reader, Decoder},
    error::DecodeError,
    Decode,
};

use super::{
    attr::{get_namespace_from_flags, get_namespace_size_from_flags, Attr},
    sb::Sb,
};

#[derive(Debug, Clone, Decode)]
pub struct AttrSfHdr {
    _totsize:  u16,
    pub count: u8,
    _padding:  u8,
}

#[derive(Debug, Clone)]
pub struct AttrSfEntry {
    pub namelen: u8,
    pub flags:   u8,
    pub nameval: Vec<u8>,
}

impl<Ctx> Decode<Ctx> for AttrSfEntry {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let namelen: u8 = Decode::decode(decoder)?;
        let valuelen: u8 = Decode::decode(decoder)?;
        let flags: u8 = Decode::decode(decoder)?;
        let mut nameval = vec![0u8; usize::from(namelen + valuelen)];
        decoder.reader().read(&mut nameval[..])?;

        Ok(AttrSfEntry {
            namelen,
            flags,
            nameval,
        })
    }
}

#[derive(Debug, Clone)]
pub struct AttrShortform {
    pub list: Vec<AttrSfEntry>,

    pub total_size: u32,
}

impl<Ctx> Decode<Ctx> for AttrShortform {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let hdr: AttrSfHdr = Decode::decode(decoder)?;

        let mut list = Vec::<AttrSfEntry>::new();
        let mut total_size: u32 = 0;

        for _ in 0..hdr.count {
            let entry: AttrSfEntry = Decode::decode(decoder)?;
            total_size += get_namespace_size_from_flags(entry.flags) + u32::from(entry.namelen) + 1;
            list.push(entry);
        }

        Ok(AttrShortform { list, total_size })
    }
}

impl Attr for AttrShortform {
    fn get_total_size<R: BufRead + Reader + Seek>(
        &mut self,
        _buf_reader: &mut R,
        _super_block: &Sb,
    ) -> u32 {
        self.total_size
    }

    fn list<R: BufRead + Reader + Seek>(
        &mut self,
        buf_reader: &mut R,
        super_block: &Sb,
    ) -> Vec<u8> {
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

    fn get<R>(
        &mut self,
        _buf_reader: &mut R,
        _super_block: &Sb,
        name: &OsStr,
    ) -> Result<Vec<u8>, i32>
    where
        R: BufRead + Reader + Seek,
    {
        for entry in &self.list {
            let entry_name = entry.nameval[0..(entry.namelen as usize)].to_vec();

            if name.as_bytes().to_vec() == entry_name {
                let namelen = entry.namelen as usize;

                return Ok(entry.nameval[namelen..].to_vec());
            }
        }

        Err(crate::libxfuse::ENOATTR)
    }

    fn set<R>(
        &mut self,
        _buf_reader: &mut R,
        _super_block: &Sb,
        name: &OsStr,
        value: &[u8],
        flags: u32,
    ) -> Result<u32, libc::c_int>
    where
        R: BufRead + Reader + Seek,
    {
        let name_bytes = name.as_bytes();
        let namelen = name_bytes.len() as u8;

        // Check if attribute already exists
        if let Some(idx) = self
            .list
            .iter()
            .position(|e| e.nameval[0..e.namelen as usize] == *name_bytes)
        {
            // Replace existing attribute
            let entry = &mut self.list[idx];
            let old_namelen = entry.namelen as usize;
            let old_valuelen = entry.nameval.len() - old_namelen;
            let old_total = old_namelen + old_valuelen;

            // Update the entry
            entry.namelen = namelen;
            entry.flags = flags as u8;
            entry.nameval = [name_bytes, value].concat();

            let _new_total = name_bytes.len() + value.len();
            self.total_size = self.total_size - old_total as u32 + _new_total as u32;
        } else {
            // Add new attribute
            let new_entry = AttrSfEntry {
                namelen,
                flags: flags as u8,
                nameval: [name_bytes, value].concat(),
            };
            let _new_total = name_bytes.len() + value.len();
            self.total_size += get_namespace_size_from_flags(flags as u8)
                + u32::from(namelen)
                + value.len() as u32;
            self.list.push(new_entry);
        }

        Ok(self.total_size)
    }

    fn remove<R>(
        &mut self,
        _buf_reader: &mut R,
        _super_block: &Sb,
        name: &OsStr,
    ) -> Result<u32, libc::c_int>
    where
        R: BufRead + Reader + Seek,
    {
        let name_bytes = name.as_bytes();

        if let Some(idx) = self
            .list
            .iter()
            .position(|e| e.nameval[0..e.namelen as usize] == *name_bytes)
        {
            let entry = self.list[idx].clone();
            let namelen = entry.namelen as usize;
            let valuelen = entry.nameval.len() - namelen;

            self.list.remove(idx);
            self.total_size -= get_namespace_size_from_flags(entry.flags)
                + u32::from(entry.namelen)
                + valuelen as u32;
            Ok(self.total_size)
        } else {
            Err(crate::libxfuse::ENOATTR)
        }
    }

    fn serialize(&mut self, _super_block: &Sb) -> Result<Vec<u8>, i32> {
        let mut buf = Vec::with_capacity(self.total_size as usize);

        // Write header
        buf.extend_from_slice(&self.total_size.to_be_bytes());
        buf.extend_from_slice(&[self.list.len() as u8, 0]); // count + padding

        // Write entries
        for entry in &self.list {
            buf.push(entry.namelen);
            let valuelen = (entry.nameval.len() - entry.namelen as usize) as u8;
            buf.push(valuelen);
            buf.push(entry.flags);
            buf.extend_from_slice(&entry.nameval);
        }

        Ok(buf)
    }
}
