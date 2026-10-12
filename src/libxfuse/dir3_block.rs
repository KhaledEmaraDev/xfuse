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
    convert::TryInto,
    ffi::{OsStr, OsString},
    io::{BufRead, Seek, SeekFrom},
    os::unix::ffi::OsStrExt,
};

use bincode_next::{de::read::Reader, Decode};
use fuser::FileType;
use libc::{c_int, ENOENT};

use super::{
    btree::crc32c_without_its_own_field,
    da_btree::hashname,
    definitions::{XfsDahash, XFS_DIR2_BLOCK_MAGIC, XFS_DIR3_BLOCK_MAGIC, *},
    dir3::{
        Dir2DataEntry, Dir2DataHdr, Dir2DataUnused, Dir2LeafEntry, Dir3, Dir3DataHdr,
        XfsDir2Dataptr,
    },
    sb::Sb,
    utils::{decode, get_file_type, FileKind},
};

#[derive(Debug, Decode)]
pub struct Dir2BlockTail {
    pub count: u32,
    pub stale: u32,
}

impl Dir2BlockTail {
    /// On-disk size in bytes
    pub const SIZE: usize = 8;
}

#[derive(Debug)]
pub struct Dir2BlockDisk {
    pub leaf: Vec<Dir2LeafEntry>,
    tail: Dir2BlockTail,
    raw: Vec<u8>,
    /// Start of directory entries within the directory block
    data_offset: usize,
}

impl Dir2BlockDisk {
    pub fn new<T>(buf_reader: &mut T, offset: u64, size: u32) -> Dir2BlockDisk
    where
        T: BufRead + Seek,
    {
        buf_reader.seek(SeekFrom::Start(offset)).unwrap();
        let mut raw = vec![0u8; size as usize];
        buf_reader.read_exact(&mut raw).unwrap();

        let magic: u32 = decode(&raw[..]).unwrap().0;
        let data_offset = match magic {
            XFS_DIR2_BLOCK_MAGIC => {
                let hdr: Dir2DataHdr = decode(&raw[..]).unwrap().0;
                assert_eq!(hdr.magic, XFS_DIR2_BLOCK_MAGIC);
                Dir2DataHdr::SIZE as usize
            }
            XFS_DIR3_BLOCK_MAGIC => {
                let hdr: Dir3DataHdr = decode(&raw[..]).unwrap().0;
                assert_eq!(hdr.hdr.magic, XFS_DIR3_BLOCK_MAGIC);
                Dir3DataHdr::SIZE as usize
            }
            _ => panic!("Unknown magic number for block directory {magic:#x}"),
        };

        let tail_offset = (size as usize) - Dir2BlockTail::SIZE;
        let tail: Dir2BlockTail = decode(&raw[tail_offset..]).unwrap().0;

        let mut leaf_offset = tail_offset - Dir2LeafEntry::SIZE * tail.count as usize;

        let mut leaf = Vec::with_capacity(tail.count as usize);
        for _i in 0..tail.count {
            leaf.push(decode(&raw[leaf_offset..]).unwrap().0);
            leaf_offset += Dir2LeafEntry::SIZE;
        }

        Dir2BlockDisk {
            leaf,
            tail,
            raw,
            data_offset,
        }
    }

    /// get the length of the raw data region
    fn get_data_len(&self, directory_block_size: u32) -> u64 {
        directory_block_size as u64
            - Dir2BlockTail::SIZE as u64
            - Dir2LeafEntry::SIZE as u64 * (self.tail.count as u64)
    }
}

#[derive(Debug)]
pub struct Dir2Block {
    ents: Vec<Dir2LeafEntry>,
    raw: Box<[u8]>,
    /// Start of directory entries within the directory block
    data_offset: usize,
    /// Tail of the block directory
    tail: Dir2BlockTail,
}

impl Dir2Block {
    pub fn new<T: BufRead + Seek>(
        buf_reader: &mut T,
        superblock: &Sb,
        start_block: XfsFsblock,
    ) -> Dir2Block {
        let offset = superblock.fsb_to_offset(start_block);
        let dir_blk_size = superblock.sb_blocksize << superblock.sb_dirblklog;

        let dir_disk = Dir2BlockDisk::new(buf_reader.by_ref(), offset, dir_blk_size);

        let data_len = dir_disk.get_data_len(dir_blk_size);
        assert!(data_len as usize <= dir_disk.raw.len());
        let mut raw = dir_disk.raw;
        raw.truncate(data_len as usize);

        Dir2Block {
            raw: raw.into(),
            ents: dir_disk.leaf,
            data_offset: dir_disk.data_offset,
            tail: dir_disk.tail,
        }
    }

    fn get_addresses(&self, hash: XfsDahash) -> impl Iterator<Item = usize> + '_ {
        let i = self.ents.partition_point(|ent| ent.hashval < hash);
        let l = self.ents.len();
        let j = (i..l).find(|x| self.ents[*x].hashval > hash).unwrap_or(l);
        self.ents[i..j]
            .iter()
            .map(|ent| (ent.address << 3) as usize)
    }
}

impl Dir2Block {
    /// Add a directory entry to a block directory.
    /// Returns the offset (tag) of the new entry.
    #[allow(dead_code)]
    pub fn add_dirent(
        &mut self,
        sb: &Sb,
        name: &OsStr,
        ftype: u8,
        inumber: XfsIno,
    ) -> Result<u16, c_int> {
        // Check if entry already exists
        if self.lookup_raw(name).is_ok() {
            return Err(libc::EEXIST);
        }

        let name_bytes = name.as_bytes();
        let namelen = name_bytes.len() as u8;
        let has_ftype = sb.has_ftype();

        // Calculate entry length (aligned to 8 bytes)
        let entry_len = if has_ftype {
            ((namelen as usize + 19).div_ceil(8)) * 8
        } else {
            ((namelen as usize + 18).div_ceil(8)) * 8
        };

        // Find free space in data region
        let free_offset = self.find_free_space(entry_len)?;

        // Write the new entry at free_offset
        let tag = free_offset as u16;
        self.write_dirent(free_offset, inumber, name_bytes, ftype, tag, has_ftype, sb)?;

        // Add leaf entry
        let hash = hashname(name);
        let address = (free_offset >> 3) as u32; // address is in 8-byte units
        self.add_leaf_entry(hash, address)?;

        // Update tail count
        self.tail.count += 1;
        self.write_tail()?;

        // For v3, update checksum
        if self.is_v3() {
            self.update_checksum(sb)?;
        }

        Ok(tag)
    }

    /// Remove a directory entry from a block directory.
    #[allow(dead_code)]
    pub fn remove_dirent(&mut self, sb: &Sb, name: &OsStr) -> Result<(), c_int> {
        // Find the entry
        let (offset, entry) = self.find_entry(name)?;
        if entry.name != name {
            return Err(libc::ENOENT);
        }

        // Mark as unused
        self.mark_unused(offset, entry, sb)?;

        // Remove leaf entry
        let hash = hashname(name);
        self.remove_leaf_entry(hash)?;

        // Update tail count
        self.tail.count = self.tail.count.saturating_sub(1);
        self.write_tail()?;

        // For v3, update checksum
        if self.is_v3() {
            self.update_checksum(sb)?;
        }

        Ok(())
    }

    /// Check if this is a v3 (CRC) block directory
    fn is_v3(&self) -> bool {
        if self.raw.len() < 4 {
            return false;
        }
        let magic = u32::from_be_bytes([self.raw[0], self.raw[1], self.raw[2], self.raw[3]]);
        magic == XFS_DIR3_BLOCK_MAGIC
    }

    /// Lookup an entry by name and return its raw offset
    fn lookup_raw(&self, name: &OsStr) -> Result<usize, c_int> {
        let hash = hashname(name);
        for offset in self.get_addresses(hash) {
            if offset < self.raw.len() {
                let entry: Dir2DataEntry = decode(&self.raw[offset..]).unwrap().0;
                if entry.name == name {
                    return Ok(offset);
                }
            }
        }
        Err(libc::ENOENT)
    }

    /// Find an entry by name, returning its offset and decoded entry
    #[allow(dead_code)]
    fn find_entry(&self, name: &OsStr) -> Result<(usize, Dir2DataEntry), c_int> {
        let offset = self.lookup_raw(name)?;
        let entry: Dir2DataEntry = decode(&self.raw[offset..]).unwrap().0;
        Ok((offset, entry))
    }

    /// Find free space of at least `min_len` bytes in the data region
    #[allow(dead_code, clippy::unnecessary_cast)]
    fn find_free_space(&self, min_len: usize) -> Result<usize, c_int> {
        let mut offset = self.data_offset;
        while offset + min_len <= self.raw.len() {
            let freetag: u16 = decode(&self.raw[offset..]).unwrap().0;
            if freetag == 0xffff {
                let (_, length) = decode::<Dir2DataUnused>(&self.raw[offset..]).unwrap();
                if (length as usize) >= min_len {
                    return Ok(offset);
                }
                let len_usize = length as usize;
                offset += len_usize;
            } else {
                let length = Dir2DataEntry::get_length_static(&self.raw[offset..]);
                let len_usize = length as usize;
                offset += len_usize;
            }
        }
        Err(libc::ENOSPC)
    }

    /// Write a directory entry at the given offset
    #[allow(dead_code, clippy::too_many_arguments)]
    fn write_dirent(
        &mut self,
        offset: usize,
        inumber: XfsIno,
        name_bytes: &[u8],
        ftype: u8,
        tag: u16,
        has_ftype: bool,
        _sb: &Sb,
    ) -> Result<(), c_int> {
        let namelen = name_bytes.len() as u8;

        // Calculate entry length
        let entry_len = if has_ftype {
            ((namelen as usize + 19).div_ceil(8)) * 8
        } else {
            ((namelen as usize + 18).div_ceil(8)) * 8
        };

        // Ensure we have enough space
        if offset + entry_len > self.raw.len() {
            return Err(libc::ENOSPC);
        }

        // Encode the entry
        let mut buf = Vec::with_capacity(entry_len);
        buf.extend_from_slice(&inumber.to_be_bytes());
        buf.push(namelen);
        buf.extend_from_slice(name_bytes);
        if has_ftype {
            buf.push(ftype);
        }

        // Pad to 8-byte boundary before tag
        let current_len = buf.len();
        let pad = (8 - (current_len % 8)) % 8;
        buf.extend(vec![0u8; pad]);

        // Write tag
        buf.extend_from_slice(&tag.to_be_bytes());

        // Write to raw
        self.raw[offset..offset + entry_len].copy_from_slice(&buf);
        Ok(())
    }

    /// Add a leaf entry to the leaf array
    #[allow(dead_code)]
    fn add_leaf_entry(&mut self, hash: XfsDahash, address: XfsDir2Dataptr) -> Result<(), c_int> {
        // The leaf array is at the end of the block before the tail
        let leaf_start =
            self.raw.len() - Dir2BlockTail::SIZE - self.tail.count as usize * Dir2LeafEntry::SIZE;

        // Find insertion point (leaf is sorted by hashval)
        let mut insert_idx = 0;
        for i in 0..self.tail.count as usize {
            let leaf_offset = leaf_start + i * Dir2LeafEntry::SIZE;
            let entry: Dir2LeafEntry = decode(&self.raw[leaf_offset..]).unwrap().0;
            if entry.hashval >= hash {
                insert_idx = i;
                break;
            }
            insert_idx = i + 1;
        }

        // Make space by shifting later entries and the tail
        let tail_offset = self.raw.len() - Dir2BlockTail::SIZE;

        // Shift leaf entries and tail - copy data to temp buffer first to avoid borrow issues
        for i in (insert_idx..self.tail.count as usize).rev() {
            let src = leaf_start + i * Dir2LeafEntry::SIZE;
            let dst = src + Dir2LeafEntry::SIZE;
            let mut tmp = [0u8; Dir2LeafEntry::SIZE];
            tmp.copy_from_slice(&self.raw[src..src + Dir2LeafEntry::SIZE]);
            self.raw[dst..dst + Dir2LeafEntry::SIZE].copy_from_slice(&tmp);
        }

        // Also shift the tail
        let tail_src = tail_offset;
        let tail_dst = tail_src + Dir2LeafEntry::SIZE;
        let mut tail_tmp = [0u8; Dir2BlockTail::SIZE];
        tail_tmp.copy_from_slice(&self.raw[tail_src..tail_src + Dir2BlockTail::SIZE]);
        self.raw[tail_dst..tail_dst + Dir2BlockTail::SIZE].copy_from_slice(&tail_tmp);

        // Write new leaf entry
        let new_leaf_offset = leaf_start + insert_idx * Dir2LeafEntry::SIZE;
        let new_leaf = Dir2LeafEntry {
            hashval: hash,
            address,
        };
        let mut buf = Vec::with_capacity(Dir2LeafEntry::SIZE);
        buf.extend_from_slice(&new_leaf.hashval.to_be_bytes());
        buf.extend_from_slice(&new_leaf.address.to_be_bytes());
        self.raw[new_leaf_offset..new_leaf_offset + Dir2LeafEntry::SIZE].copy_from_slice(&buf);

        Ok(())
    }

    /// Remove a leaf entry by hash
    #[allow(dead_code)]
    fn remove_leaf_entry(&mut self, hash: XfsDahash) -> Result<(), c_int> {
        let leaf_start =
            self.raw.len() - Dir2BlockTail::SIZE - self.tail.count as usize * Dir2LeafEntry::SIZE;

        // Find the leaf entry with matching hash
        for i in 0..self.tail.count as usize {
            let leaf_offset = leaf_start + i * Dir2LeafEntry::SIZE;
            let entry: Dir2LeafEntry = decode(&self.raw[leaf_offset..]).unwrap().0;
            if entry.hashval == hash {
                // Shift later entries forward - copy data to temp buffer first to avoid borrow issues
                for j in i..self.tail.count as usize - 1 {
                    let src = leaf_start + (j + 1) * Dir2LeafEntry::SIZE;
                    let dst = leaf_start + j * Dir2LeafEntry::SIZE;
                    let mut tmp = [0u8; Dir2LeafEntry::SIZE];
                    tmp.copy_from_slice(&self.raw[src..src + Dir2LeafEntry::SIZE]);
                    self.raw[dst..dst + Dir2LeafEntry::SIZE].copy_from_slice(&tmp);
                }
                return Ok(());
            }
        }
        Err(libc::ENOENT)
    }

    /// Mark a data entry as unused
    #[allow(dead_code)]
    fn mark_unused(&mut self, offset: usize, entry: Dir2DataEntry, sb: &Sb) -> Result<(), c_int> {
        let entry_len = Dir2DataEntry::get_length(sb, &self.raw[offset..]);
        let unused = Dir2DataUnused {
            freetag: 0xffff,
            length: entry_len as u16,
            tag: entry.tag,
        };
        let mut buf = Vec::with_capacity(entry_len as usize);
        buf.extend_from_slice(&unused.freetag.to_be_bytes());
        buf.extend_from_slice(&unused.length.to_be_bytes());
        buf.extend(vec![0u8; entry_len as usize - 6]);
        buf.extend_from_slice(&unused.tag.to_be_bytes());
        self.raw[offset..offset + entry_len as usize].copy_from_slice(&buf);
        Ok(())
    }

    /// Write the tail to the block
    #[allow(dead_code)]
    fn write_tail(&mut self) -> Result<(), c_int> {
        let tail_offset = self.raw.len() - Dir2BlockTail::SIZE;
        let mut buf = Vec::with_capacity(Dir2BlockTail::SIZE);
        buf.extend_from_slice(&self.tail.count.to_be_bytes());
        buf.extend_from_slice(&self.tail.stale.to_be_bytes());
        self.raw[tail_offset..tail_offset + Dir2BlockTail::SIZE].copy_from_slice(&buf);
        Ok(())
    }

    /// Update the CRC32 checksum for v3 block directories
    #[allow(dead_code)]
    fn update_checksum(&mut self, _sb: &Sb) -> Result<(), c_int> {
        if !self.is_v3() {
            return Ok(());
        }
        // The CRC is in the Dir3BlkHdr at offset 4 (after the 4-byte magic).
        // CRC32C is stored as little-endian 4 bytes.
        let crc = crc32c_without_its_own_field(&self.raw, 4);
        self.raw[4..8].copy_from_slice(&crc.to_le_bytes());
        Ok(())
    }
}

impl Dir3 for Dir2Block {
    fn lookup<R: Reader + BufRead + Seek>(
        &self,
        _buf_reader: &mut R,
        _sb: &Sb,
        name: &OsStr,
    ) -> Result<u64, c_int> {
        let hash = hashname(name);

        for offset in self.get_addresses(hash) {
            assert!(offset < self.raw.len());
            let entry: Dir2DataEntry = decode(&self.raw[offset..]).unwrap().0;
            if entry.name == name {
                return Ok(entry.inumber);
            }
        }
        Err(libc::ENOENT)
    }

    /// Read the next dirent from a Directory
    fn next<R: Reader + BufRead + Seek>(
        &self,
        _buf_reader: &mut R,
        sb: &Sb,
        offset: i64,
    ) -> Result<(XfsIno, i64, Option<FileType>, OsString), c_int> {
        let mut offset: usize = offset.try_into().unwrap();
        assert!(offset < self.raw.len());
        let mut next = offset == 0;

        if offset == 0 {
            offset += self.data_offset;
        }

        while offset < self.raw.len() {
            let freetag: u16 = decode(&self.raw[offset..]).unwrap().0;
            if freetag == 0xffff {
                let (_, length) = decode::<Dir2DataUnused>(&self.raw[offset..]).unwrap();
                offset += length;
            } else if !next {
                let length = Dir2DataEntry::get_length(sb, &self.raw[offset..]);
                offset += length as usize;
                next = true;
            } else {
                let (entry, _l) = decode::<Dir2DataEntry>(&self.raw[offset..]).unwrap();
                let kind = match entry.ftype {
                    Some(ftype) => Some(get_file_type(FileKind::Type(ftype))?),
                    None => None,
                };
                let name = entry.name;
                let entry_offset = entry.tag as u64;
                return Ok((entry.inumber, entry_offset as i64, kind, name));
            }
        }
        Err(ENOENT)
    }
}
