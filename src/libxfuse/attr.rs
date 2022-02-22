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
use std::{
    cmp::Ordering,
    convert::TryInto,
    io::{BufRead, Seek, SeekFrom},
    mem::size_of,
};

use byteorder::{BigEndian, ReadBytesExt};
use uuid::Uuid;

use super::{
    da_btree::XfsDa3Blkinfo,
    definitions::{XfsFileoff, XfsFsblock},
    sb::Sb,
};

pub const XFS_ATTR_LOCAL_BIT: u8 = 0;
pub const XFS_ATTR_ROOT_BIT: u8 = 1;
pub const XFS_ATTR_SECURE_BIT: u8 = 2;
pub const XFS_ATTR_INCOMPLETE_BIT: u8 = 7;
pub const XFS_ATTR_LOCAL: u8 = 1 << XFS_ATTR_LOCAL_BIT;
pub const XFS_ATTR_ROOT: u8 = 1 << XFS_ATTR_ROOT_BIT;
pub const XFS_ATTR_SECURE: u8 = 1 << XFS_ATTR_SECURE_BIT;
pub const XFS_ATTR_INCOMPLETE: u8 = 1 << XFS_ATTR_INCOMPLETE_BIT;
pub const XFS_ATTR_NSP_ONDISK_MASK: u8 = XFS_ATTR_ROOT | XFS_ATTR_SECURE;

pub fn get_namespace_from_flags(flags: u8) -> String {
    let namespace: String;

    if flags & XFS_ATTR_SECURE != 0 {
        namespace = String::from("secure.");
    } else if flags & XFS_ATTR_ROOT != 0 {
        namespace = String::from("trusted.");
    } else {
        namespace = String::from("user.");
    }

    namespace
}

pub fn get_namespace_size_from_flags(flags: u8) -> u32 {
    if flags & XFS_ATTR_SECURE != 0 {
        7 // secure.
    } else if flags & XFS_ATTR_ROOT != 0 {
        8 // trusted.
    } else {
        5 // user.
    }
}

#[derive(Debug)]
pub struct AttrLeafMap {
    pub base: u16,
    pub size: u16,
}

impl AttrLeafMap {
    pub fn from<R: BufRead>(buf_reader: &mut R) -> AttrLeafMap {
        let base = buf_reader.read_u16::<BigEndian>().unwrap();
        let size = buf_reader.read_u16::<BigEndian>().unwrap();

        AttrLeafMap { base, size }
    }
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

impl AttrLeafHdr {
    pub fn from<R: BufRead + Seek>(buf_reader: &mut R, super_block: &Sb) -> AttrLeafHdr {
        let info = XfsDa3Blkinfo::from(buf_reader.by_ref(), super_block);
        let count = buf_reader.read_u16::<BigEndian>().unwrap();
        let usedbytes = buf_reader.read_u16::<BigEndian>().unwrap();
        let firstused = buf_reader.read_u16::<BigEndian>().unwrap();
        let holes = buf_reader.read_u8().unwrap();
        let pad1 = buf_reader.read_u8().unwrap();

        let freemap: [AttrLeafMap; 3] = [
            AttrLeafMap::from(buf_reader.by_ref()),
            AttrLeafMap::from(buf_reader.by_ref()),
            AttrLeafMap::from(buf_reader.by_ref()),
        ];

        let pad2 = buf_reader.read_u32::<BigEndian>().unwrap();

        AttrLeafHdr {
            info,
            count,
            usedbytes,
            firstused,
            holes,
            pad1,
            freemap,
            pad2,
        }
    }
}

#[derive(Debug)]
pub struct AttrLeafEntry {
    pub hashval: u32,
    pub nameidx: u16,
    pub flags: u8,
    pub pad2: u8,
}

impl AttrLeafEntry {
    pub fn from<R: BufRead>(buf_reader: &mut R) -> AttrLeafEntry {
        let hashval = buf_reader.read_u32::<BigEndian>().unwrap();
        let nameidx = buf_reader.read_u16::<BigEndian>().unwrap();
        let flags = buf_reader.read_u8().unwrap();
        let pad2 = buf_reader.read_u8().unwrap();

        AttrLeafEntry {
            hashval,
            nameidx,
            flags,
            pad2,
        }
    }
}

#[derive(Debug)]
pub struct AttrLeafNameLocal {
    pub valuelen: u16,
    pub namelen: u8,
    pub nameval: Vec<u8>,
}

impl AttrLeafNameLocal {
    pub fn from<R: BufRead>(buf_reader: &mut R) -> AttrLeafNameLocal {
        let valuelen = buf_reader.read_u16::<BigEndian>().unwrap();
        let namelen = buf_reader.read_u8().unwrap();

        let mut nameval = Vec::<u8>::new();
        for _i in 0..((namelen as u16) + valuelen) {
            nameval.push(buf_reader.read_u8().unwrap());
        }

        AttrLeafNameLocal {
            valuelen,
            namelen,
            nameval,
        }
    }
}

#[derive(Debug)]
pub struct AttrLeafblock {
    pub hdr: AttrLeafHdr,
    pub entries: Vec<AttrLeafEntry>,
    // pub namelist: AttrLeafNameLocal,
    // pub valuelist: AttrLeafNameRemote,
}

impl AttrLeafblock {
    pub fn from<R: BufRead + Seek>(buf_reader: &mut R, super_block: &Sb) -> AttrLeafblock {
        let hdr = AttrLeafHdr::from(buf_reader.by_ref(), super_block);

        let mut entries = Vec::<AttrLeafEntry>::with_capacity(hdr.count.into());
        for _i in 0..entries.capacity() {
            entries.push(AttrLeafEntry::from(buf_reader.by_ref()));
        }

        AttrLeafblock { hdr, entries }
    }

    pub fn get_total_size<R: BufRead + Seek>(
        &mut self,
        buf_reader: &mut R,
        leaf_offset: u64,
    ) -> u32 {
        let mut total_size: u32 = 0;

        for entry in self.entries.iter() {
            buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();
            buf_reader
                .seek(SeekFrom::Current(i64::from(entry.nameidx)))
                .unwrap();

            if entry.flags & XFS_ATTR_LOCAL == 0 {
                let name_entry = AttrLeafNameLocal::from(buf_reader.by_ref());
                total_size +=
                    get_namespace_size_from_flags(entry.flags) + u32::from(name_entry.namelen) + 1;
            } else {
                let name_entry = AttrLeafNameRemote::from(buf_reader.by_ref());
                total_size +=
                    get_namespace_size_from_flags(entry.flags) + u32::from(name_entry.namelen) + 1;
            }
        }

        total_size
    }

    pub fn get_size<R: BufRead + Seek>(
        &self,
        buf_reader: &mut R,
        hash: u32,
        leaf_offset: u64,
    ) -> u32 {
        let mut low: u32 = 0;
        let mut high: u32 = self.hdr.count.into();

        while low <= high {
            let mid = low + ((high - low) / 2);

            buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();
            buf_reader
                .seek(SeekFrom::Current(
                    size_of::<AttrLeafHdr>().try_into().unwrap(),
                ))
                .unwrap();
            buf_reader
                .seek(SeekFrom::Current(
                    i64::from(mid) * (size_of::<AttrLeafEntry>() as i64),
                ))
                .unwrap();

            let entry = AttrLeafEntry::from(buf_reader.by_ref());

            match entry.hashval.cmp(&hash) {
                Ordering::Greater => {
                    high = mid - 1;
                }
                Ordering::Less => {
                    low = mid + 1;
                }
                Ordering::Equal => {
                    buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();
                    buf_reader
                        .seek(SeekFrom::Current(i64::from(entry.nameidx)))
                        .unwrap();

                    if entry.flags & XFS_ATTR_LOCAL == 0 {
                        let name_entry = AttrLeafNameLocal::from(buf_reader.by_ref());
                        return name_entry.valuelen.into();
                    } else {
                        let name_entry = AttrLeafNameRemote::from(buf_reader.by_ref());
                        return name_entry.valuelen;
                    }
                }
            }
        }

        panic!("Couldn't find the attribute entry");
    }

    pub fn list<R: BufRead + Seek>(
        &mut self,
        buf_reader: &mut R,
        list: &mut Vec<u8>,
        leaf_offset: u64,
    ) {
        for entry in self.entries.iter() {
            buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();
            buf_reader
                .seek(SeekFrom::Current(i64::from(entry.nameidx)))
                .unwrap();

            if entry.flags & XFS_ATTR_LOCAL == 0 {
                let name_entry = AttrLeafNameLocal::from(buf_reader.by_ref());

                list.extend_from_slice(get_namespace_from_flags(entry.flags).as_bytes());
                let namelen = name_entry.namelen as usize;
                list.extend_from_slice(&name_entry.nameval[0..namelen]);
            } else {
                let name_entry = AttrLeafNameRemote::from(buf_reader.by_ref());

                list.extend_from_slice(get_namespace_from_flags(entry.flags).as_bytes());
                let namelen = name_entry.namelen as usize;
                list.extend_from_slice(&name_entry.name[0..namelen]);
            }

            list.push(0)
        }
    }

    pub fn get<R: BufRead + Seek, F: Fn(XfsFileoff, &mut R) -> XfsFsblock>(
        &self,
        buf_reader: &mut R,
        super_block: &Sb,
        hash: u32,
        leaf_offset: u64,
        map_logical_block_to_fs_block: F,
    ) -> Vec<u8> {
        let mut low: u32 = 0;
        let mut high: u32 = self.hdr.count.into();

        while low <= high {
            let mid = ((high - low) / 2) + low;

            buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();
            buf_reader
                .seek(SeekFrom::Current(
                    size_of::<AttrLeafHdr>().try_into().unwrap(),
                ))
                .unwrap();
            buf_reader
                .seek(SeekFrom::Current(
                    i64::from(mid) * (size_of::<AttrLeafEntry>() as i64),
                ))
                .unwrap();

            let entry = AttrLeafEntry::from(buf_reader.by_ref());

            match entry.hashval.cmp(&hash) {
                Ordering::Greater => {
                    high = mid - 1;
                }
                Ordering::Less => {
                    low = mid + 1;
                }
                Ordering::Equal => {
                    buf_reader.seek(SeekFrom::Start(leaf_offset)).unwrap();
                    buf_reader
                        .seek(SeekFrom::Current(i64::from(entry.nameidx)))
                        .unwrap();

                    if entry.flags & XFS_ATTR_LOCAL == 0 {
                        let name_entry = AttrLeafNameLocal::from(buf_reader.by_ref());

                        let namelen = name_entry.namelen as usize;

                        return name_entry.nameval[namelen..].to_vec();
                    } else {
                        let name_entry = AttrLeafNameRemote::from(buf_reader.by_ref());

                        let mut valuelen: i64 = name_entry.valuelen.into();
                        let mut valueblk = name_entry.valueblk;

                        let mut res: Vec<u8> = Vec::with_capacity(valuelen as usize);

                        while valueblk > 0 {
                            let blk_num =
                                map_logical_block_to_fs_block(valueblk.into(), buf_reader.by_ref());
                            buf_reader
                                .seek(SeekFrom::Start(
                                    blk_num * u64::from(super_block.sb_blocksize),
                                ))
                                .unwrap();

                            let (_, data) = AttrRmtHdr::from(buf_reader.by_ref());

                            valuelen -= data.len() as i64;
                            res.extend(data);
                            valueblk += 1;
                        }
                    }
                }
            }
        }

        panic!("Couldn't find the attribute entry");
    }
}

#[derive(Debug)]
pub struct AttrLeafNameRemote {
    pub valueblk: u32,
    pub valuelen: u32,
    pub namelen: u8,
    pub name: Vec<u8>,
}

impl AttrLeafNameRemote {
    pub fn from<R: BufRead>(buf_reader: &mut R) -> AttrLeafNameRemote {
        let valueblk = buf_reader.read_u32::<BigEndian>().unwrap();
        let valuelen = buf_reader.read_u32::<BigEndian>().unwrap();
        let namelen = buf_reader.read_u8().unwrap();

        let mut name = Vec::<u8>::new();
        for _i in 0..namelen {
            name.push(buf_reader.read_u8().unwrap());
        }

        AttrLeafNameRemote {
            valueblk,
            valuelen,
            namelen,
            name,
        }
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
        let start_offset = buf_reader.stream_position().unwrap();

        let rm_magic = buf_reader.read_u32::<BigEndian>().unwrap();
        let rm_offset = buf_reader.read_u32::<BigEndian>().unwrap();
        let rm_bytes = buf_reader.read_u32::<BigEndian>().unwrap();
        let rm_crc = buf_reader.read_u32::<BigEndian>().unwrap();

        let rm_uuid = Uuid::from_u128(buf_reader.read_u128::<BigEndian>().unwrap());

        let rm_owner = buf_reader.read_u64::<BigEndian>().unwrap();
        let rm_blkno = buf_reader.read_u64::<BigEndian>().unwrap();
        let rm_lsn = buf_reader.read_u64::<BigEndian>().unwrap();

        buf_reader
            .seek(SeekFrom::Start(start_offset + u64::from(rm_offset)))
            .unwrap();

        let mut data = Vec::<u8>::with_capacity(rm_bytes as usize);
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

    fn get_size(&self, buf_reader: &mut R, super_block: &Sb, name: &str) -> u32;

    fn list(&mut self, buf_reader: &mut R, super_block: &Sb) -> Vec<u8>;

    fn get(&self, buf_reader: &mut R, super_block: &Sb, name: &str) -> Vec<u8>;
}
