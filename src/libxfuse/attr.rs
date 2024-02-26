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
    io::{BufRead, Seek, SeekFrom},
};

use bincode::{
    Decode,
    de::{Decoder, read::Reader},
    error::DecodeError,
    impl_borrow_decode
};
use byteorder::{BigEndian, ReadBytesExt};
use uuid::Uuid;

use super::{
    attr_leaf::AttrLeaf,
    attr_node::AttrNode,
    bmbt_rec::BmbtRec,
    da_btree::{XfsDa3Blkinfo, XfsDa3Intnode},
    definitions::{XFS_ATTR3_LEAF_MAGIC, XFS_DA3_NODE_MAGIC, XfsFileoff, XfsFsblock},
    sb::Sb,
    utils,
    volume::SUPERBLOCK
};

#[allow(dead_code)]
mod constants {
    pub const XFS_ATTR_LOCAL_BIT: u8 = 0;
    pub const XFS_ATTR_ROOT_BIT: u8 = 1;
    pub const XFS_ATTR_SECURE_BIT: u8 = 2;
    pub const XFS_ATTR_INCOMPLETE_BIT: u8 = 7;
    pub const XFS_ATTR_LOCAL: u8 = 1 << XFS_ATTR_LOCAL_BIT;
    pub const XFS_ATTR_ROOT: u8 = 1 << XFS_ATTR_ROOT_BIT;
    pub const XFS_ATTR_SECURE: u8 = 1 << XFS_ATTR_SECURE_BIT;
    pub const XFS_ATTR_INCOMPLETE: u8 = 1 << XFS_ATTR_INCOMPLETE_BIT;
    pub const XFS_ATTR_NSP_ONDISK_MASK: u8 = XFS_ATTR_ROOT | XFS_ATTR_SECURE;
}

pub const fn get_namespace_from_flags(flags: u8) -> &'static [u8] {
    if flags & constants::XFS_ATTR_SECURE != 0 {
        b"secure."
    } else if flags & constants::XFS_ATTR_ROOT != 0 {
        b"trusted."
    } else {
        b"user."
    }
}

pub const fn get_namespace_size_from_flags(flags: u8) -> u32 {
    get_namespace_from_flags(flags).len() as u32
}

#[derive(Debug, Decode)]
pub struct AttrLeafMap {
    pub base: u16,
    pub size: u16,
}

#[derive(Debug)]
pub struct AttrLeafHdr {
    pub info: XfsDa3Blkinfo,
    pub count: u16,
    pub usedbytes: u16,
    pub firstused: u16,
    pub holes: u8,
    pub pad1: u8,
    pub freemap: [AttrLeafMap; 3],
    pub pad2: u32,
}

impl Decode for AttrLeafHdr {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let info: XfsDa3Blkinfo = Decode::decode(decoder)?;
        let count = Decode::decode(decoder)?;
        let usedbytes = Decode::decode(decoder)?;
        let firstused = Decode::decode(decoder)?;
        let holes = Decode::decode(decoder)?;
        let pad1 = Decode::decode(decoder)?;
        let freemap = Decode::decode(decoder)?;
        let pad2 = Decode::decode(decoder)?;

        assert_eq!(info.magic, XFS_ATTR3_LEAF_MAGIC,
           "bad magic!  expected {:#x} but found {:#x}", XFS_ATTR3_LEAF_MAGIC, info.magic);
        Ok(Self{info, count, usedbytes, firstused, holes, pad1, freemap, pad2})
    }
}
impl_borrow_decode!(AttrLeafHdr);

#[derive(Debug, Decode)]
pub struct AttrLeafEntry {
    pub hashval: u32,
    pub nameidx: u16,
    pub flags: u8,
    pub pad2: u8,
}

#[derive(Debug)]
pub struct AttrLeafNameLocal {
    pub valuelen: u16,
    pub namelen: u8,
    pub nameval: Vec<u8>,
}

impl Decode for AttrLeafNameLocal {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let valuelen = Decode::decode(decoder)?;
        let namelen: u8 = Decode::decode(decoder)?;
        let mut nameval = vec![0u8; usize::from(namelen) + usize::from(valuelen)];
        decoder.reader().read(&mut nameval[..])?;

        Ok(Self{ valuelen, namelen, nameval})
    }
}

#[derive(Debug)]
pub enum AttrLeafName {
    Local(AttrLeafNameLocal),
    Remote(AttrLeafNameRemote)
}

impl AttrLeafName {
    fn name(&self) -> &[u8] {
        match self {
            AttrLeafName::Local(local) => &local.nameval[0..usize::from(local.namelen)],
            AttrLeafName::Remote(remote) => &remote.name[0..usize::from(remote.namelen)],
        }
    }

    fn namelen(&self) -> u8 {
        match self {
            AttrLeafName::Local(local) => local.namelen,
            AttrLeafName::Remote(remote) => remote.namelen,
        }
    }

    fn value<F, R>(
        &self,
        buf_reader: &mut R,
        map_logical_block_to_fs_block: F,
    ) -> Vec<u8>
        where R: BufRead + Reader + Seek,
              F: Fn(XfsFileoff, &mut R) -> XfsFsblock
    {
        match self {
            AttrLeafName::Local(local) => {
                local.nameval[local.namelen as usize..].to_vec()
            },
            AttrLeafName::Remote(remote) => {
                let sb = SUPERBLOCK.get().unwrap();
                let mut res: Vec<u8> = Vec::with_capacity(remote.valuelen as usize);
                let mut valueblk = remote.valueblk.into();
                let mut valuelen: i64 = remote.valuelen.into();

                while valuelen > 0 {
                    let blk_num =
                        map_logical_block_to_fs_block(valueblk, buf_reader.by_ref());
                    buf_reader.seek(SeekFrom::Start(sb.fsb_to_offset(blk_num))).unwrap();

                    let (_, data) = AttrRmtHdr::from(buf_reader.by_ref());

                    valuelen -= data.len() as i64;
                    res.extend(data);
                    valueblk += 1;
                }
                res
            }
        }
    }

    fn valuelen(&self) -> u32 {
        match self {
            AttrLeafName::Local(local) => local.valuelen.into(),
            AttrLeafName::Remote(remote) => remote.valuelen,
        }
    }
}

#[derive(Debug)]
pub struct AttrLeafblock {
    pub hdr: AttrLeafHdr,
    // TODO: in-memory, combine AttrLeafEntry and AttrLeafName into a struct, so we'll only need a
    // single Vec
    pub entries: Vec<AttrLeafEntry>,
    pub names: Vec<AttrLeafName>
}

impl AttrLeafblock {
    pub fn get_total_size(&mut self) -> u32 {
        let mut total: u32 = 0;

        for (entry, name) in std::iter::zip(self.entries.iter(), self.names.iter()) {
            total += get_namespace_size_from_flags(entry.flags) + u32::from(name.namelen()) + 1;
        }

        total
    }

    pub fn get_size(&self, hash: u32) -> Result<u32, libc::c_int> {
        // TODO: handle hash collisions
        match self.entries.binary_search_by_key(&hash, |entry| entry.hashval) {
            Ok(i) => Ok(self.names[i].valuelen()),
            Err(_) => Err(libc::ENOATTR)
        }
    }

    pub fn list(&mut self, list: &mut Vec<u8>) {
        for (entry, name_entry) in std::iter::zip(self.entries.iter(), self.names.iter()) {
            list.extend_from_slice(get_namespace_from_flags(entry.flags));
            list.extend_from_slice(name_entry.name());
            list.push(0)
        }
    }

    // TODO: return ENOENT instead of panicing.  It might be due to a hash collision one level up
    // the tree.
    pub fn get<R: BufRead + Reader + Seek, F: Fn(XfsFileoff, &mut R) -> XfsFsblock>(
        &self,
        buf_reader: &mut R,
        hash: u32,
        map_logical_block_to_fs_block: F,
    ) -> Vec<u8> {
        match self.entries.binary_search_by_key(&hash, |entry| entry.hashval) {
            Ok(i) => self.names[i].value(buf_reader, map_logical_block_to_fs_block),
            Err(_) => panic!("Couldn't find the attribute entry")
        }
    }
}

impl Decode for AttrLeafblock {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let blocksize = SUPERBLOCK.get().unwrap().sb_blocksize as usize;
        let mut raw = vec![0u8; blocksize];
        decoder.reader().read(&mut raw[..])?;

        let config = decoder.config();
        let sl = bincode::de::read::SliceReader::new(&raw);
        let mut sldecoder = bincode::de::DecoderImpl::new(sl, *config);
        let hdr: AttrLeafHdr = Decode::decode(&mut sldecoder)?;

        let mut entries = Vec::<AttrLeafEntry>::with_capacity(hdr.count.into());
        for _i in 0..entries.capacity() {
            entries.push(Decode::decode(&mut sldecoder)?);
        }

        let mut names = Vec::with_capacity(entries.len());
        for e in entries.iter() {
            let ofs = usize::from(e.nameidx);
            if e.flags & constants::XFS_ATTR_LOCAL != 0 {
                let local = bincode::decode_from_slice(&raw[ofs..], *config)?.0;
                names.push(AttrLeafName::Local(local));
            } else {
                let remote = bincode::decode_from_slice(&raw[ofs..], *config)?.0;
                names.push(AttrLeafName::Remote(remote));
            }
        }

        Ok(AttrLeafblock {hdr, entries,
            names
        })
    }
}

#[derive(Debug)]
pub struct AttrLeafNameRemote {
    pub valueblk: u32,
    pub valuelen: u32,
    pub namelen: u8,
    pub name: Vec<u8>,
}

impl Decode for AttrLeafNameRemote {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let valueblk = Decode::decode(decoder)?;
        let valuelen = Decode::decode(decoder)?;
        let namelen: u8 = Decode::decode(decoder)?;
        let mut name = vec![0u8; usize::from(namelen)];
        decoder.reader().read(&mut name[..])?;

        Ok(Self{ valueblk, valuelen, namelen, name})
    }
}

#[derive(Debug)]
pub struct AttrRmtHdr {
    pub rm_magic: u32,
    pub rm_offset: u32,
    pub rm_bytes: u32,
    pub rm_crc: u32,
    pub rm_uuid: Uuid,
    pub rm_owner: u64,
    pub rm_blkno: u64,
    pub rm_lsn: u64,
}

impl AttrRmtHdr {
    pub fn from<R: BufRead + Seek>(buf_reader: &mut R) -> (AttrRmtHdr, Vec<u8>) {
        let rm_magic = buf_reader.read_u32::<BigEndian>().unwrap();
        let rm_offset = buf_reader.read_u32::<BigEndian>().unwrap();
        let rm_bytes = buf_reader.read_u32::<BigEndian>().unwrap();
        let rm_crc = buf_reader.read_u32::<BigEndian>().unwrap();

        let rm_uuid = Uuid::from_u128(buf_reader.read_u128::<BigEndian>().unwrap());

        let rm_owner = buf_reader.read_u64::<BigEndian>().unwrap();
        let rm_blkno = buf_reader.read_u64::<BigEndian>().unwrap();
        let rm_lsn = buf_reader.read_u64::<BigEndian>().unwrap();

        let mut data = vec![0; rm_bytes as usize];
        buf_reader.read_exact(&mut data).unwrap();

        (
            AttrRmtHdr {
                rm_magic,
                rm_offset,
                rm_bytes,
                rm_crc,
                rm_uuid,
                rm_owner,
                rm_blkno,
                rm_lsn,
            },
            data,
        )
    }
}

pub trait Attr<R: BufRead + Seek> {
    fn get_total_size(&mut self, buf_reader: &mut R, super_block: &Sb) -> u32;

    fn get_size(&self, buf_reader: &mut R, super_block: &Sb, name: &OsStr) -> Result<u32, libc::c_int>;

    fn list(&mut self, buf_reader: &mut R, super_block: &Sb) -> Vec<u8>;

    fn get(&self, buf_reader: &mut R, super_block: &Sb, name: &OsStr) -> Result<Vec<u8>, libc::c_int>;
}

/// Open an attribute block, whose type may be unknown until its contents are examined.
pub fn open<R: Reader + BufRead + Seek>(
        buf_reader: &mut R,
        superblock: &Sb,
        bmx: Vec<BmbtRec>,
    ) -> Box<dyn Attr<R>>
{
    if let Some(rec) = bmx.first() {
        let ofs = superblock.fsb_to_offset(rec.br_startblock);
        buf_reader.seek(SeekFrom::Start(ofs)).unwrap();
        let mut raw = vec![0u8; superblock.sb_blocksize as usize];
        buf_reader.read_exact(&mut raw).unwrap();
        let info: XfsDa3Blkinfo = utils::decode(&raw).unwrap().0; 

        match info.magic {
            XFS_ATTR3_LEAF_MAGIC => {
                let leaf: AttrLeafblock = utils::decode(&raw).unwrap().0;
                Box::new(AttrLeaf {
                    bmx,
                    leaf,
                    leaf_offset: ofs,
                    total_size: -1,
                })
            },
            XFS_DA3_NODE_MAGIC => {
                let node: XfsDa3Intnode = utils::decode(&raw).unwrap().0;
                Box::new(AttrNode {
                    bmx,
                    node,
                    total_size: -1
                })
            },
            magic => {
                panic!("bad magic!  expected either {:#x} or {:#x} but found {:#x}",
                       XFS_ATTR3_LEAF_MAGIC, XFS_DA3_NODE_MAGIC, magic);
            }
        }
    } else {
        panic!("Extent records missing!");
    }
}
