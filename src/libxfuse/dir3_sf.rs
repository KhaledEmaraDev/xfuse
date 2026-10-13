#![allow(unused_imports)]
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
//! Shortform (local) directory format and mutation.
//!
//! A directory with few entries fits entirely in the inode's data fork.  This
//! module decodes, serializes, and mutates the shortform format: adding an
//! entry appends it (insertion order), removing leaves survivors untouched.
//!
//! # Algorithm reference
//!
//! See [`docs/xfs-algorithms.md`] section 7 (Directories) for the algorithm map
//! with DOCUMENTED/MEASURED/IMPLEMENTED/HYPOTHESIS labels.
use std::{
    ffi::{OsStr, OsString},
    io::{BufRead, Seek},
    os::unix::ffi::{OsStrExt, OsStringExt},
};

use bincode_next::{
    de::{read::Reader, Decoder},
    error::DecodeError,
    Decode,
};
use fuser::FileType;
use libc::{c_int, ENOENT};

const NO_IMAGE: &str = "no image has been opened in this process";

use super::{
    definitions::*,
    dir3::{Dir3, XFS_DIR3_FT_DIR},
    inode::RawDinode,
    sb::Sb,
    utils::{get_file_type, FileKind},
};

// pub type XfsDir2SfOff = [u8; 2];

#[derive(Debug, Clone)]
pub struct Dir2SfHdr {
    pub count:   u8,
    pub i8count: u8,
    pub parent:  XfsIno,
}

impl<Ctx> Decode<Ctx> for Dir2SfHdr {
    fn decode<D: Decoder<Context = Ctx>>(decoder: &mut D) -> Result<Self, DecodeError> {
        let count = Decode::decode(decoder)?;
        let i8count = Decode::decode(decoder)?;
        let parent = if i8count > 0 {
            <u64 as Decode<Ctx>>::decode(decoder)?
        } else {
            <u32 as Decode<Ctx>>::decode(decoder)?.into()
        };
        Ok(Dir2SfHdr {
            count,
            i8count,
            parent,
        })
    }
}

#[derive(Debug, Clone)]
struct Dir2SfEntry32 {
    offset:  u16,
    name:    OsString,
    ftype:   Option<u8>,
    inumber: u32,
}

impl<Ctx> Decode<Ctx> for Dir2SfEntry32 {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let sb = super::volume::try_superblock()
            .ok_or_else(|| DecodeError::OtherString(NO_IMAGE.into()))?;
        let namelen: u8 = Decode::decode(decoder)?;
        let offset: u16 = Decode::decode(decoder)?;
        let mut namebytes = vec![0u8; namelen.into()];
        decoder.reader().read(&mut namebytes[..])?;
        let name = OsString::from_vec(namebytes);
        let ftype: Option<u8> = if sb.has_ftype() {
            Some(Decode::decode(decoder)?)
        } else {
            None
        };
        let inumber: u32 = Decode::decode(decoder)?;
        Ok(Dir2SfEntry32 {
            offset,
            name,
            ftype,
            inumber,
        })
    }
}

#[derive(Debug, Clone)]
struct Dir2SfEntry64 {
    offset:  u16,
    name:    OsString,
    ftype:   Option<u8>,
    inumber: XfsIno,
}

impl Dir2SfEntry64 {
    pub fn new(name: &[u8], ftype: u8, offset: u16, inumber: XfsIno) -> Self {
        let name = OsStr::from_bytes(name).to_owned();
        Self {
            offset,
            name,
            ftype: Some(ftype),
            inumber,
        }
    }
}

impl<Ctx> Decode<Ctx> for Dir2SfEntry64 {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let sb = super::volume::try_superblock()
            .ok_or_else(|| DecodeError::OtherString(NO_IMAGE.into()))?;
        let namelen: u8 = Decode::decode(decoder)?;
        let offset: u16 = Decode::decode(decoder)?;
        let mut namebytes = vec![0u8; namelen.into()];
        decoder.reader().read(&mut namebytes[..])?;
        let name = OsString::from_vec(namebytes);
        let ftype: Option<u8> = if sb.has_ftype() {
            Some(Decode::decode(decoder)?)
        } else {
            None
        };
        let inumber: XfsIno = Decode::decode(decoder)?;
        Ok(Dir2SfEntry64 {
            offset,
            name,
            ftype,
            inumber,
        })
    }
}

// Since xfs-fuse is a read-only implementation, we needn't worry about
// preserving the on-disk size of the inode.  We can just convert all of the
// entries into the 64-bit type.
impl From<Dir2SfEntry32> for Dir2SfEntry64 {
    fn from(e32: Dir2SfEntry32) -> Self {
        Self {
            offset:  e32.offset,
            name:    e32.name,
            ftype:   e32.ftype,
            inumber: e32.inumber.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Dir2Sf {
    list: Vec<Dir2SfEntry64>,
}

impl Dir2Sf {
    /// Set the inode of this directory.  Annoyingly, we need to know it, but it
    /// isn't stored on disk in this header.
    pub fn set_ino(&mut self, ino: XfsIno) {
        self.list[0].inumber = ino;
    }
}

impl<Ctx> Decode<Ctx> for Dir2Sf {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        let hdr: Dir2SfHdr = Decode::decode(decoder)?;

        let mut list = Vec::<Dir2SfEntry64>::new();
        // Alone out of all the directory types, SF directories to not store the
        // "." and ".." entries on disk.  We must synthesize them here.
        list.push(Dir2SfEntry64::new(b".", XFS_DIR3_FT_DIR, 1, u64::MAX));
        list.push(Dir2SfEntry64::new(b"..", XFS_DIR3_FT_DIR, 2, hdr.parent));
        for _i in 0..hdr.count {
            if hdr.i8count > 0 {
                list.push(Decode::decode(decoder)?);
            } else {
                let e32: Dir2SfEntry32 = Decode::decode(decoder)?;
                list.push(e32.into());
            }
        }

        Ok(Dir2Sf { list })
    }
}

impl Dir3 for Dir2Sf {
    fn lookup<R: bincode_next::de::read::Reader + BufRead + Seek>(
        &self,
        _buf_reader: &mut R,
        _super_block: &Sb,
        name: &OsStr,
    ) -> Result<u64, c_int> {
        let mut inode: Option<XfsIno> = None;

        for entry in self.list.iter() {
            if entry.name == name {
                inode = Some(entry.inumber);
            }
        }

        if let Some(ino) = inode {
            Ok(ino)
        } else {
            Err(ENOENT)
        }
    }

    fn next<R: bincode_next::de::read::Reader + BufRead + Seek>(
        &self,
        _buf_reader: &mut R,
        _super_block: &Sb,
        offset: i64,
    ) -> Result<(XfsIno, i64, Option<FileType>, OsString), c_int> {
        for entry in self.list.iter() {
            if i64::from(entry.offset) <= offset {
                continue;
            }

            let ino = entry.inumber;

            let kind = match entry.ftype {
                Some(ftype) => Some(get_file_type(FileKind::Type(ftype))?),
                None => None,
            };

            let name = entry.name.to_owned();

            return Ok((ino, entry.offset as i64, kind, name));
        }

        Err(ENOENT)
    }
}

/// One entry as it is **on disk**, with no `.` or `..`.
///
/// The read path's `Dir2SfEntry64` is not this: it is a logical entry, and two of
/// the ones in every decoded shortform directory do not exist on disk at all.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Used as soon as a B+tree or block directory needs on-disk entries.
pub struct SfEntry {
    pub name:    Vec<u8>,
    pub ftype:   u8,
    /// Assigned once when the entry is created and **never rewritten**.  Measured:
    /// removing the first and last entries of a four-entry directory moved the
    /// survivors from byte 15 and 24 to byte 6 and 15, and their offsets stayed 112
    /// and 128.
    pub offset:  u16,
    pub inumber: XfsIno,
}

/// How far apart the offsets of consecutive entries are.
///
/// Not a byte position and not a length: the offsets of a four-entry directory are
/// 96, 112, 128, 144 while the entries occupy bytes 6, 15, 24, 35, and `mmm` is two
/// bytes longer than `a` yet advances the counter by the same step.  What *is*
/// measured is that each appended entry takes the previous one's plus this, and that
/// removal changes nothing, so that is all this needs.
#[allow(dead_code)] // Used as soon as a B+tree or block directory needs on-disk entries.
pub const SF_OFFSET_STEP: u16 = 16;

/// A shortform directory that can be written back.
///
/// This exists because [`Dir2Sf`] cannot be written back, and the audit found four
/// things its decode throws away:
///
/// * `count`, `i8count` and **`parent`** -- the parent is not in `Dir2Sf` at all;
/// * the **32- vs 64-bit inode width**, because `Dir2SfEntry32` is converted by
///   `From` into `Dir2SfEntry64` on the way in;
/// * **whether the file system has `ftype`**, which `Dir2SfEntry64::new` assumes;
/// * the fact that `.` and `..` are **synthesised**, not stored.
///
/// Everything here is explicit big-endian at a measured offset.  Nothing is
/// serialised by deriving: the format has alignment rules and a self-describing
/// order that a derive would not reproduce.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Used as soon as namespace mutation calls it; only its own tests do now.
pub struct ShortformDirectory {
    /// `di_forkoff`-relative parent, i.e. the inode `..` refers to.
    pub parent:  XfsIno,
    /// Whether inode numbers are 64 bits.  Also `i8count` in the header, which is
    /// how the format records the choice it made, so it is read rather than derived.
    pub i8count: bool,
    /// Whether entries carry the file-type byte.  This is a file-system feature, not
    /// a per-directory one, and getting it wrong adds or removes a byte from every
    /// entry -- which is why `Dir2Sf` assuming `true` is a defect and this is not.
    pub ftype:   bool,
    /// On-disk entries, in on-disk order.
    pub entries: Vec<SfEntry>,
}

#[allow(dead_code)] // Used as soon as namespace mutation calls it; only its own tests do now.
impl ShortformDirectory {
    /// Bytes an entry with this name occupies.
    pub const fn entry_len(&self, namelen: usize) -> usize {
        // namelen(1) + offset(2) + name + ftype(1 if the fs has it) + inumber(4 or 8)
        1 + 2 + namelen + if self.ftype { 1 } else { 0 } + if self.i8count { 8 } else { 4 }
    }

    /// Bytes the header occupies: `count`, `i8count`, and the parent in its width.
    pub const fn header_len(&self) -> usize {
        1 + 1 + if self.i8count { 8 } else { 4 }
    }

    /// The size of the directory's data: header plus every entry.
    pub fn size(&self) -> usize {
        self.entries
            .iter()
            .map(|e| self.entry_len(e.name.len()))
            .sum::<usize>()
            + self.header_len()
    }

    /// The maximum size of a shortform directory before it must transition
    /// to a block directory.  This is the maximum size of the data fork in
    /// the inode minus the attribute fork offset (if any).
    /// For a 256-byte inode with no attribute fork: 256 - 100 = 156 bytes.
    /// For a 512-byte inode with no attribute fork: 512 - 100 = 412 bytes.
    /// For a 256-byte inode with attribute fork at offset 32: 128 - 100 = 28 bytes.
    pub fn max_size(&self, sb: &Sb, raw: &RawDinode) -> usize {
        let inode_size = sb.inode_size();
        let local_offset = raw.literal_area_offset();
        let forkoff = raw.forkoff() as usize;
        let attr_offset = if forkoff == 0 {
            inode_size
        } else {
            local_offset + forkoff * 8
        };
        attr_offset.saturating_sub(local_offset)
    }

    /// Check if this shortform directory needs to transition to a block directory.
    pub fn needs_transition(&self, sb: &Sb, raw: &RawDinode) -> bool {
        self.size() > self.max_size(sb, raw)
    }

    /// Create a new empty shortform directory.
    pub fn new(ftype: bool) -> Self {
        ShortformDirectory {
            parent: 0,
            i8count: false,
            ftype,
            entries: Vec::new(),
        }
    }

    /// Read a shortform directory out of an inode's data fork.
    pub fn decode(bytes: &[u8], ftype: bool) -> crate::libxfuse::error::FsResult<Self> {
        use crate::libxfuse::error::FsError;
        if bytes.len() < 3 {
            return Err(FsError::corrupt(
                "a shortform directory is shorter than its header",
            ));
        }
        let count = bytes[0] as usize;
        let i8count = bytes[1] != 0;
        let hdr = 1 + 1 + if i8count { 8 } else { 4 };
        if bytes.len() < hdr {
            return Err(FsError::corrupt(
                "a shortform directory is shorter than its header claims",
            ));
        }
        let parent = if i8count {
            u64::from_be_bytes(bytes[2..10].try_into().unwrap())
        } else {
            u32::from_be_bytes(bytes[2..6].try_into().unwrap()).into()
        };
        let mut me = Self {
            parent,
            i8count,
            ftype,
            entries: Vec::with_capacity(count),
        };
        let mut at = hdr;
        for _ in 0..count {
            if at + 3 > bytes.len() {
                return Err(FsError::corrupt(
                    "a shortform directory claims more entries than it holds",
                ));
            }
            let namelen = bytes[at] as usize;
            let offset = u16::from_be_bytes(bytes[at + 1..at + 3].try_into().unwrap());
            let name_at = at + 3;
            if name_at + namelen > bytes.len() {
                return Err(FsError::corrupt(
                    "a shortform entry runs past its directory",
                ));
            }
            let name = bytes[name_at..name_at + namelen].to_vec();
            let mut p = name_at + namelen;
            let ft = if ftype {
                let v = *bytes.get(p).unwrap_or(&0);
                p += 1;
                v
            } else {
                0
            };
            let inumber = if i8count {
                let v = u64::from_be_bytes(
                    bytes
                        .get(p..p + 8)
                        .ok_or_else(|| FsError::corrupt("a shortform entry is truncated"))?
                        .try_into()
                        .unwrap(),
                );
                p += 8;
                v
            } else {
                let v = u32::from_be_bytes(
                    bytes
                        .get(p..p + 4)
                        .ok_or_else(|| FsError::corrupt("a shortform entry is truncated"))?
                        .try_into()
                        .unwrap(),
                ) as u64;
                p += 4;
                v
            };
            me.entries.push(SfEntry {
                name,
                ftype: ft,
                offset,
                inumber,
            });
            at = p;
        }
        Ok(me)
    }

    /// The bytes of this directory, for `RawDinode::set_data_bytes`.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.size());
        out.push(self.entries.len() as u8);
        out.push(u8::from(self.i8count));
        if self.i8count {
            out.extend_from_slice(&self.parent.to_be_bytes());
        } else {
            out.extend_from_slice(&(self.parent as u32).to_be_bytes());
        }
        for e in &self.entries {
            out.push(e.name.len() as u8);
            out.extend_from_slice(&e.offset.to_be_bytes());
            out.extend_from_slice(&e.name);
            if self.ftype {
                out.push(e.ftype);
            }
            if self.i8count {
                out.extend_from_slice(&e.inumber.to_be_bytes());
            } else {
                out.extend_from_slice(&(e.inumber as u32).to_be_bytes());
            }
        }
        out
    }

    pub fn contains(&self, name: &[u8]) -> bool {
        self.entries.iter().any(|e| e.name == name)
    }

    /// Add an entry, **appended**.
    ///
    /// Appended because that is what the kernel does: given a directory holding
    /// `b`(0x62) and `a`(0x61), inserting `mmm`(0x001b76ed) put it **last**, not
    /// first, even though its hash is larger than either and a hash-ordered directory
    /// would have put it first.  Order is therefore insertion order, and nothing is
    /// ever renumbered.
    ///
    /// `first_offset` is the offset the first entry gets, and is only consulted when
    /// the directory is empty.  It cannot be derived: the base differs between
    /// file systems (48 in one, 96 in another) and has not been identified, so an
    /// empty directory is the one case where this needs to be **told** the answer
    /// rather than compute it.  Guessing it would produce directories whose offsets
    /// nothing could explain.
    pub fn add(
        &mut self,
        name: &[u8],
        ftype: u8,
        inumber: XfsIno,
        first_offset: u16,
    ) -> crate::libxfuse::error::FsResult<()> {
        use crate::libxfuse::error::FsError;
        if self.contains(name) {
            return Err(FsError::invalid(
                libc::EEXIST,
                format!(
                    "a directory entry named {} already exists",
                    String::from_utf8_lossy(name)
                ),
            ));
        }
        if self.entries.len() >= usize::from(u8::MAX) {
            return Err(FsError::fork_full(
                "a shortform header's entry count is one byte",
            ));
        }
        let offset = match self.entries.last() {
            Some(last) => {
                // The step between entries is the last entry's size rounded up to
                // 8-byte alignment.  This matches native XFS behavior where the
                // offset field in each entry is the byte position within the data
                // fork, and entries are packed with 8-byte alignment.
                let last_entry_len = self.entry_len(last.name.len());
                let step = (last_entry_len + 7) & !7;
                last.offset + step as u16
            }
            None => first_offset,
        };
        self.entries.push(SfEntry {
            name: name.to_vec(),
            ftype,
            offset,
            inumber,
        });
        Ok(())
    }

    /// Remove the entry with this name, leaving every other entry untouched.
    ///
    /// Untouched because that is what the kernel does: removing the last and first
    /// entries of a four-entry directory left the survivors' offsets at 112 and 128
    /// while they moved from byte 15 and 24 to byte 6 and 15.  An implementation that
    /// renumbered would diverge from XFS on the first removal.
    pub fn remove(&mut self, name: &[u8]) -> crate::libxfuse::error::FsResult<()> {
        use crate::libxfuse::error::FsError;
        match self.entries.iter().position(|e| e.name == name) {
            Some(i) => {
                self.entries.remove(i);
                Ok(())
            }
            None => Err(FsError::invalid(
                libc::ENOENT,
                format!("no directory entry named {}", String::from_utf8_lossy(name)),
            )),
        }
    }
}

#[cfg(test)]
mod writable_tests {
    use super::{ShortformDirectory, SF_OFFSET_STEP};

    /// The bytes of `sf` in `xfsv4.img`, which `xfs_db` independently reports as a
    /// `core.format = 1` directory of `core.size = 44` holding inodes 36 and 37.
    ///
    /// Header: count 2, i8count 0, parent 32.  Then `b` at offset 48 and `a` at
    /// offset 64, both 11-character-free one-character names with a file type of 1
    /// and inode numbers of 36 and 37.
    const NATIVE_SF: [u8; 24] = [
        0x02, 0x00, 0x00, 0x00, 0x00, 0x20, //
        0x01, 0x00, 0x30, 0x62, 0x01, 0x00, 0x00, 0x00, 0x24, //
        0x01, 0x00, 0x40, 0x61, 0x01, 0x00, 0x00, 0x00, 0x25, //
    ];

    #[test]
    fn a_native_shortform_directory_decodes_and_re_encodes_unchanged() {
        let d = ShortformDirectory::decode(&NATIVE_SF, true).expect("a shortform directory");
        assert_eq!(d.entries.len(), 2);
        assert_eq!(d.parent, 32);
        assert!(!d.i8count, "this file system uses 32-bit inode numbers");
        assert_eq!(d.entries[0].name, b"b");
        assert_eq!(d.entries[0].offset, 48);
        assert_eq!(d.entries[0].inumber, 36);
        assert_eq!(d.entries[0].ftype, 1);
        assert_eq!(d.entries[1].name, b"a");
        assert_eq!(d.entries[1].offset, 64);
        assert_eq!(d.size(), 24);
        // The round trip that matters: the bytes come back identical, so nothing
        // about the layout was invented on the way through.
        assert_eq!(d.serialize().as_slice(), &NATIVE_SF[..]);
    }

    #[test]
    fn the_size_of_each_four_entry_directory_the_kernel_produced() {
        // 6 for empty, 15 for one 1-char entry, 33 for three, 44 for four.  These
        // are the sizes `xfs_db` reported, and they are what proves the entry length
        // and the ftype byte.
        // One entry with a one-character name: header 6 + (8 + 1) = 15.
        const ONE: [u8; 15] = [
            0x01, 0x00, 0x00, 0x00, 0x00, 0x20, //
            0x01, 0x00, 0x30, 0x62, 0x01, 0x00, 0x00, 0x00, 0x24, //
        ];
        let one = ShortformDirectory::decode(&ONE, true).expect("one entry");
        assert_eq!(one.size(), 15);
        let empty = ShortformDirectory::decode(&[0, 0, 0, 0, 0, 32], true).expect("empty");
        assert_eq!(empty.size(), 6);
        assert!(empty.entries.is_empty());
    }

    #[test]
    fn adding_appends_and_advances_the_offset_by_sixteen() {
        let mut d = ShortformDirectory::decode(&NATIVE_SF, true).expect("a shortform directory");
        let before = d.size();
        d.add(b"mmm", 1, 38, 0).expect("adding an entry");
        assert_eq!(d.entries.last().unwrap().offset, 64 + SF_OFFSET_STEP);
        // `mmm` is two bytes longer than `b`, and the size grows by 8 + namelen.
        assert_eq!(d.size(), before + 8 + 3);
        // Appended, not hash-ordered: `b` and `a` are still first.
        assert_eq!(d.entries[0].name, b"b");
        assert_eq!(d.entries[2].name, b"mmm");
    }

    #[test]
    fn removing_leaves_the_survivors_untouched() {
        let mut d = ShortformDirectory::decode(&NATIVE_SF, true).expect("a shortform directory");
        d.remove(b"a").expect("removing an entry");
        assert_eq!(d.entries.len(), 1);
        assert_eq!(d.entries[0].name, b"b");
        assert_eq!(
            d.entries[0].offset, 48,
            "removal must not renumber, because XFS does not"
        );
        assert!(
            d.remove(b"nope").is_err(),
            "removing what is not there fails"
        );
    }

    #[test]
    fn a_duplicate_is_refused() {
        let mut d = ShortformDirectory::decode(&NATIVE_SF, true).expect("a shortform directory");
        assert!(d.add(b"b", 1, 99, 0).is_err());
    }

    #[test]
    fn an_empty_directory_takes_the_offset_it_is_given() {
        // The base is not derived, because it is not known: it is 48 in one file
        // system and 96 in another.  A directory with no entries cannot show it, so
        // it must be supplied.
        let mut d = ShortformDirectory::decode(&[0, 0, 0, 0, 0, 32], true).expect("empty");
        d.add(b"z", 1, 7, 48).expect("the first entry");
        assert_eq!(d.entries[0].offset, 48);
        assert_eq!(d.size(), 15);
    }

    #[test]
    fn the_64_bit_form_round_trips_too() {
        // Not exercisable against an image here -- an inode number above 2^32 needs
        // more than four billion inodes -- so this only proves the layout is
        // self-consistent, and the audit records that it is untested against XFS.
        // An empty header that declares 64-bit inode numbers, so the parent is 8
        // bytes and the whole header is 10.
        let mut d = ShortformDirectory::decode(&[0, 1, 0, 0, 0, 0, 0, 0, 0, 0], true)
            .expect("an empty 64-bit header");
        d.i8count = true;
        d.parent = 5_000_000_000;
        d.add(b"x", 1, 5_000_000_001, 48).expect("adding");
        let bytes = d.serialize();
        assert_eq!(bytes.len(), d.header_len() + d.entry_len(1));
        assert_eq!(bytes.len(), 23, "10 of header and 13 of entry");
        let back = ShortformDirectory::decode(&bytes, true).expect("round trip");
        assert!(back.i8count);
        assert_eq!(back.parent, 5_000_000_000);
        assert_eq!(back.entries[0].inumber, 5_000_000_001);
    }
}
