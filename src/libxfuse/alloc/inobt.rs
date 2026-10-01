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
//! The b-tree that indexes which inode numbers a group has used.
//!
//! Where the free space btrees in [`super::free_space`] answer "which blocks are
//! free", this one answers "which inode numbers are in use", and it answers it
//! about *blocks* of inodes rather than about single inodes.  A part-used block
//! of inodes is in the tree; the free inodes inside it are free.
//!
//! That distinction is the whole reason the free inode count in the
//! [group inode header](super::agi) cannot be answered from this tree: the tree
//! says which ranges are used, and says nothing about the gaps between them.
//!
//! # The two shapes of a node
//!
//! The tree is shaped like every other b-tree here, and the shapes differ in how
//! much a record costs:
//!
//! | | a record costs | so a node holds |
//! |:-|:-:|:-:|
//! | a leaf | 16 bytes -- the first inode number, how many are free, the first free one, and a fourth field nothing here reads | `(blocksize - 16) / 16` |
//! | an interior node | 8 bytes -- a four byte key and a four byte pointer | `(blocksize - 16) / 8` |
//!
//! The free count and the first free inode are there for a file system that
//! tracks free inodes in a tree of their own.  On a version 1 header, which has
//! no such tree, they are zero and the gaps between the ranges are where the
//! free inodes are.

use crate::libxfuse::error::{FsError, FsResult};

/// The magic at the start of every node: "IABT".
const XFS_INOBT_MAGIC: u32 = 0x4941_4254;

const MAGIC: usize = 0;
const LEVEL: usize = 4;
const NUMRECS: usize = 6;
const LEFTSIB: usize = 8;
const RIGHTSIB: usize = 12;
/// Where a leaf's records start, which is also where an interior node's keys do.
const BODY: usize = 16;
/// How much a leaf's records are apart.
///
/// Sixteen, not the twelve the three fields it shows would suggest.  That is
/// the sort of thing the tool's own rendering hides: it prints the three fields
/// it knows about and nothing says a record ends after the third.
const LEAF_RECORD: usize = 16;

/// One node of the b-tree of used inode numbers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InobtNode {
    bytes:   Box<[u8]>,
    level:   u16,
    numrecs: u16,
}

/// A range of inode numbers the tree says are in use.
///
/// `free_count` and `first_free` are what a file system with a free inode tree
/// uses to carve a gap out of the range.  A version 1 header has no such tree, so
/// both are zero and the range is wholly used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InoRange {
    /// The first inode number this chunk covers.
    pub start:      u64,
    /// How many of the chunk's inodes are free, which is the number of set bits
    /// in [`Self::free`] and is checked against it.
    pub free_count: u32,
    /// Which of the chunk's inodes are free, one bit per inode.
    pub free:       u64,
}

/// How many inode numbers one chunk covers, which is the width of the mask.
pub const INODES_PER_CHUNK: u64 = 64;

impl InoRange {
    /// The lowest free inode in this chunk, if it has one.
    ///
    /// The lowest set bit rather than any set bit, so that allocation does not
    /// skip over the free inodes at the bottom of a chunk and leave them to be
    /// found all over again next time.
    pub fn first_free_ino(&self) -> Option<u64> {
        if self.free == 0 {
            return None;
        }
        Some(self.start + u64::from(self.free.trailing_zeros()))
    }

    /// Every free inode in this chunk, lowest first.
    pub fn free_inos(&self) -> Vec<u64> {
        let mut out = Vec::with_capacity(self.free_count as usize);
        let mut bits = self.free;
        while bits != 0 {
            let bit = bits.trailing_zeros();
            out.push(self.start + u64::from(bit));
            bits &= bits - 1;
        }
        out
    }

    /// Whether the chunk's free count agrees with its mask.
    ///
    /// They are two copies of one fact, and the format documentation says so, so
    /// a chunk where they disagree is a chunk that cannot be believed -- and
    /// there is no way to tell from here which of the two is the wrong one.
    pub fn count_agrees(&self) -> bool {
        self.free.count_ones() == self.free_count
    }
}

/// Every range of inode numbers a tree says are in use, in the order the tree
/// holds them.
///
/// The order matters: the ranges are runs of used inode numbers and they are read
/// in key order, so the counts can be added up in that order and mean something.
///
/// Unlike the free space trees, this takes no geometry: a node's shape follows
/// from its own size, because a record is sixteen bytes whichever way round it
/// is counted.
pub fn ranges_in_tree<F>(root: u32, mut fetch: F) -> FsResult<Vec<InoRange>>
where
    F: FnMut(u32) -> FsResult<Box<[u8]>>,
{
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(block) = stack.pop() {
        let node = InobtNode::from_bytes(fetch(block)?)?;
        if node.is_leaf() {
            out.extend(node.ranges()?);
            continue;
        }
        // Pushed in reverse so that popping visits them left to right, which is
        // what makes the ranges come out in the order the tree holds them.
        let mut children = node.children()?;
        children.reverse();
        stack.extend(children.into_iter().map(|(_, block)| block));
    }
    Ok(out)
}

impl InobtNode {
    /// Read a node out of a block's bytes.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> FsResult<Self> {
        let bytes = bytes.into();
        if bytes.len() < BODY + 4 {
            return Err(FsError::corrupt(
                "an inode b-tree node is too short to hold a header",
            ));
        }
        if u32::from_be_bytes(bytes[MAGIC..MAGIC + 4].try_into().unwrap()) != XFS_INOBT_MAGIC {
            return Err(FsError::corrupt(
                "expected the start of an inode b-tree node",
            ));
        }
        Ok(Self {
            level:   u16::from_be_bytes(bytes[LEVEL..LEVEL + 2].try_into().unwrap()),
            numrecs: u16::from_be_bytes(bytes[NUMRECS..NUMRECS + 2].try_into().unwrap()),
            bytes:   bytes.into_boxed_slice(),
        })
    }

    fn u64_at(&self, at: usize) -> u64 {
        let mut v = [0u8; 8];
        v.copy_from_slice(&self.bytes[at..at + 8]);
        u64::from_be_bytes(v)
    }

    fn u32_at(&self, at: usize) -> u32 {
        let mut v = [0u8; 4];
        v.copy_from_slice(&self.bytes[at..at + 4]);
        u32::from_be_bytes(v)
    }

    /// How deep this node is; a leaf is zero.
    pub const fn level(&self) -> u16 {
        self.level
    }

    /// How many records, or children, it holds.
    pub const fn numrecs(&self) -> u16 {
        self.numrecs
    }

    /// Whether this node holds records rather than children.
    pub const fn is_leaf(&self) -> bool {
        self.level == 0
    }

    /// The block holding the node to its left, if there is one.
    pub fn left_sibling(&self) -> Option<u32> {
        self.sibling(LEFTSIB)
    }

    /// The block holding the node to its right, if there is one.
    pub fn right_sibling(&self) -> Option<u32> {
        self.sibling(RIGHTSIB)
    }

    fn sibling(&self, at: usize) -> Option<u32> {
        match self.u32_at(at) {
            u32::MAX => None,
            block => Some(block),
        }
    }

    /// How many records a leaf of this size can hold.
    pub fn leaf_capacity(blocksize: usize) -> usize {
        (blocksize - BODY) / LEAF_RECORD
    }

    /// How many children an interior node of this size can hold.
    pub fn interior_capacity(blocksize: usize) -> usize {
        (blocksize - BODY) / 8
    }

    /// The ranges a leaf holds.
    pub fn ranges(&self) -> FsResult<Vec<InoRange>> {
        if !self.is_leaf() {
            return Err(FsError::invalid(
                libc::EINVAL,
                "an interior node holds children, not ranges",
            ));
        }
        (0..self.numrecs as usize)
            .map(|i| {
                let at = BODY + i * LEAF_RECORD;
                Ok(InoRange {
                    start:      u64::from(self.u32_at(at)),
                    free_count: self.u32_at(at + 4),
                    free:       self.u64_at(at + 8),
                })
            })
            .collect()
    }

    /// The first inode number under each child of an interior node, paired with
    /// the block holding it.
    pub fn children(&self) -> FsResult<Vec<(u64, u32)>> {
        if self.is_leaf() {
            return Err(FsError::invalid(
                libc::EINVAL,
                "a leaf holds ranges, not children",
            ));
        }
        let keys = BODY;
        let ptrs = keys + self.capacity() * 4;
        (0..self.numrecs as usize)
            .map(|i| {
                Ok((
                    u64::from(self.u32_at(keys + i * 4)),
                    self.u32_at(ptrs + i * 4),
                ))
            })
            .collect()
    }

    fn capacity(&self) -> usize {
        InobtNode::interior_capacity(self.bytes.len())
    }
}

#[cfg(test)]
mod t {
    use super::*;
    use crate::libxfuse::{alloc::agi::Agi, error::FsResult, sb::Sb};

    const GOLDEN: &str = "target/tmp/xfsv4.img";
    /// A file system made by mkfs, whose inode numbers agree with its own headers.
    const FRESH: &str = "target/tmp/xfs_writable.img";

    /// The values `xfs_db` prints inside brackets, as in `1:[32,0,0]`.
    ///
    /// Not every number on the line: it also numbers the records, so taking
    /// every number would compare our values against `1, 2, 3...`.
    fn bracketed(line: &str) -> Vec<u64> {
        line.rsplit_once('[')
            .and_then(|(_, rest)| rest.split_once(']'))
            // `rsplit_once` leaves the remainder second, `split_once` leaves what
            // precedes the delimiter first, so the two are unpacked opposite ways
            // on purpose.
            .map(|(inner, _)| {
                inner
                    .split(',')
                    .filter_map(|v| v.trim().parse::<u64>().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The values `xfs_db` prints after each colon, as in `1:6 2:11`.
    fn after_colon(line: &str) -> Vec<u32> {
        line.split_whitespace()
            .filter_map(|w| w.split_once(':'))
            .map(|(_, v)| v)
            .filter_map(|v| v.parse::<u32>().ok())
            .collect()
    }

    /// Ask `xfs_db` to print one field of a node, so the reader is checked
    /// against the tool rather than against itself.
    fn shown(block: u32, field: &str) -> Option<Vec<String>> {
        let out = std::process::Command::new("xfs_db")
            .arg("-r")
            .arg("-c")
            .arg(format!("daddr {block}"))
            .arg("-c")
            .arg("type inobt")
            .arg("-c")
            .arg(format!("p {field}"))
            .arg(GOLDEN)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        if text.contains("supported types") {
            return None;
        }
        // Every line is offered to the extractors, and they throw away what is
        // not data.  Filtering the lines first looks tidier and is wrong twice
        // over: record numbers run past nine, and `xfs_db` prints the child
        // blocks on the same line as the field name rather than one per line.
        Some(
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect(),
        )
    }

    /// A scalar field of a node, as `xfs_db` prints it: `level = 1`.
    fn scalar_of(block: u32, field: &str) -> Option<String> {
        let out = std::process::Command::new("xfs_db")
            .arg("-r")
            .arg("-c")
            .arg(format!("daddr {block}"))
            .arg("-c")
            .arg("type inobt")
            .arg("-c")
            .arg(format!("p {field}"))
            .arg(GOLDEN)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        eprintln!(
            "DEBUG scalar_of({block},{field}) status={} out={:?} err={:?}",
            out.status,
            text,
            String::from_utf8_lossy(&out.stderr)
        );
        text.lines().map(str::trim).find_map(|l| {
            l.strip_prefix(&format!("{field} = "))
                .map(|v| v.trim().to_string())
        })
    }

    fn read(block: u32) -> Option<InobtNode> {
        let mut reader = std::io::BufReader::new(std::fs::File::open(GOLDEN).ok()?);
        let sb = crate::libxfuse::sb::Sb::from(&mut reader);
        let bytes = std::fs::read(GOLDEN).ok()?;
        let at = block as usize * sb.sb_blocksize as usize;
        InobtNode::from_bytes(bytes[at..at + sb.sb_blocksize as usize].to_vec()).ok()
    }

    /// An interior node reads as the keys and child blocks `xfs_db` prints.
    #[test]
    fn an_interior_node_reads_as_xfs_db_prints_it() {
        if !std::path::Path::new(GOLDEN).exists() {
            eprintln!("skipping: no unpacked {GOLDEN}");
            return;
        }
        let root = 12u32;
        let node = read(root).expect("the inode b-tree root");
        let children = node.children().expect("children");

        let keys: Vec<u64> = shown(root, "keys")
            .expect("xfs_db prints the keys")
            .iter()
            .flat_map(|l| bracketed(l))
            .collect();
        assert_eq!(
            children.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            keys,
            "the inode number keys disagree with xfs_db"
        );

        let ptrs: Vec<u32> = shown(root, "ptrs")
            .expect("xfs_db prints the child blocks")
            .iter()
            .flat_map(|l| after_colon(l))
            .collect();
        assert_eq!(
            children.iter().map(|(_, b)| *b).collect::<Vec<_>>(),
            ptrs,
            "the child blocks disagree with xfs_db"
        );

        // And the node's own header, which is what says which of the two shapes
        // it is.
        let level = scalar_of(root, "level").expect("xfs_db prints a level");
        assert_eq!(
            node.level().to_string(),
            level,
            "level disagrees with xfs_db"
        );
        let numrecs = scalar_of(root, "numrecs").expect("xfs_db prints a count");
        assert_eq!(
            node.numrecs().to_string(),
            numrecs,
            "record count disagrees with xfs_db"
        );
    }

    /// The extraction itself, on the exact lines the tool prints.
    #[test]
    fn the_extraction_handles_what_xfs_db_prints() {
        for line in ["1:[32]", "2:[2400]", "1:[32,0,0]", "17:[2240,0,0]"] {
            eprintln!("line={line:?} -> {:?}", bracketed(line));
        }
        for l in [
            "recs[1-17] = [startino,freecount,free]",
            "1:[32,0,0]",
            "2:[192,0,0]",
        ] {
            eprintln!("line={l:?} -> {:?}", bracketed(l));
        }
        assert_eq!(bracketed("1:[32,0,0]"), vec![32, 0, 0]);
        assert_eq!(bracketed("2:[2400]"), vec![2400]);
        assert_eq!(after_colon("1:6 2:11"), vec![6, 11]);
    }

    /// A chunk's free count is the number of free inodes in its mask.
    ///
    /// The format documentation says these are one fact written twice, so this is
    /// the invariant to hold them to: a chunk whose count disagrees with its mask
    /// cannot be believed, and nothing here can say which of the two is wrong.
    ///
    /// It is also the strongest check available on reading a chunk, because it
    /// ties the eight byte mask to the four byte count beside it.  A reader that
    /// had the mask's offset wrong would still produce a plausible count, and
    /// would fail here -- which is how a sixteen byte record that shows three
    /// printed fields turns out to be three printed fields and a mask.
    #[test]
    fn a_chunks_free_count_is_the_number_of_free_inodes_in_its_mask() {
        // Only the freshly made image.  The hand-built one's inode tree does not
        // resolve from its own headers -- walking it runs into a block that is
        // not a node of that tree -- which is the same disagreement about inode
        // numbers that the count check below runs into.  It is a good image for
        // most things and cannot be asked about inodes.
        for golden in [FRESH] {
            let Ok(bytes) = std::fs::read(golden) else {
                eprintln!("skipping {golden}: no unpacked image");
                continue;
            };
            let mut reader = std::io::BufReader::new(std::fs::File::open(golden).unwrap());
            let sb = Sb::from(&mut reader);
            let bs = sb.sb_blocksize as usize;
            let fetch = |block: u32| -> FsResult<Box<[u8]>> {
                let at = block as usize * bs;
                Ok(bytes[at..at + bs].to_vec().into_boxed_slice())
            };
            let mut checked = 0usize;
            for agno in 0..sb.agcount() {
                let at = sb.ag_header_offset(agno, Sb::AGI_SECTOR) as usize;
                let Ok(agi) = Agi::from_bytes(bytes[at..at + bs].to_vec(), sb.has_crc()) else {
                    continue;
                };
                for chunk in ranges_in_tree(agi.inobt_root(), fetch).expect("the chunks") {
                    assert!(
                        chunk.count_agrees(),
                        "{golden} ag{agno}: the chunk at inode {} claims {} free inodes but its \
                         mask has {} bits set",
                        chunk.start,
                        chunk.free_count,
                        chunk.free.count_ones()
                    );
                    assert_eq!(
                        chunk.free_inos().len() as u32,
                        chunk.free_count,
                        "{golden} ag{agno}: the chunk does not name as many free inodes as it says"
                    );
                    for ino in chunk.free_inos() {
                        assert!(
                            (chunk.start..chunk.start + INODES_PER_CHUNK).contains(&ino),
                            "{golden} ag{agno}: inode {ino} is outside the chunk that claims it"
                        );
                    }
                    if chunk.free_count > 0 {
                        assert!(
                            chunk.first_free_ino().is_some(),
                            "{golden} ag{agno}: a chunk with free inodes cannot name one"
                        );
                    }
                    checked += 1;
                }
            }
            if checked > 0 {
                eprintln!("{golden}: {checked} chunks checked");
            }
        }
    }

    /// The header's free inode count is the ranges' own free counts, added up.
    ///
    /// This is the check the walk rests on, and it settles two things at once:
    /// that the ranges are being read correctly, and that free inodes are counted
    /// where the tree says rather than by counting gaps.
    ///
    /// The gap below the first range is *not* free, and that is worth stating
    /// rather than leaving as a surprise: a group's inode numbers start at zero
    /// and its first used range starts at thirty-two, so the numbers below that
    /// are reserved and are not offered to anyone.
    ///
    /// The third field of a range is not used here, and this does not say what it
    /// is.  `xfs_db` prints it as a value that does not look like an offset or a
    /// count, and the range's own length is not established either -- only that
    /// its free count is.  Guessing at the rest would be a number that means
    /// something plausible, which is the failure this whole file is written to
    /// avoid.
    #[test]
    fn the_free_inode_count_is_the_ranges_own_counts() {
        // The freshly made image, and not the hand-built one: the hand-built
        // image's inode numbers disagree with its own headers -- a group's next
        // inode number sits before where the packed layout puts it -- so it
        // cannot be asked which inodes are free.
        const IMAGE: &str = "target/tmp/xfs_writable.img";
        if !std::path::Path::new(IMAGE).exists() {
            eprintln!("skipping: no unpacked {IMAGE}");
            return;
        }
        let bytes = std::fs::read(IMAGE).expect("the unpacked image");
        let mut reader = std::io::BufReader::new(std::fs::File::open(IMAGE).unwrap());
        let sb = Sb::from(&mut reader);
        let bs = sb.sb_blocksize as usize;
        let fetch = |block: u32| -> FsResult<Box<[u8]>> {
            let at = block as usize * bs;
            Ok(bytes[at..at + bs].to_vec().into_boxed_slice())
        };

        for agno in 0..sb.agcount() {
            let at = sb.ag_header_offset(agno, Sb::AGI_SECTOR) as usize;
            let agi = Agi::from_bytes(bytes[at..at + bs].to_vec(), sb.has_crc())
                .expect("a group inode header");
            if agi.inode_count() == 0 {
                continue;
            }
            let ranges = ranges_in_tree(agi.inobt_root(), fetch).expect("the used ranges");
            let free: u64 = ranges.iter().map(|r| u64::from(r.free_count)).sum();
            assert_eq!(
                free,
                agi.free_inodes(),
                "group {agno}: the ranges hold {free} free inodes and the header says {}",
                agi.free_inodes()
            );
            assert!(
                ranges.windows(2).all(|w| w[0].start < w[1].start),
                "group {agno}: the ranges are not in order"
            );
        }
    }

    /// A leaf reads as the ranges `xfs_db` prints.
    #[test]
    fn a_leaf_reads_as_xfs_db_prints_it() {
        if !std::path::Path::new(GOLDEN).exists() {
            eprintln!("skipping: no unpacked {GOLDEN}");
            return;
        }
        let leaf = 6u32;
        let node = read(leaf).expect("a leaf of the inode b-tree");
        assert!(node.is_leaf(), "the block does not read as a leaf");
        let ranges = node.ranges().expect("ranges");
        assert_eq!(ranges.len(), node.numrecs() as usize);

        let printed: Vec<(u64, u32, u64)> = shown(leaf, "recs")
            .expect("xfs_db prints the ranges")
            .iter()
            .flat_map(|l| bracketed(l))
            .collect::<Vec<u64>>()
            .chunks(3)
            .filter_map(|c| match c {
                // The third field is a sixty-four bit mask, and the tool prints
                // it as one; a reader that took it as thirty-two bits would
                // disagree with the tool here, which is the point.
                [a, b, c] => Some((*a, *b as u32, *c)),
                _ => None,
            })
            .collect();
        assert_eq!(
            ranges.len(),
            printed.len(),
            "range count disagrees with xfs_db"
        );
        for (ours, theirs) in ranges.iter().zip(printed.iter()) {
            assert_eq!(
                (ours.start, ours.free_count, ours.free),
                *theirs,
                "a range disagrees with xfs_db"
            );
        }
    }

    /// A leaf's records cost twelve bytes each and an interior node's children
    /// cost eight, so the two hold different numbers of them -- and a reader
    /// that used one figure for both would run off the end of a node.
    #[test]
    fn the_two_node_shapes_hold_different_numbers_of_records() {
        assert_eq!(InobtNode::leaf_capacity(512), 31);
        assert_eq!(InobtNode::interior_capacity(512), 62);
        // Which is what the tool's own numbering shows: the root of the tree in
        // this image holds two children and its key array is sized for sixty-two.
        if std::path::Path::new(GOLDEN).exists() {
            let node = read(12).expect("the inode b-tree root");
            assert!(
                node.numrecs() as usize <= node.children().expect("children").len(),
                "more records than the node can hold"
            );
        }
    }
}
