/*
 * BSD 2-Clause License
 *
 * Copyright (c) 2026, the xfuse authors
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
//! The serialized form of an inode.
//!
//! # Why the bytes come first
//!
//! An XFS inode is a fixed-size region of the image, and most of what is in it
//! is not something this program needs to understand: reserved fields, the
//! device-inode fields, the copy-on-write size, the preallocation bookkeeping.
//! Reading such a structure is easy; *writing* one is where care is needed,
//! because anything the program does not understand must survive being
//! rewritten.
//!
//! So [`RawDinode`] keeps the inode's bytes exactly as they were read, and
//! modifies them in place.  Writing the inode back is then "put these bytes
//! where these bytes came from", with the fields that were deliberately changed
//! patched into it.  Nothing is regenerated from a Rust struct, so nothing is
//! silently defaulted, and a field this program has never heard of is
//! preserved by construction.
//!
//! # Where an inode lives
//!
//! An inode number is split into an allocation group, a block within that
//! group, and an index within that block:
//!
//! ```text
//! ino = [ ag number | block in ag | index in block ]
//! ```
//!
//! Multiplying the first two by the block size and the last by the inode size
//! gives the inode's byte offset in the image, which is what
//! [`Sb::inode_offset`](super::sb::Sb::inode_offset) computes.  Because the
//! offset is derived from the inode number rather than stored anywhere, an
//! inode does not have to be found before it can be modified.
//!
//! # What has to be fixed up when an inode changes
//!
//! * **Timestamps.**  XFS stores a timestamp as a 32-bit number of seconds
//!   since the epoch plus a nanosecond count.  Inodes that have the big-time
//!   flag store the same two fields as one 64-bit value, with the seconds in
//!   the high half.  The representation is a property of the inode, so the
//!   writer has to ask which one is in use rather than deciding for itself.
//! * **The checksum.**  Inode version 3, used by version 5 file systems,
//!   carries a CRC-32C over the whole inode, with the checksum field itself
//!   treated as zeroes.  An inode whose checksum does not match is a damaged
//!   inode, and `xfs_repair` will say so, so a modified inode must have its
//!   checksum recomputed.
//! * **The change counter.**  Version 3 inodes count their own modifications,
//!   which lets a later reader notice that something changed behind its back.
//! * **The log sequence number.**  A version 3 inode records which log
//!   transaction last wrote it.  A write that does not go through the log has
//!   no transaction, so the number is cleared; see [`RawDinode::finalise`].

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use byteorder::{BigEndian, ByteOrder, LittleEndian};
use crc::{Crc, CRC_32_ISCSI};

use super::{
    bmbt_rec::BmbtRec,
    definitions::{XfsFsize, XfsIno, XFS_DINODE_MAGIC},
    error::{FsError, FsResult},
};

/// Byte offsets of the fields of an inode.
///
/// The first part is common to all inode versions: the version 3 fields are
/// simply absent from versions 1 and 2, which is why the version 3 offsets
/// start again at 100 and the literal area of a version 1 or 2 inode begins
/// there.
///
/// This is a table of the format rather than a set of constants that the code
/// happens to use, so entries that no caller needs today are still part of it.
#[allow(dead_code)]
mod offset {
    pub const MAGIC: usize = 0;
    pub const MODE: usize = 2;
    pub const VERSION: usize = 4;
    pub const FORMAT: usize = 5;
    /// The link count of a version 1 inode.  Later versions use `NLINK2`.
    pub const ONLINK: usize = 6;
    pub const UID: usize = 8;
    pub const GID: usize = 12;
    /// The link count of a version 2 or 3 inode.
    pub const NLINK2: usize = 16;
    /// The 64-bit extent count of a version 3 inode.  In earlier versions these
    /// eight bytes hold the flush iterator and padding.
    pub const NEXTENTS: usize = 24;
    pub const ATIME: usize = 32;
    pub const MTIME: usize = 40;
    pub const CTIME: usize = 48;
    pub const SIZE: usize = 56;
    pub const NBLOCKS: usize = 64;
    pub const ANEXTENTS: usize = 80;
    pub const FORKOFF: usize = 82;
    pub const AFORMAT: usize = 83;
    pub const FLAGS: usize = 90;
    pub const GEN: usize = 92;
    /// The start of the version 3 fields.
    pub const V3: usize = 100;
    pub const CRC: usize = 100;
    pub const CHANGECOUNT: usize = 104;
    pub const LSN: usize = 112;
    pub const FLAGS2: usize = 120;
    pub const CRTIME: usize = 144;
    pub const INO: usize = 152;
    pub const UUID: usize = 160;
    /// The end of the version 3 fields, which is where an inode's data fork
    /// begins.
    pub const V3_END: usize = 176;

    /// The 32-bit extent count.  In a version 3 inode with 64-bit counts this
    /// field is instead the attribute fork's 64-bit count, and the data fork's
    /// count moves to `NEXTENTS`.
    pub const NEXTENTS32: usize = 76;
}

/// The 64-bit extent count flag, which swaps the width of the extent counts
/// and of the extent records that go with them.
#[allow(dead_code)] // Used as soon as a file's extents are changed.
const FLAGS2_NREXT64: u64 = 1 << 4;
/// The big-time flag, which changes how timestamps are stored.
const FLAGS2_BIGTIME: u64 = 1 << 3;

/// The instant that a big-time timestamp counts from.
///
/// It is the same instant that the smallest value of a 32-bit signed second
/// count represents, which keeps the two representations describing the same
/// range of time.
fn bigtime_epoch() -> SystemTime {
    // 1901-12-13 20:45:52 UTC is 2^31 seconds before the Unix epoch, which is
    // the smallest instant a 32-bit signed second count can express.
    UNIX_EPOCH - Duration::from_secs(1u64 << 31)
}

/// The size of the "local" area, where a data fork that is held in the inode
/// itself lives, for each inode version.
const fn literal_area_offset(version: i8) -> usize {
    match version {
        1 | 2 => offset::V3,
        3 => offset::V3_END,
        _ => 0,
    }
}

/// The serialized form of one inode: the bytes as they are on the image, plus
/// typed accessors that patch individual fields.
///
/// An inode is not written back automatically.  The caller patches the fields
/// it wants to change and then calls [`RawDinode::finalise`], which fixes up
/// everything that must be consistent with those changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawDinode {
    bytes:   Box<[u8]>,
    version: i8,
    /// Set when a field has been changed, so that the checksum and the change
    /// counter are only touched when there is something to protect.
    dirty:   bool,
}

impl RawDinode {
    /// Take ownership of an inode's bytes.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> FsResult<Self> {
        let bytes = bytes.into();
        if bytes.len() < offset::V3 {
            return Err(FsError::corrupt(format!(
                "inode of {} bytes is too small to be an inode",
                bytes.len()
            )));
        }
        let magic = BigEndian::read_u16(&bytes[offset::MAGIC..]);
        if magic != XFS_DINODE_MAGIC {
            return Err(FsError::corrupt(format!(
                "bad inode magic number {magic:#06x}"
            )));
        }
        let version = bytes[offset::VERSION] as i8;
        let literal = literal_area_offset(version);
        if literal == 0 {
            return Err(FsError::corrupt(format!(
                "unsupported inode version {version}"
            )));
        }
        if bytes.len() < literal {
            return Err(FsError::corrupt(format!(
                "inode of {} bytes is too small for a version {version} inode",
                bytes.len()
            )));
        }
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
            version,
            dirty: false,
        })
    }

    /// The inode's bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consume the inode and take its bytes.
    pub fn into_bytes(self) -> Box<[u8]> {
        self.bytes
    }

    /// The inode version: 1, 2, or 3.
    pub const fn version(&self) -> i8 {
        self.version
    }

    /// The inode number this inode claims, for the versions that store it.
    pub fn ino(&self) -> Option<XfsIno> {
        match self.version {
            3 => Some(BigEndian::read_u64(&self.bytes[offset::INO..])),
            _ => None,
        }
    }

    /// The file type and permissions, as XFS stores them: the type in the high
    /// bits and the permissions in the low ones.
    pub fn mode(&self) -> u16 {
        BigEndian::read_u16(&self.bytes[offset::MODE..])
    }

    /// Used by the metadata operations, which are a later phase.
    #[allow(dead_code)]
    pub fn set_mode(&mut self, mode: u16) {
        BigEndian::write_u16(&mut self.bytes[offset::MODE..], mode);
        self.dirty = true;
    }

    pub fn uid(&self) -> u32 {
        BigEndian::read_u32(&self.bytes[offset::UID..])
    }

    /// Used by the metadata operations, which are a later phase.
    #[allow(dead_code)]
    pub fn set_uid(&mut self, uid: u32) {
        BigEndian::write_u32(&mut self.bytes[offset::UID..], uid);
        self.dirty = true;
    }

    pub fn gid(&self) -> u32 {
        BigEndian::read_u32(&self.bytes[offset::GID..])
    }

    /// Used by the metadata operations, which are a later phase.
    #[allow(dead_code)]
    pub fn set_gid(&mut self, gid: u32) {
        BigEndian::write_u32(&mut self.bytes[offset::GID..], gid);
        self.dirty = true;
    }

    /// The number of names that refer to this inode.  A version 1 inode keeps
    /// this in a 16-bit field that later versions use for something else.
    #[allow(dead_code)]
    pub fn nlink(&self) -> u32 {
        match self.version {
            1 => u32::from(BigEndian::read_u16(&self.bytes[offset::ONLINK..])),
            _ => BigEndian::read_u32(&self.bytes[offset::NLINK2..]),
        }
    }

    /// Used by the metadata operations, which are a later phase.
    #[allow(dead_code)]
    pub fn set_nlink(&mut self, nlink: u32) {
        match self.version {
            1 => BigEndian::write_u16(&mut self.bytes[offset::ONLINK..], nlink as u16),
            _ => BigEndian::write_u32(&mut self.bytes[offset::NLINK2..], nlink),
        }
        self.dirty = true;
    }

    /// The size of the file, in bytes.
    pub fn size(&self) -> XfsFsize {
        BigEndian::read_i64(&self.bytes[offset::SIZE..])
    }

    #[allow(dead_code)]
    pub fn set_size(&mut self, size: XfsFsize) {
        BigEndian::write_i64(&mut self.bytes[offset::SIZE..], size);
        self.dirty = true;
    }

    /// How many file system blocks this inode's forks occupy, counting
    /// indirect blocks and the attribute fork as well as data.
    #[allow(dead_code)]
    pub fn nblocks(&self) -> u64 {
        BigEndian::read_u64(&self.bytes[offset::NBLOCKS..])
    }

    /// How many extents the data fork has, taking the large-extent-count
    /// feature into account.
    ///
    /// Inodes that do not use that feature keep the count in a 32-bit field
    /// next to the attribute fork's count.  Inodes that do use it widen both
    /// counts, and the data fork's moves to the 64-bit field near the top of
    /// the inode, leaving this field to the attribute fork.
    pub fn nextents(&self) -> u64 {
        if self.nrext64() {
            BigEndian::read_u64(&self.bytes[offset::NEXTENTS..])
        } else {
            u64::from(BigEndian::read_u32(&self.bytes[offset::NEXTENTS32..]))
        }
    }

    /// How many extents the attribute fork has.
    pub fn anextents(&self) -> u32 {
        if self.nrext64() {
            BigEndian::read_u32(&self.bytes[offset::NEXTENTS32..])
        } else {
            u32::from(BigEndian::read_u16(&self.bytes[offset::ANEXTENTS..]))
        }
    }

    /// Where the attribute fork begins, as a number of eight-byte units from
    /// the start of the local area.  Zero when there is no attribute fork.
    #[allow(dead_code)]
    pub fn forkoff(&self) -> u8 {
        self.bytes[offset::FORKOFF]
    }

    /// The format of the data fork: `1` for local, `2` for an extent list in
    /// the inode, `3` for a B+tree rooted in the inode.
    pub fn format(&self) -> u8 {
        self.bytes[offset::FORMAT]
    }

    /// The format of the attribute fork, in the same numbering as
    /// [`RawDinode::format`].
    #[allow(dead_code)]
    pub fn aformat(&self) -> u8 {
        self.bytes[offset::AFORMAT]
    }

    /// The inode's flags, as defined by the file system format.
    #[allow(dead_code)]
    pub fn flags(&self) -> u16 {
        BigEndian::read_u16(&self.bytes[offset::FLAGS..])
    }

    /// The inode's generation number, which the file system uses to tell an
    /// old inode from a new one that reused its number.
    #[allow(dead_code)]
    pub fn gen(&self) -> u32 {
        BigEndian::read_u32(&self.bytes[offset::GEN..])
    }

    /// The offset within the image at which this inode's local area starts.
    pub const fn literal_area_offset(&self) -> usize {
        literal_area_offset(self.version)
    }

    /// Is this inode's data on a real-time device?
    pub fn is_realtime(&self) -> bool {
        // Bit 0 of the flags: the data lives on the real-time device.
        self.flags() & 1 != 0
    }

    /// Does this inode store 64-bit extent counts?
    pub fn nrext64(&self) -> bool {
        self.version == 3 && self.flags2() & FLAGS2_NREXT64 != 0
    }

    /// Does this inode store timestamps in the wide form?
    pub fn is_bigtime(&self) -> bool {
        self.version == 3 && self.flags2() & FLAGS2_BIGTIME != 0
    }

    /// The version 3 flags, or zero for earlier versions.
    pub fn flags2(&self) -> u64 {
        match self.version {
            3 => BigEndian::read_u64(&self.bytes[offset::FLAGS2..]),
            _ => 0,
        }
    }

    /// The inode's log sequence number, or zero for a version 1 or 2 inode.
    #[allow(dead_code)]
    pub fn lsn(&self) -> u64 {
        match self.version {
            3 => BigEndian::read_u64(&self.bytes[offset::LSN..]),
            _ => 0,
        }
    }

    /// How many times this inode has been modified, as recorded by the file
    /// system itself.
    #[allow(dead_code)]
    pub fn change_count(&self) -> u64 {
        match self.version {
            3 => BigEndian::read_u64(&self.bytes[offset::CHANGECOUNT..]),
            _ => 0,
        }
    }

    /// Read a timestamp, converting whichever representation this inode uses
    /// into an ordinary system time.
    pub fn timestamp(&self, at: usize) -> SystemTime {
        if self.is_bigtime() {
            // In the wide form the two halves are one 64-bit number of
            // nanoseconds, counted from the same instant that a 32-bit signed
            // second count starts at: 1901-12-13 20:45:52 UTC.  A time before
            // that instant is a negative count.
            let nanos = BigEndian::read_i64(&self.bytes[at..]);
            match nanos.cmp(&0) {
                std::cmp::Ordering::Less => {
                    bigtime_epoch() - Duration::from_nanos(nanos.unsigned_abs())
                }
                _ => bigtime_epoch() + Duration::from_nanos(nanos as u64),
            }
        } else {
            let sec = BigEndian::read_i32(&self.bytes[at..]);
            let nsec = BigEndian::read_u32(&self.bytes[at + 4..]);
            if sec >= 0 {
                UNIX_EPOCH + Duration::new(sec as u64, nsec)
            } else {
                UNIX_EPOCH - Duration::new(sec.unsigned_abs() as u64, 0)
                    + Duration::from_nanos(nsec as u64)
            }
        }
    }

    /// When this inode was last read.
    pub fn atime(&self) -> SystemTime {
        self.timestamp(offset::ATIME)
    }

    /// When this inode's data was last written.
    pub fn mtime(&self) -> SystemTime {
        self.timestamp(offset::MTIME)
    }

    /// When this inode's metadata was last changed.
    pub fn ctime(&self) -> SystemTime {
        self.timestamp(offset::CTIME)
    }

    /// Store a timestamp in the representation this inode uses.
    ///
    /// A time that the representation cannot express — a negative second count
    /// in the narrow form, or a time more than 584 years from the big-time
    /// epoch — is clamped to the nearest value it can express, because a
    /// clamped timestamp is a lesser evil than a mangled inode, and because the
    /// clock of the machine writing the file is what is being recorded anyway.
    pub fn set_timestamp(&mut self, at: usize, time: SystemTime) {
        if self.is_bigtime() {
            let nanos: i128 = match time.duration_since(bigtime_epoch()) {
                Ok(d) => d.as_nanos() as i128,
                Err(e) => -(e.duration().as_nanos() as i128),
            };
            let clamped = nanos.clamp(i64::MIN as i128, i64::MAX as i128);
            BigEndian::write_i64(&mut self.bytes[at..], clamped as i64);
        } else {
            let (secs, nanos) = match time.duration_since(UNIX_EPOCH) {
                Ok(d) => (d.as_secs() as i128, d.subsec_nanos()),
                Err(e) => {
                    let d = e.duration();
                    (-(d.as_secs() as i128), d.subsec_nanos())
                }
            };
            let clamped = secs.clamp(i32::MIN as i128, i32::MAX as i128) as i32;
            BigEndian::write_i32(&mut self.bytes[at..], clamped);
            BigEndian::write_u32(&mut self.bytes[at + 4..], nanos);
        }
        self.dirty = true;
    }

    /// Record when the file's data was last written.
    pub fn set_mtime(&mut self, time: SystemTime) {
        self.set_timestamp(offset::MTIME, time)
    }

    /// Record when the inode's metadata last changed.
    pub fn set_ctime(&mut self, time: SystemTime) {
        self.set_timestamp(offset::CTIME, time)
    }

    /// Record when the file was last read.
    #[allow(dead_code)]
    pub fn set_atime(&mut self, time: SystemTime) {
        self.set_timestamp(offset::ATIME, time)
    }

    /// Where this inode's data fork begins within the inode.
    pub const fn dfork_offset(&self) -> usize {
        0
    }

    /// Read the extent records of a data fork that is stored in the inode.
    ///
    /// A data fork in the `extents` format is simply a run of fixed-size extent
    /// records starting at the beginning of the local area, one per extent.
    /// Returns `None` if the fork is not in that format.
    #[allow(dead_code)] // Used as soon as a file's extents are changed.
    pub fn core_extents(&self) -> Option<Vec<BmbtRec>> {
        if self.format() != 2 {
            return None;
        }
        let n = usize::try_from(self.nextents()).ok()?;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let at = self.literal_area_offset() + i * EXTENT_REC_SIZE;
            out.push(decode_extent(
                &self.bytes[at..at + EXTENT_REC_SIZE],
                self.nrext64(),
            )?);
        }
        Some(out)
    }

    /// Replace the extent records of a data fork that is stored in the inode.
    ///
    /// The records are written over the old ones, and the count is updated.
    /// A fork that is not in the `extents` format is not touched, because a
    /// B+tree's records live in its own blocks and rewriting them here would
    /// throw the tree away.
    #[allow(dead_code)] // Used as soon as a file's extents are changed.
    pub fn set_core_extents(&mut self, extents: &[BmbtRec]) -> FsResult<()> {
        if self.format() != 2 {
            return Err(FsError::unsupported(format!(
                "rewriting a data fork in format {}",
                self.format()
            )));
        }
        if self.nrext64() {
            return Err(FsError::unsupported(
                "rewriting a data fork with 64-bit extent counts",
            ));
        }
        // The count of extents the data fork can hold, without turning a local
        // fork into a B+tree, is how much room there is in the local area
        // before the attribute fork starts.
        let start = self.literal_area_offset();
        let limit = self.attribute_fork_offset().unwrap_or(self.bytes.len());
        let room = limit.saturating_sub(start) / EXTENT_REC_SIZE;
        if extents.len() > room {
            return Err(FsError::NoSpace);
        }
        for (i, extent) in extents.iter().enumerate() {
            let at = start + i * EXTENT_REC_SIZE;
            encode_extent(&mut self.bytes[at..at + EXTENT_REC_SIZE], extent);
        }
        let count = extents.len() as u64;
        if self.nrext64() {
            BigEndian::write_u64(&mut self.bytes[offset::NEXTENTS..], count);
        } else {
            BigEndian::write_u32(&mut self.bytes[offset::NEXTENTS32..], count as u32);
        }
        self.dirty = true;
        Ok(())
    }

    /// Where the attribute fork begins, in bytes, if there is one.
    pub fn attribute_fork_offset(&self) -> Option<usize> {
        match self.forkoff() {
            0 => None,
            n => Some(self.literal_area_offset() + n as usize * 8),
        }
    }

    /// Has anything been changed?
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Finish the modification: account for the change, and make the inode
    /// self-consistent again.
    ///
    /// For a version 3 inode that means bumping the change counter, clearing
    /// the log sequence number, and recomputing the checksum.  The sequence
    /// number is cleared rather than left alone because this write did not go
    /// through the log: an inode that still claims to be part of some earlier
    /// log transaction could have that transaction replayed over it, which
    /// would undo the change.  Once the journal exists, the transaction will
    /// set the sequence number to the transaction that logs the inode, and this
    /// will change accordingly.
    pub fn finalise(&mut self) {
        if !self.dirty {
            return;
        }
        if self.version == 3 {
            let count = self.change_count().wrapping_add(1);
            BigEndian::write_u64(&mut self.bytes[offset::CHANGECOUNT..], count);
            BigEndian::write_u64(&mut self.bytes[offset::LSN..], 0);
            self.update_crc();
        }
        self.dirty = false;
    }

    /// Recompute the inode's checksum over its own bytes.
    ///
    /// A version 3 inode's checksum is a CRC-32C over the whole inode, with
    /// the checksum field itself read as zeroes, which is what makes the
    /// checksum a function of everything else in the inode.  The result is
    /// stored least-significant byte first, which is how version 5 metadata
    /// keeps its checksums.
    pub fn update_crc(&mut self) {
        if self.version != 3 {
            return;
        }
        LittleEndian::write_u32(&mut self.bytes[offset::CRC..], 0);
        const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);
        let crc = CASTAGNOLI.checksum(&self.bytes);
        LittleEndian::write_u32(&mut self.bytes[offset::CRC..], crc);
    }

    /// Is this inode's stored checksum correct?
    pub fn verify_crc(&self) -> bool {
        if self.version != 3 {
            return true;
        }
        let stored = LittleEndian::read_u32(&self.bytes[offset::CRC..]);
        let mut copy = self.bytes.to_vec();
        LittleEndian::write_u32(&mut copy[offset::CRC..], 0);
        const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);
        CASTAGNOLI.checksum(&copy) == stored
    }
}

/// The size, in bytes, of an extent record that is stored in an inode.
///
/// The encoder and the decoder below are the on-disk form of a file's data fork
/// when the fork is held in the inode.  They are tested now, against both
/// hand-built and real inodes, so that the format is pinned down before
/// anything depends on it; the first operation that actually rewrites an
/// extent list is a later phase.
///
/// A record packs the offset within the file, the starting block, the number of
/// blocks, and the "not written yet" flag into one 128-bit number, big-endian.
const EXTENT_REC_SIZE: usize = 16;

/// Decode one extent record.
///
/// `nrext64` selects between the narrow record, in which the length is 21 bits,
/// and the wide one, in which it is 63.
#[allow(dead_code)] // Used as soon as a file's extents are changed.
fn decode_extent(bytes: &[u8], nrext64: bool) -> Option<BmbtRec> {
    if bytes.len() < EXTENT_REC_SIZE {
        return None;
    }
    let mut raw = [0u8; 16];
    raw.copy_from_slice(&bytes[..EXTENT_REC_SIZE]);
    let br = u128::from_be_bytes(raw);
    let (blockcount_bits, blockcount_shift) = if nrext64 { (63, 0) } else { (21, 21) };
    let br_blockcount = (br & ((1 << blockcount_bits) - 1)) as u64;
    let br = br >> blockcount_shift;
    let br_startblock = (br & ((1 << 52) - 1)) as u64;
    let br = br >> 52;
    let br_startoff = (br & ((1 << 54) - 1)) as u64;
    let br_flag = (br >> 54) != 0;
    Some(BmbtRec {
        br_startoff,
        br_startblock,
        br_blockcount,
        br_flag,
    })
}

/// Encode one extent record, in the narrow form.
#[allow(dead_code)] // Used as soon as a file's extents are changed.
fn encode_extent(bytes: &mut [u8], rec: &BmbtRec) {
    assert!(bytes.len() >= EXTENT_REC_SIZE);
    debug_assert!(rec.br_blockcount < (1 << 21));
    // The record is one 128-bit number: the block count in the low bits, then
    // the starting block, then the offset within the file, and the "not
    // written yet" flag in the very top bit.
    let br_flag = u128::from(rec.br_flag) << 127;
    let br_startoff = u128::from(rec.br_startoff) << 73;
    let br_startblock = u128::from(rec.br_startblock) << 21;
    let br_blockcount = u128::from(rec.br_blockcount);
    let br = br_startoff | br_startblock | br_blockcount | br_flag;
    bytes[..EXTENT_REC_SIZE].copy_from_slice(&br.to_be_bytes());
}

#[cfg(test)]
mod t {
    use super::*;

    const V3_INODE_HEX: &[&str] = &[
        "49 4e 81 a4 03 03 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 01 00 00 00 00 00 00 00 00 00 00 00 00",
        "35 9e f9 f6 b3 2c 31 8f 35 9e f9 f6 d3 d6 07 93",
        "35 9e f9 f6 d3 d6 07 93 00 00 00 00 00 01 00 00",
        "00 00 00 00 00 00 00 42 00 00 00 00 00 00 00 40",
        "00 00 00 02 00 00 00 00 00 00 00 00 25 bf f2 9d",
        "ff ff ff ff 82 59 b5 a8 00 00 00 00 00 00 01 05",
        "00 00 00 02 00 00 6e 02 00 00 00 00 00 00 00 08",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "35 9e f9 f6 b3 2c 31 8f 00 00 00 00 00 08 03 33",
        "4a 83 99 f3 a6 fc 43 4d 80 2a 47 1c d1 0c 26 9c",
        "00 01 00 02 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 1e 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 04 01 a5 00 00 00 00",
        "00 04 01 a7 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
    ];

    const V2_INODE_HEX: &[&str] = &[
        "49 4e 81 a4 02 03 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 01 00 00 00 00 00 00 00 00 00 00 00 01",
        "66 7d 8e 53 0e f9 59 56 66 7d 8e 53 2e af 0a 1c",
        "66 7d 8e 53 2e af 0a 1c 00 00 00 00 00 00 80 00",
        "00 00 00 00 00 00 00 43 00 00 00 00 00 00 00 40",
        "00 00 00 02 00 00 00 00 00 00 00 00 b3 cb bf 84",
        "ff ff ff ff 00 01 00 03 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 1e 00 00 00 00 00 00 00 2d",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 c4 89 00 00 00 00 00 00 c4 8b",
        "00 00 00 00 00 00 c4 8d 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
    ];

    /// Turn a list of hex rows into the bytes they describe.
    fn unhex(rows: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for row in rows {
            for byte in row.split(' ') {
                out.push(u8::from_str_radix(byte, 16).expect("hex"));
            }
        }
        out
    }

    /// A plausible version 3 inode: 512 bytes, a regular file with a B+tree
    /// data fork, big timestamps, and a checksum that is initially correct.
    fn v3_inode() -> RawDinode {
        let mut bytes = vec![0u8; 512];
        BigEndian::write_u16(&mut bytes[offset::MAGIC..], XFS_DINODE_MAGIC);
        BigEndian::write_u16(&mut bytes[offset::MODE..], 0o100644);
        bytes[offset::VERSION] = 3;
        bytes[offset::FORMAT] = 3;
        BigEndian::write_u32(&mut bytes[offset::UID..], 1000);
        BigEndian::write_u32(&mut bytes[offset::GID..], 1000);
        BigEndian::write_u32(&mut bytes[offset::NLINK2..], 1);
        BigEndian::write_u32(&mut bytes[offset::NEXTENTS32..], 3);
        BigEndian::write_i64(&mut bytes[offset::SIZE..], 32768);
        BigEndian::write_u64(&mut bytes[offset::NBLOCKS..], 67);
        bytes[offset::AFORMAT] = 2;
        BigEndian::write_u64(&mut bytes[offset::FLAGS2..], FLAGS2_BIGTIME);
        BigEndian::write_u64(&mut bytes[offset::CHANGECOUNT..], 7);
        BigEndian::write_u64(&mut bytes[offset::LSN..], 0xdead_beef);
        BigEndian::write_u64(&mut bytes[offset::INO..], 12345);
        let mut inode = RawDinode::from_bytes(bytes).unwrap();
        inode.update_crc();
        inode
    }

    /// A plausible version 2 inode: 256 bytes, no checksum and no version 3
    /// fields at all.
    fn v2_inode() -> RawDinode {
        let mut bytes = vec![0u8; 256];
        BigEndian::write_u16(&mut bytes[offset::MAGIC..], XFS_DINODE_MAGIC);
        BigEndian::write_u16(&mut bytes[offset::MODE..], 0o100644);
        bytes[offset::VERSION] = 2;
        bytes[offset::FORMAT] = 2;
        BigEndian::write_u32(&mut bytes[offset::UID..], 0);
        BigEndian::write_u32(&mut bytes[offset::NLINK2..], 2);
        BigEndian::write_i64(&mut bytes[offset::SIZE..], 512);
        BigEndian::write_u64(&mut bytes[offset::NBLOCKS..], 2);
        BigEndian::write_u32(&mut bytes[offset::NEXTENTS32..], 1);
        bytes[offset::AFORMAT] = 2;
        RawDinode::from_bytes(bytes).unwrap()
    }

    /// Reading an inode must not change it.
    #[test]
    fn read_only_accessors() {
        let inode = v3_inode();
        assert_eq!(inode.version(), 3);
        assert_eq!(inode.mode(), 0o100644);
        assert_eq!(inode.uid(), 1000);
        assert_eq!(inode.nlink(), 1);
        assert_eq!(inode.size(), 32768);
        assert_eq!(inode.nblocks(), 67);
        assert_eq!(inode.ino(), Some(12345));
        assert_eq!(inode.nextents(), 3);
        assert_eq!(inode.gen(), 0);
        assert!(inode.is_bigtime());
        assert!(!inode.nrext64());
        assert!(!inode.is_realtime());
        assert!(!inode.is_dirty());
        assert!(inode.verify_crc());
    }

    /// A change must be visible in the bytes and must be reflected in the
    /// checksum, the change counter, and the log sequence number.
    #[test]
    fn modification_updates_consistency() {
        let mut inode = v3_inode();
        let before = inode.change_count();
        assert_eq!(inode.lsn(), 0xdead_beef);

        inode.set_size(4096);
        inode.set_mtime(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        inode.finalise();

        assert_eq!(inode.size(), 4096);
        assert_eq!(inode.change_count(), before + 1);
        assert_eq!(inode.lsn(), 0);
        assert!(inode.verify_crc());

        assert_eq!(inode.mtime(), UNIX_EPOCH + Duration::new(1_700_000_000, 0));
    }

    /// Timestamps stored in the wide form must survive a round trip.
    #[test]
    fn bigtime_round_trip() {
        let mut inode = v3_inode();
        let t = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
        inode.set_mtime(t);
        inode.finalise();
        assert_eq!(inode.mtime(), t);
    }

    /// An inode without the big-time flag must store and read timestamps in the
    /// narrow form, and must not silently widen them.
    #[test]
    fn narrow_time_round_trip() {
        let mut bytes = v3_inode().into_bytes();
        BigEndian::write_u64(&mut bytes[offset::FLAGS2..], 0);
        let mut inode = RawDinode::from_bytes(bytes.to_vec()).unwrap();
        let t = UNIX_EPOCH + Duration::new(1_000_000, 999);
        inode.set_ctime(t);
        inode.finalise();
        assert!(!inode.is_bigtime());
        assert_eq!(inode.ctime(), t);
    }

    /// Extent records must survive being read and written back.
    #[test]
    fn core_extents_round_trip() {
        let mut bytes = v3_inode().into_bytes();
        bytes[offset::FORMAT] = 2;
        BigEndian::write_u32(&mut bytes[offset::NEXTENTS32..], 0);
        BigEndian::write_u64(&mut bytes[offset::FLAGS2..], 0);
        let mut inode = RawDinode::from_bytes(bytes.to_vec()).unwrap();
        let extents = [
            BmbtRec {
                br_startoff:   0,
                br_startblock: 100,
                br_blockcount: 4,
                br_flag:       false,
            },
            BmbtRec {
                br_startoff:   4,
                br_startblock: 200,
                br_blockcount: 2,
                br_flag:       true,
            },
        ];
        inode.set_core_extents(&extents).unwrap();
        inode.finalise();
        assert!(inode.verify_crc());

        let reread = RawDinode::from_bytes(inode.into_bytes()).unwrap();
        // The extent count goes back where the 32-bit count lives, which is
        // what a reader of an inode without 64-bit counts will look at.
        assert_eq!(reread.nextents(), 2);
        let back = reread.core_extents().unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].br_startoff, 0);
        assert_eq!(back[0].br_startblock, 100);
        assert_eq!(back[0].br_blockcount, 4);
        assert!(!back[0].br_flag);
        assert_eq!(back[1].br_startoff, 4);
        assert_eq!(back[1].br_startblock, 200);
        assert!(back[1].br_flag);
    }

    /// Bytes this program does not understand must survive a modification.
    #[test]
    fn unknown_fields_are_preserved() {
        let mut bytes = v3_inode().into_bytes();
        // Fill the reserved areas with a pattern.
        for b in &mut bytes[128..132] {
            *b = 0xa5;
        }
        for b in &mut bytes[132..144] {
            *b = 0x5a;
        }
        for b in &mut bytes[176..512] {
            *b = 0x77;
        }
        let mut inode = RawDinode::from_bytes(bytes).unwrap();
        inode.set_size(1);
        inode.finalise();
        let out = inode.as_bytes();
        assert!(out[128..132].iter().all(|b| *b == 0xa5));
        assert!(out[132..144].iter().all(|b| *b == 0x5a));
        assert!(out[176..512].iter().all(|b| *b == 0x77));
        assert!(inode.verify_crc());
    }

    /// A version 2 inode has no checksum and no version 3 fields, and its link
    /// count lives in the 32-bit field.
    #[test]
    fn v2_has_no_crc() {
        let mut inode = v2_inode();
        assert_eq!(inode.version(), 2);
        assert_eq!(inode.literal_area_offset(), 100);
        assert!(inode.verify_crc());
        assert_eq!(inode.lsn(), 0);
        inode.set_nlink(3);
        inode.set_uid(7);
        inode.finalise();
        assert_eq!(inode.nlink(), 3);
        assert_eq!(inode.uid(), 7);
    }

    /// A version 1 inode keeps its link count in the 16-bit field.
    #[test]
    fn v1_link_count() {
        let mut bytes = vec![0u8; 256];
        BigEndian::write_u16(&mut bytes[offset::MAGIC..], XFS_DINODE_MAGIC);
        bytes[offset::VERSION] = 1;
        bytes[offset::FORMAT] = 2;
        BigEndian::write_u16(&mut bytes[offset::ONLINK..], 5);
        let mut inode = RawDinode::from_bytes(bytes).unwrap();
        assert_eq!(inode.nlink(), 5);
        inode.set_nlink(9);
        assert_eq!(inode.nlink(), 9);
    }

    /// A version 3 inode taken from a real file system must pass its own
    /// checksum, which is the only thing that proves this implementation
    /// agrees with the one that wrote it.
    ///
    /// The bytes are the inode of an ordinary 64 KiB file with a B+tree data
    /// fork on a version 5 file system with 1 KiB blocks and 512-byte inodes.
    #[test]
    fn real_v3_inode_verifies() {
        let inode = RawDinode::from_bytes(unhex(V3_INODE_HEX)).unwrap();
        assert_eq!(inode.version(), 3);
        assert!(inode.verify_crc(), "checksum of a real inode must verify");
        assert_eq!(inode.size(), 65536);
        assert_eq!(inode.nblocks(), 66);
        assert_eq!(inode.nextents(), 64);
        assert_eq!(inode.anextents(), 0);
        assert_eq!(inode.gen(), 633336477);
        assert_eq!(inode.ino(), Some(525107));
        assert!(inode.is_bigtime());
        assert!(!inode.nrext64());
        assert_eq!(inode.change_count(), 261);
        assert_eq!(inode.forkoff(), 0);
        assert_eq!(inode.format(), 3);
        assert_eq!(inode.aformat(), 2);
        assert!(!inode.is_realtime());
    }

    /// The same, for the fields a write changes: a real inode's timestamps must
    /// come back as the times they are, to the nanosecond.
    #[test]
    fn real_v3_inode_timestamps() {
        let inode = RawDinode::from_bytes(unhex(V3_INODE_HEX)).unwrap();
        let mtime = inode.mtime();
        let since = mtime.duration_since(UNIX_EPOCH).expect("after the epoch");
        assert_eq!(since.as_secs(), 1_716_316_720);
        assert_eq!(since.subsec_nanos(), 841_754_515);
        assert_eq!(inode.ctime(), mtime);
        let atime = inode.atime();
        let since = atime.duration_since(UNIX_EPOCH).unwrap();
        assert_eq!(since.as_secs(), 1_716_316_720);
        assert_eq!(since.subsec_nanos(), 293_753_231);
    }

    /// A real version 2 inode has no checksum, keeps its link count in the
    /// 32-bit field, and keeps its timestamps in the narrow form.
    #[test]
    fn real_v2_inode() {
        let inode = RawDinode::from_bytes(unhex(V2_INODE_HEX)).unwrap();
        assert_eq!(inode.version(), 2);
        assert!(inode.verify_crc());
        assert_eq!(inode.size(), 32768);
        assert_eq!(inode.nblocks(), 67);
        assert_eq!(inode.nextents(), 64);
        assert_eq!(inode.anextents(), 0);
        assert_eq!(inode.gen(), 3_016_474_500);
        assert_eq!(inode.nlink(), 1);
        assert!(!inode.is_bigtime());
        assert_eq!(inode.ino(), None);
        let mtime = inode.mtime();
        let since = mtime.duration_since(UNIX_EPOCH).unwrap();
        assert_eq!(since.as_secs(), 1_719_504_467);
        assert_eq!(since.subsec_nanos(), 783_223_324);
    }

    /// Modifying a real version 3 inode must leave it verifiable, and must
    /// change only the fields it was asked to change plus the three that follow
    /// from a change.
    #[test]
    fn real_v3_inode_survives_a_change() {
        let mut inode = RawDinode::from_bytes(unhex(V3_INODE_HEX)).unwrap();
        let before = inode.as_bytes().to_vec();
        let gen = inode.gen();
        let uuid = inode.ino();
        let ino = inode.ino();
        inode.set_mtime(UNIX_EPOCH + Duration::new(1_700_000_000, 7));
        inode.finalise();
        assert!(inode.verify_crc());
        assert_eq!(inode.gen(), gen);
        assert_eq!(uuid, ino);
        // The change counter moved on, the log sequence number was cleared, and
        // the timestamps were the point of the exercise.
        assert_eq!(inode.change_count(), 262);
        assert_eq!(inode.lsn(), 0);
        let since = inode.mtime().duration_since(UNIX_EPOCH).unwrap();
        assert_eq!((since.as_secs(), since.subsec_nanos()), (1_700_000_000, 7));
        // Everything the write did not touch, it did not touch.
        let after = inode.as_bytes();
        for i in 0..before.len() {
            let changed = (32..48).contains(&i) || (100..120).contains(&i);
            if !changed {
                assert_eq!(before[i], after[i], "byte {i} changed unexpectedly");
            }
        }
    }

    /// Damaged or impossible inodes must be reported, not accepted.
    #[test]
    fn bad_inodes_are_rejected() {
        assert!(RawDinode::from_bytes(vec![0u8; 4]).is_err());
        let mut bytes = vec![0u8; 512];
        BigEndian::write_u16(&mut bytes[offset::MAGIC..], 0xdead);
        assert!(RawDinode::from_bytes(bytes).is_err());
        let mut bytes = vec![0u8; 512];
        BigEndian::write_u16(&mut bytes[offset::MAGIC..], XFS_DINODE_MAGIC);
        bytes[offset::VERSION] = 7;
        assert!(RawDinode::from_bytes(bytes).is_err());
        // A version 3 inode needs room for its version 3 fields.
        let mut bytes = vec![0u8; 128];
        BigEndian::write_u16(&mut bytes[offset::MAGIC..], XFS_DINODE_MAGIC);
        bytes[offset::VERSION] = 3;
        assert!(RawDinode::from_bytes(bytes).is_err());
    }
}
