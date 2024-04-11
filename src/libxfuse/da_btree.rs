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
    cell::{Ref, RefCell},
    collections::{BTreeMap, btree_map::Entry},
    ffi::OsStr,
    io::{BufRead, Seek, SeekFrom},
    os::unix::ffi::OsStrExt
};


use bincode::{
    Decode,
    de::{Decoder, read::Reader},
    error::DecodeError,
    impl_borrow_decode,
};

use byteorder::{BigEndian, ReadBytesExt};

use super::{
    definitions::*,
    utils::Uuid,
    sb::Sb,
    utils::decode_from,
    volume::SUPERBLOCK
};

macro_rules! rol32 {
    ($x:expr, $y:expr) => {
        ((($x) << ($y)) | (($x) >> (32 - ($y))))
    };
}

pub fn hashname(name: &OsStr) -> XfsDahash {
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
    pub magic: u16,
}

impl Decode for XfsDa3Blkinfo {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let forw = Decode::decode(decoder)?;
        let _back: u32 = Decode::decode(decoder)?;
        let magic = Decode::decode(decoder)?;
        let _pad: u16 = Decode::decode(decoder)?;
        let _crc: u32 = Decode::decode(decoder)?;
        let _blkno: u64 = Decode::decode(decoder)?;
        let _lsn: u64 = Decode::decode(decoder)?;
        let uuid: Uuid = Decode::decode(decoder)?;
        let _owner: u64 = Decode::decode(decoder)?;
        assert_eq!(uuid, SUPERBLOCK.get().unwrap().sb_uuid, "UUID mismatch!");

        Ok(XfsDa3Blkinfo {
            forw,
            magic,
        })
    }
}
impl_borrow_decode!(XfsDa3Blkinfo);


#[derive(Debug)]
pub struct XfsDa3NodeHdr {
    pub info: XfsDa3Blkinfo,
    pub count: u16,
    pub level: u16,
}

impl Decode for XfsDa3NodeHdr {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let info: XfsDa3Blkinfo = Decode::decode(decoder)?;
        if info.magic != XFS_DA3_NODE_MAGIC {
            return Err(DecodeError::Other("bad magic"));
        }
        let count = Decode::decode(decoder)?;
        let level = Decode::decode(decoder)?;
        let _pad32: u32 = Decode::decode(decoder)?;
        Ok(XfsDa3NodeHdr {
            info,
            count,
            level,
        })
    }
}
impl_borrow_decode!(XfsDa3NodeHdr);

#[derive(Debug, Decode)]
pub struct XfsDa3NodeEntry {
    pub hashval: XfsDahash,
    pub before: XfsDablk,
}

impl XfsDa3NodeEntry {
    pub fn from<R: BufRead>(buf_reader: &mut R) -> XfsDa3NodeEntry {
        let hashval = buf_reader.read_u32::<BigEndian>().unwrap();
        let before = buf_reader.read_u32::<BigEndian>().unwrap();

        XfsDa3NodeEntry { hashval, before }
    }
}

#[derive(Debug)]
pub struct XfsDa3Intnode {
    pub hdr: XfsDa3NodeHdr,
    pub btree: Vec<XfsDa3NodeEntry>,
    children: RefCell<BTreeMap<XfsDablk, Self>>
}

impl XfsDa3Intnode {
    pub fn from<R: BufRead + Reader + Seek>(buf_reader: &mut R) -> XfsDa3Intnode {
        let hdr: XfsDa3NodeHdr = decode_from(buf_reader.by_ref()).unwrap();
        assert_eq!(hdr.info.magic, XFS_DA3_NODE_MAGIC, "bad magic!  Expected {:#x}, found {:#x}",
                   XFS_DA3_NODE_MAGIC, hdr.info.magic);

        let mut btree = Vec::<XfsDa3NodeEntry>::new();
        for _i in 0..hdr.count {
            btree.push(XfsDa3NodeEntry::from(buf_reader.by_ref()))
        }
        let children = Default::default();

        XfsDa3Intnode { hdr, btree, children }
    }

    pub fn lookup<R: BufRead + Reader + Seek, F: Fn(XfsDablk, &mut R) -> XfsFsblock>(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        hash: u32,
        map_dblock: F,
    ) -> Result<XfsDablk, i32> {
        let pidx = self.btree.partition_point(|k| k.hashval < hash);
        if pidx >= self.btree.len() {
            return Err(libc::ENOENT);
        }
        let before = self.btree[pidx].before;

        if self.hdr.level == 1 {
            Ok(before)
        } else {
            assert!(self.hdr.level > 1);

            let node = self.read_child(buf_reader.by_ref(), super_block, before,
                &map_dblock)?;
            node.lookup(
                buf_reader.by_ref(),
                super_block,
                hash,
                map_dblock,
            )
        }
    }

    pub fn first_block<R, F>(&self, buf_reader: &mut R, super_block: &Sb, map_dblock: F,
        ) -> XfsDablk
        where R: BufRead + Reader + Seek,
              F: Fn(XfsDablk, &mut R) -> XfsFsblock
    {
        if self.hdr.level == 1 {
            self.btree.first().unwrap().before
        } else {
            let before = self.btree.first().unwrap().before;
            let node = self.read_child(buf_reader.by_ref(), super_block, before,
                &map_dblock).unwrap();
            node.first_block(buf_reader.by_ref(), super_block, map_dblock)
        }
    }

    fn read_child<'a, R, F>(
        &'a self,
        buf_reader: &mut R,
        super_block: &Sb,
        dblock: XfsDablk,
        map_dblock: &F
        ) -> Result<impl std::ops::Deref<Target=Self> + 'a, i32>
        where R: BufRead + Reader + Seek,
              F: Fn(XfsDablk, &mut R) -> XfsFsblock
    {
        let mut cache_guard = self.children.borrow_mut();
        let entry = cache_guard.entry(dblock);
        if matches!(entry, Entry::Vacant(_)) {
            let fsblock = map_dblock(dblock, buf_reader.by_ref());
            let offset = super_block.fsb_to_offset(fsblock);
            buf_reader.seek(SeekFrom::Start(offset)).unwrap();
            let node = XfsDa3Intnode::from(buf_reader.by_ref());
            entry.or_insert(node);
        }
        // Annoyingly, there's no function to downgrade a RefMut into a Ref.
        drop(cache_guard);
        let cache_guard = self.children.borrow();
        Ok(Ref::map(cache_guard, |v| &v[&dblock]))
    }
}


impl Decode for XfsDa3Intnode {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let hdr: XfsDa3NodeHdr = Decode::decode(decoder)?;
        let mut btree = Vec::<XfsDa3NodeEntry>::new();
        for _i in 0..hdr.count {
            btree.push(Decode::decode(decoder)?);
        }
        let children = Default::default();

        Ok(XfsDa3Intnode {
            hdr,
            btree,
            children
        })
    }
}

/// Not really a "test" per se.  Instead it finds hash collisions to use in other tests.
#[test]
#[ignore = "Not a real test"]
fn hashname_collisions() {
    use std::{
        collections::HashMap,
        ffi::OsString
    };

    let way = 4;
    let want = 10;
    let mut allnames = HashMap::new();
    let mut collisions = 0;
    for i in 0u64.. {
        let name = OsString::from(format!("{:x}", i));
        let hash = hashname(&name);
        if let Some(mut v) = allnames.insert(hash, vec![name.clone()]) {
            v.push(name);
            if v.len() >= way {
                println!("{}-way hash collision found: {}", way,
                         v.join(OsStr::new(" ")).to_string_lossy());
                collisions += 1;
            }
            allnames.insert(hash, v);
        }
        if collisions >= want {
            break;
        }
    }
}
