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
//! Reading the btrees that say which blocks are free.
//!
//! # What this is
//!
//! A group's free space is not a bitmap.  It is a list of *runs* -- this run of
//! 400 free blocks starting here, that run of 9 starting there -- kept in a
//! B+tree, and there are two of them per group, one keyed by where each run
//! starts and one keyed by how long each run is.  The two answer the two
//! questions an allocation actually asks: "is there a run at or after this
//! block", and "is there a run at least this long".  The group header says where
//! the root of each is.
//!
//! This module reads one node of one of those btrees.  It is the piece that has
//! to be right before a single block can be allocated, because an allocator
//! that misreads a run will hand out a block that is in use, and two files will
//! overwrite each other.
//!
//! # The shape of a node
//!
//! ```text
//!  0   the magic number
//!  4   the level, as a 16-bit number
//!  6   how many records, as a 16-bit number
//!  8   the block holding the node to the left, or the null block
//! 12   the block holding the node to the right, or the null block
//! 16   ... on a file system with checksums: the log sequence number, the
//!      owner, a pad, the file system's identifier, and the checksum
//!      ... then the records, and then the keys
//! ```
//!
//! A record is where a run starts and how long it is, both 32-bit.  A key is
//! where a run starts, how long it is, and -- for a run that a btree has split
//! across nodes -- how far into the run this node begins; the last key in a node
//! is a sentinel that starts past the end of the node's last run.
//!
//! The checksum, on a file system that has one, is a CRC-32C over the whole
//! block with the checksum field read as zeroes, stored least significant byte
//! first: the same convention the superblock, the inodes, the group header and
//! the free list all use.
//!
//! # Why the header is a different size on different file systems
//!
//! A version 4 file system has no checksums and no owner, so its header ends
//! after the two sibling pointers and the records start sixteen bytes into the
//! block.  A version 5 one adds the log sequence number, the owner, the file
//! system's identifier and the checksum, and its records start fifty-six bytes
//! in.  This is not a detail that can be guessed from the block size, and the
//! two are checked against real file systems in the tests below.
//!
//! # What is here and what is not
//!
//! Here: reading a node -- its header, its records, and its checksums -- and
//! telling a leaf from an interior node.
//!
//! Not yet: walking from a root down through the interior nodes to the leaf that
//! covers a given block.  That is the next step, and it is where the *keys*
//! above start to matter, because an interior node's records are the blocks
//! holding its children rather than free runs.

use byteorder::{BigEndian, ByteOrder, LittleEndian};
use crc::{Crc, CRC_32_ISCSI};

use super::super::{
    definitions::XfsAgblock,
    error::{FsError, FsResult},
};

/// The magic of a btree of free space, keyed by where a run starts.
pub const XFS_ABTB_MAGIC: u32 = 0x4142_5442; // "ABTB"
/// The magic of a btree of free space, keyed by how long a run is.
pub const XFS_ABTC_MAGIC: u32 = 0x4142_5443; // "ABTC"
/// The version 5 spelling of [`XFS_ABTB_MAGIC`], for a tree that fits in one
/// block.
pub const XFS_AB3B_MAGIC: u32 = 0x4142_3342; // "AB3B"
/// The version 5 spelling of [`XFS_ABTC_MAGIC`], for a tree that fits in one
/// block.
pub const XFS_AB3C_MAGIC: u32 = 0x4142_3343; // "AB3C"

/// The block number that means "no block", which is how a node with no left or
/// right sibling says so.
pub const NULL_AGBLOCK: XfsAgblock = u32::MAX;

/// The bytes of one free space btree node.
mod offset {
    pub const MAGIC: usize = 0;
    pub const LEVEL: usize = 4;
    pub const NUMRECS: usize = 6;
    pub const LEFTSIB: usize = 8;
    pub const RIGHTSIB: usize = 12;
    /// Where the records start on a file system with no checksums.
    pub const RECORDS_NO_CRC: usize = 16;
    pub const LSN: usize = 16;
    pub const OWNER: usize = 24;
    pub const UUID: usize = 32;
    pub const CRC: usize = 52;
    /// Where the records start on a file system with checksums.
    pub const RECORDS: usize = 56;
}

/// The size of a record: where a run starts, and how long it is.
pub const RECORD_LEN: usize = 8;
/// The size of a key: where a run starts, how long it is, and how far into it
/// this node begins.
pub const KEY_LEN: usize = 12;

/// One run of free blocks within a group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FreeRun {
    /// The first block of the run.
    pub start: XfsAgblock,
    /// How many blocks it covers.
    pub len:   u32,
}

impl FreeRun {
    /// The block just past the end of the run.
    pub const fn end(&self) -> u64 {
        self.start as u64 + self.len as u64
    }
}

/// One node of a group's free space btree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FreeSpaceNode {
    bytes:    Box<[u8]>,
    /// Where the records start, which is decided by whether the file system has
    /// checksums rather than by anything in the block itself.
    records:  usize,
    numrecs:  u16,
    has_crc:  bool,
    /// Whether this btree is the one keyed by where a run starts, or the one
    /// keyed by how long a run is.
    by_block: bool,
}

impl FreeSpaceNode {
    /// Take ownership of a node's bytes.
    ///
    /// The magic number has to be one of the four the two free space btrees use,
    /// because a block that is not a node of one of them is not something to
    /// read a list of free blocks out of.  `by_block` says which of the two is
    /// expected, so that reading the btree keyed by run length where the one
    /// keyed by start block should have been is caught rather than used.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>, has_crc: bool, by_block: bool) -> FsResult<Self> {
        let bytes = bytes.into();
        if bytes.len() < offset::RECORDS_NO_CRC + RECORD_LEN {
            return Err(FsError::corrupt("free space btree node is too short"));
        }
        let magic = BigEndian::read_u32(&bytes[offset::MAGIC..]);
        let wanted = if by_block {
            [XFS_ABTB_MAGIC, XFS_AB3B_MAGIC]
        } else {
            [XFS_ABTC_MAGIC, XFS_AB3C_MAGIC]
        };
        if !wanted.contains(&magic) {
            return Err(FsError::corrupt(format!(
                "expected the {} free space btree and found magic {magic:#010x}",
                if by_block {
                    "start block"
                } else {
                    "run length"
                }
            )));
        }
        let numrecs = BigEndian::read_u16(&bytes[offset::NUMRECS..]);
        let records = if has_crc {
            offset::RECORDS
        } else {
            offset::RECORDS_NO_CRC
        };
        if records + RECORD_LEN * numrecs as usize > bytes.len() {
            return Err(FsError::corrupt(format!(
                "free space btree node says it holds {numrecs} records but the block is too small"
            )));
        }
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
            records,
            numrecs,
            has_crc,
            by_block,
        })
    }

    /// The node's bytes, as they are on the image.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// How deep in the tree this node is; 0 means it holds the runs themselves.
    pub fn level(&self) -> u16 {
        BigEndian::read_u16(&self.bytes[offset::LEVEL..])
    }

    /// Is this a leaf, holding the runs of free blocks?
    pub fn is_leaf(&self) -> bool {
        self.level() == 0
    }

    /// How many records the node holds.
    pub const fn numrecs(&self) -> u16 {
        self.numrecs
    }

    /// The block holding the node to the left, or [`NULL_AGBLOCK`].
    pub fn left_sibling(&self) -> XfsAgblock {
        BigEndian::read_u32(&self.bytes[offset::LEFTSIB..])
    }

    /// The block holding the node to the right, or [`NULL_AGBLOCK`].
    pub fn right_sibling(&self) -> XfsAgblock {
        BigEndian::read_u32(&self.bytes[offset::RIGHTSIB..])
    }

    /// The log sequence number of the last change to this node.
    pub fn lsn(&self) -> u64 {
        if self.has_crc {
            BigEndian::read_u64(&self.bytes[offset::LSN..])
        } else {
            0
        }
    }

    /// Is this the btree keyed by where a run starts?
    pub const fn is_by_block(&self) -> bool {
        self.by_block
    }

    /// The free runs this node holds.
    ///
    /// Only meaningful for a leaf: in an interior node the same records hold the
    /// blocks holding the children, and are not free runs.
    pub fn runs(&self) -> FsResult<Vec<FreeRun>> {
        if !self.is_leaf() {
            return Err(FsError::invalid(
                libc::EINVAL,
                "this node holds subtrees, not free runs",
            ));
        }
        let mut out = Vec::with_capacity(self.numrecs as usize);
        for i in 0..self.numrecs as usize {
            let at = self.records + RECORD_LEN * i;
            let start = BigEndian::read_u32(&self.bytes[at..]);
            let len = BigEndian::read_u32(&self.bytes[at + 4..]);
            if start == NULL_AGBLOCK {
                return Err(FsError::corrupt("a free run starts at the null block"));
            }
            out.push(FreeRun { start, len });
        }
        Ok(out)
    }

    /// The blocks this node points at, which is what an interior node's records
    /// hold: the first field of each record is the block, and the second is
    /// meaningless there.
    pub fn children(&self) -> FsResult<Vec<XfsAgblock>> {
        if self.is_leaf() {
            return Err(FsError::invalid(
                libc::EINVAL,
                "a leaf holds free runs, not subtrees",
            ));
        }
        let mut out = Vec::with_capacity(self.numrecs as usize);
        for i in 0..self.numrecs as usize {
            let at = self.records + RECORD_LEN * i;
            out.push(BigEndian::read_u32(&self.bytes[at..]));
        }
        Ok(out)
    }

    /// Is the node's checksum correct?
    ///
    /// A file system without checksums has none to check, and this reports
    /// those as correct rather than as damaged.
    pub fn verify_crc(&self) -> bool {
        if !self.has_crc {
            return true;
        }
        self.stored_crc() == self.computed_crc()
    }

    fn stored_crc(&self) -> u32 {
        LittleEndian::read_u32(&self.bytes[offset::CRC..])
    }

    fn computed_crc(&self) -> u32 {
        const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);
        let mut copy = self.bytes.to_vec();
        LittleEndian::write_u32(&mut copy[offset::CRC..], 0);
        CASTAGNOLI.checksum(&copy)
    }

    /// Recompute the node's checksum, if it has one.
    pub fn update_crc(&mut self) {
        if !self.has_crc {
            return;
        }
        let crc = self.computed_crc();
        LittleEndian::write_u32(&mut self.bytes[offset::CRC..], crc);
    }
}

#[cfg(test)]
mod t {
    use super::*;

    /// The first 176 bytes of the free space btree of the first group of
    /// `resources/xfsv4.img`, which is a leaf in a version 4 file system: its
    /// header and all nineteen of its records.  The rest of that block is keys
    /// and unused space.
    ///
    /// What the runs in it are is not a guess: `xfs_db`'s `freesp -a 0 -d`
    /// prints the same runs for the same group, and the expectations below are
    /// those.
    const V4_NODE: &str = concat!(
        "41 42 54 42 00 00 00 13 ff ff ff ff ff ff ff ff ",
        "00 00 00 0d 00 00 00 03 00 00 02 39 00 00 00 07 ",
        "00 00 05 c9 00 00 00 07 00 00 08 e8 00 00 00 18 ",
        "00 00 09 20 00 00 00 08 00 00 09 30 00 00 00 50 ",
        "00 00 09 a8 00 00 00 18 00 00 09 e0 00 00 00 18 ",
        "00 00 0a 00 00 00 00 48 00 00 0a 70 00 00 00 20 ",
        "00 00 0a b0 00 00 00 08 00 00 0a b9 00 00 00 08 ",
        "00 00 0a c9 00 00 00 67 00 00 0b 50 00 00 00 08 ",
        "00 00 0b 60 00 00 00 48 00 00 0b d0 00 00 00 20 ",
        "00 00 0c 10 00 00 00 18 00 00 0c 30 00 00 00 50 ",
        "00 00 0c a8 00 00 73 58 ",
    );

    /// The first 96 bytes of the free space btree of the first group of
    /// `resources/xfs_4kn.img`: a leaf in a version 5 file system, with the
    /// longer header and five records.
    const V5_NODE: &str = concat!(
        "41 42 33 42 00 00 00 05 ff ff ff ff ff ff ff ff ",
        "00 00 00 00 00 00 00 20 00 00 00 01 00 00 00 08 ",
        "8d 0c 39 d3 96 de 47 ef a4 76 1c 07 14 0c b9 36 ",
        "00 00 00 00 4b ed 12 94 00 00 00 0d 00 00 00 02 ",
        "00 00 00 19 00 00 00 01 00 00 00 1b 00 00 00 01 ",
        "00 00 00 20 00 00 00 01 00 00 00 22 00 00 0f de ",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ",
        "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ",
        "00 00 00 00 00 00 00 00 ",
    );

    fn block_of(hex_rows: &str, blocksize: usize) -> Vec<u8> {
        let mut out: Vec<u8> = hex_rows
            .split_whitespace()
            .map(|byte| u8::from_str_radix(byte, 16).expect("two hex digits per byte"))
            .collect();
        out.resize(blocksize, 0);
        out
    }

    /// A real version 4 leaf, decoded, must produce the runs that `xfs_db`
    /// reports for its group.
    #[test]
    fn real_v4_leaf_lists_its_runs() {
        let node = FreeSpaceNode::from_bytes(block_of(V4_NODE, 512), false, true).unwrap();
        assert!(node.is_leaf());
        assert_eq!(node.level(), 0);
        assert_eq!(node.numrecs(), 19);
        assert_eq!(node.left_sibling(), NULL_AGBLOCK);
        assert_eq!(node.right_sibling(), NULL_AGBLOCK);
        assert!(
            node.verify_crc(),
            "a version 4 node has no checksum to be wrong about"
        );
        assert!(node.is_by_block());

        let runs = node.runs().unwrap();
        assert_eq!(runs.len(), 19);
        assert_eq!(
            runs[0],
            FreeRun {
                start: 13,
                len:   3,
            }
        );
        assert_eq!(
            runs[1],
            FreeRun {
                start: 569,
                len:   7,
            }
        );
        assert_eq!(
            runs[2],
            FreeRun {
                start: 1481,
                len:   7,
            }
        );
        // The group's longest run, which the group header also reports.
        assert_eq!(
            runs[18],
            FreeRun {
                start: 3240,
                len:   29528,
            }
        );
        assert_eq!(
            runs[18].end(),
            32768,
            "the last run reaches the end of the group"
        );
    }

    /// The same for a version 5 leaf, whose header is longer.
    #[test]
    fn real_v5_leaf_lists_its_runs() {
        let node = FreeSpaceNode::from_bytes(block_of(V5_NODE, 4096), true, true).unwrap();
        assert!(node.is_leaf());
        assert_eq!(node.numrecs(), 5);
        assert!(node.verify_crc(), "a real version 5 node must verify");
        assert_eq!(node.lsn(), 0x20);

        let runs = node.runs().unwrap();
        assert_eq!(
            runs,
            vec![
                FreeRun {
                    start: 13,
                    len:   2,
                },
                FreeRun {
                    start: 25,
                    len:   1,
                },
                FreeRun {
                    start: 27,
                    len:   1,
                },
                FreeRun {
                    start: 32,
                    len:   1,
                },
                FreeRun {
                    start: 34,
                    len:   4062,
                },
            ]
        );
    }

    /// A block that is not a node of the expected btree has to be refused, and
    /// so does a node read as the wrong one of the two.
    #[test]
    fn the_wrong_block_is_refused() {
        // The version 4 leaf, read as the btree keyed by run length.
        let err = FreeSpaceNode::from_bytes(block_of(V4_NODE, 512), false, false).unwrap_err();
        assert!(matches!(err, FsError::Corrupt { .. }));
        // A block bitmap, which is another thing a group header points at.
        let err = FreeSpaceNode::from_bytes(vec![0x42; 512], false, true).unwrap_err();
        assert!(matches!(err, FsError::Corrupt { .. }));
        // Too short to hold a record.
        assert!(FreeSpaceNode::from_bytes(vec![0u8; 8], false, true).is_err());
    }

    /// A node that claims more records than its block can hold is damaged, and
    /// has to be caught rather than read past the end of the block.
    #[test]
    fn a_node_that_claims_too_many_records_is_refused() {
        let mut bytes = block_of(V4_NODE, 512);
        BigEndian::write_u16(&mut bytes[6..], 4000);
        assert!(FreeSpaceNode::from_bytes(bytes, false, true).is_err());
    }

    /// An interior node's records are the blocks holding its children, which are
    /// not free runs, and asking for runs has to say so.
    #[test]
    fn an_interior_node_holds_subtrees() {
        let mut bytes = block_of(V4_NODE, 512);
        BigEndian::write_u16(&mut bytes[4..], 1); // level 1
        let node = FreeSpaceNode::from_bytes(bytes, false, true).unwrap();
        assert!(!node.is_leaf());
        let err = node.runs().unwrap_err();
        assert_eq!(err.errno(), libc::EINVAL);
        assert_eq!(node.children().unwrap().len(), 19);
        // And a leaf is not a subtree.
        let leaf = FreeSpaceNode::from_bytes(block_of(V4_NODE, 512), false, true).unwrap();
        assert_eq!(leaf.children().unwrap_err().errno(), libc::EINVAL);
    }

    /// A version 5 node's checksum has to be caught when it changes, and
    /// recomputed when it is rewritten.
    #[test]
    fn a_version_5_node_checksums() {
        let mut bytes = block_of(V5_NODE, 4096);
        assert!(FreeSpaceNode::from_bytes(bytes.clone(), true, true)
            .unwrap()
            .verify_crc());
        // A record, in the middle of the block.
        bytes[60] ^= 0x01;
        assert!(!FreeSpaceNode::from_bytes(bytes.clone(), true, true)
            .unwrap()
            .verify_crc());
        // Recomputing must make it verify again, since that is what writing the
        // node back does.
        let mut node = FreeSpaceNode::from_bytes(bytes, true, true).unwrap();
        node.update_crc();
        assert!(node.verify_crc());
    }
}
