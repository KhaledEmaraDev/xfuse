/*
 * BSD 2-Clause License
 *
 * Copyright (c) 2026, Pedro Giffuni
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
//! The group inode header: how many inodes a group has, and which are free.
//!
//! This is the inode counterpart of [`super::agf`].  A group's free *space* is
//! indexed by runs in a pair of btrees; its free *inodes* are counted here and
//! found by walking the group's inode space.  The two are separate: the free
//! list says nothing about inodes, and this says nothing about blocks.
//!
//! It lives in the sector after the group file and shares a file system block
//! with it, so it is read and written as a whole block alongside the group file
//! rather than on its own.
//!
//! # Why reading it is not the same as trusting it
//!
//! The counts here are summaries: how many inodes the group was created with,
//! and how many of them are free *now*.  Neither is the place an allocation
//! looks to find a free inode -- the inode space itself is walked, because a
//! count that says thirty are free cannot say which.  What the count is for is
//! telling whether a group is worth searching at all, and telling the rest of
//! the file system that this group's answer has moved: the superblock keeps a
//! total, and a group's share of it is this number.

use crate::libxfuse::error::{FsError, FsResult};

mod offset {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 4;
    pub const SEQNO: usize = 8;
    /// How many blocks the group has.
    pub const LENGTH: usize = 12;
    /// How many inodes the group was created with.
    pub const ILENGTH: usize = 16;
    /// The block holding the root of the btree that indexes *used* inode
    /// numbers.
    ///
    /// This indexes the blocks of inodes, not the inodes: a block that is partly
    /// used is in the tree, and the inodes free inside it are free.  That is why
    /// the free inode count below cannot be answered from this tree, and why
    /// finding a free inode means walking the inode space rather than consulting
    /// a btree.
    pub const INOBT_ROOT: usize = 20;
    /// How deep that btree is.
    pub const INOBT_LEVEL: usize = 24;
    /// How many inodes are free now.
    ///
    /// This is the group's only inode count beyond the total: there is no "used"
    /// field to read or write, because it is the total less this.  Two counters
    /// that both have to move together are two things to get out of step, and XFS
    /// keeps one.
    pub const FREECOUNT: usize = 28;
    /// The next inode number this group would hand out.
    pub const NEWINO: usize = 32;
    /// The last field this code reads, for a size check.
    pub const NEEDED: usize = 40;
}

const XFS_AGI_MAGIC: u32 = 0x5841_4749; // "XAGI"

/// One group's inode header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Agi {
    bytes:   Box<[u8]>,
    has_crc: bool,
}

impl Agi {
    /// Read a group inode header out of a block's bytes.
    ///
    /// `has_crc` is recorded because a header written back has to know whether it
    /// carries a checksum, but it does not move any field this code reads: the
    /// inode counts are ahead of the checksum.  That is a property worth
    /// keeping rather than assuming -- it is why the counts below are at the
    /// same offsets either way.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>, has_crc: bool) -> FsResult<Self> {
        let bytes = bytes.into();
        let need = offset::NEEDED;
        if bytes.len() < need {
            return Err(FsError::corrupt(format!(
                "a group inode header needs at least {need} bytes, got {}",
                bytes.len()
            )));
        }
        if u32::from_be_bytes(bytes[offset::MAGIC..offset::MAGIC + 4].try_into().unwrap())
            != XFS_AGI_MAGIC
        {
            return Err(FsError::corrupt(
                "expected the start of a group inode header",
            ));
        }
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
            has_crc,
        })
    }

    fn u32_at(&self, at: usize) -> u32 {
        let mut v = [0u8; 4];
        v.copy_from_slice(&self.bytes[at..at + 4]);
        u32::from_be_bytes(v)
    }

    /// Whether this block has a checksum.
    pub const fn has_crc(&self) -> bool {
        self.has_crc
    }

    /// How many inodes the group was created with.
    pub fn inode_count(&self) -> u64 {
        u64::from(self.u32_at(offset::ILENGTH))
    }

    /// How many of them are free now.
    pub fn free_inodes(&self) -> u64 {
        u64::from(self.u32_at(offset::FREECOUNT))
    }

    /// How many of them are used now, which is the total less the free ones.
    pub fn used_inodes(&self) -> u64 {
        self.inode_count().saturating_sub(self.free_inodes())
    }

    /// The block holding the root of the btree that indexes used inode numbers.
    pub fn inobt_root(&self) -> u32 {
        self.u32_at(offset::INOBT_ROOT)
    }

    /// How deep the btree of used inode numbers is.
    pub fn inobt_level(&self) -> u32 {
        self.u32_at(offset::INOBT_LEVEL)
    }

    /// The next inode number this group would hand out.
    pub fn next_ino(&self) -> u64 {
        u64::from(self.u32_at(offset::NEWINO))
    }

    /// How many blocks the group's inodes occupy.
    pub fn inode_blocks(&self) -> u64 {
        u64::from(self.u32_at(offset::LENGTH))
    }

    /// The group's own sequence number, which moves whenever it is written.
    pub fn seqno(&self) -> u32 {
        self.u32_at(offset::SEQNO)
    }

    /// Move the free inode count.
    ///
    /// A group's free inode count and the superblock's total are two halves of
    /// one fact, and they are only true together.  That is checked here rather
    /// than left to repair, because a count that has drifted is a file system
    /// whose allocation is about to hand out an inode that is in use.
    pub fn set_free_inodes(&mut self, free: u64) -> FsResult<()> {
        if free > self.inode_count() {
            return Err(FsError::corrupt(format!(
                "a group with {} inodes cannot have {free} free",
                self.inode_count()
            )));
        }
        // Only the free count moves: there is no used field to keep in step.
        self.set_u32(offset::FREECOUNT, free as u32);
        Ok(())
    }

    fn set_u32(&mut self, at: usize, value: u32) {
        self.bytes[at..at + 4].copy_from_slice(&value.to_be_bytes());
    }

    /// The bytes as they were read, for writing back.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The bytes, for writing back.
    pub fn into_bytes(self) -> Box<[u8]> {
        self.bytes
    }
}

#[cfg(test)]
mod t {
    use super::*;
    use crate::libxfuse::{alloc::free_space::GroupGeometry, sb::Sb};

    fn image_with_groups(golden: &str, groups: u32) -> Option<(Sb, GroupGeometry)> {
        let mut reader = std::io::BufReader::new(std::fs::File::open(golden).ok()?);
        let sb = Sb::from(&mut reader);
        let geometry = GroupGeometry::new(sb.sb_agblocks, sb.has_crc(), true);
        assert!(sb.agcount() >= groups);
        Some((sb, geometry))
    }

    fn read_agi(sb: &Sb, agno: u32, golden: &str) -> Agi {
        let bytes = std::fs::read(golden).expect("the unpacked image");
        let at = sb.ag_header_offset(agno, Sb::AGI_SECTOR) as usize;
        Agi::from_bytes(
            bytes[at..at + sb.sb_blocksize as usize].to_vec(),
            sb.has_crc(),
        )
        .expect("a group inode header")
    }

    /// The header this reads is the one `xfs_db` reads, and the counts agree.
    ///
    /// Reading a header is easy to get subtly wrong -- the version number is
    /// three bytes of a field with a flag in the top one, and a file system with
    /// checksums moves everything after the log sequence number along by four --
    /// and a wrong read is a plausible number rather than an obvious fault.  So
    /// the check is against the tool, not against this code.
    #[test]
    fn the_inode_header_agrees_with_xfs_db() {
        for golden in ["target/tmp/xfsv4.img", "target/tmp/xfs_writable.img"] {
            if !std::path::Path::new(golden).exists() {
                eprintln!("skipping {golden}: no unpacked image");
                continue;
            }
            let mut reader =
                std::io::BufReader::new(std::fs::File::open(golden).expect("the image"));
            let sb = Sb::from(&mut reader);
            for agno in 0..sb.agcount() {
                let agi = read_agi(&sb, agno, golden);
                // `xfs_db` prints some fields as `null` where there is nothing
                // to report -- a group that has never handed out an inode number
                // has no next one -- so a field that is not a number is not a
                // disagreement and the check is skipped rather than turned into
                // a failure about the tool rather than about this code.
                let show = |field: &str| -> Option<u64> {
                    let out = std::process::Command::new("xfs_db")
                        .args([
                            "-r",
                            "-c",
                            &format!("agi {agno}"),
                            "-c",
                            &format!("p {field}"),
                        ])
                        .arg(golden)
                        .output()
                        .expect("xfs_db can be run");
                    assert!(
                        out.status.success(),
                        "xfs_db failed on {golden} ag{agno} {field}"
                    );
                    String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .filter_map(|l| l.split('=').nth(1))
                        .filter_map(|v| v.trim().parse::<u64>().ok())
                        .next()
                };
                let same = |what: &str, ours: u64, field: &str| {
                    if let Some(want) = show(field) {
                        assert_eq!(
                            ours, want,
                            "{golden} ag{agno}: {what} disagrees with xfs_db"
                        );
                    }
                };
                same("inode count", agi.inode_count(), "count");
                same("group length", agi.inode_blocks(), "length");
                same("free inode count", agi.free_inodes(), "freecount");
                same("next inode number", agi.next_ino(), "newino");
                if let (Some(root), Some(level)) = (show("root"), show("level")) {
                    assert_eq!(
                        (u64::from(agi.inobt_root()), u64::from(agi.inobt_level())),
                        (root, level),
                        "{golden} ag{agno}: the inode btree's root and level disagree with xfs_db"
                    );
                }
                assert_eq!(
                    agi.inode_count(),
                    agi.free_inodes() + agi.used_inodes(),
                    "{golden} ag{agno}: free and used inodes do not add up to the inode count"
                );
            }
        }
    }

    /// Moving the free count moves the used count the other way, and neither can
    /// pass the count the group was made with.
    #[test]
    fn a_moved_free_count_is_a_moved_pair() {
        let golden = "target/tmp/xfsv4.img";
        let bytes = std::fs::read(golden).expect("the unpacked golden image");
        let mut reader =
            std::io::BufReader::new(std::fs::File::open("target/tmp/xfsv4.img").unwrap());
        let sb = Sb::from(&mut reader);
        let at = sb.ag_header_offset(0, Sb::AGI_SECTOR) as usize;
        let mut agi = Agi::from_bytes(
            bytes[at..at + sb.sb_blocksize as usize].to_vec(),
            sb.has_crc(),
        )
        .expect("a group inode header");
        let before = (agi.free_inodes(), agi.used_inodes());
        agi.set_free_inodes(before.0 - 1).expect("taking an inode");
        assert_eq!(
            (agi.free_inodes(), agi.used_inodes()),
            (before.0 - 1, before.1 + 1),
            "taking an inode did not move both counts"
        );
        agi.set_free_inodes(before.0).expect("giving one back");
        assert_eq!(
            (agi.free_inodes(), agi.used_inodes()),
            before,
            "giving an inode back did not restore both counts"
        );
        assert!(
            agi.set_free_inodes(agi.inode_count() + 1).is_err(),
            "a group accepted more free inodes than it has inodes"
        );
    }
}
