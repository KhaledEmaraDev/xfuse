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
//! The free list of one allocation group: a flat array of blocks that are known
//! to be free.
//!
//! # What this is
//!
//! Searching a btree for a free block is more work than most allocations
//! deserve, so each group keeps a *free list*: a fixed-size array of block
//! numbers in one block of the group, holding blocks that the group's free space
//! btrees say are free.  Allocating from it is reading an entry; refilling it
//! is a btree walk, done once for many allocations.
//!
//! # Where it is
//!
//! The free list is the *third sector* of its group, after the group file and
//! the group inode header.  See
//! [`Sb::ag_header_offset`](super::super::sb::Sb::ag_header_offset).
//!
//! # Its shape
//!
//! ```text
//!  0   the magic number
//!  4   the group's sequence number
//!  8   the file system's identifier
//! 24   the log sequence number of the last change
//! 28   the checksum, on a file system that has checksums
//! 32   the block numbers, one per entry
//! ```
//!
//! The array is as long as the file system block allows, and the group header
//! says which slice of it is live: `flfirst ..= fllast`, holding `flcount`
//! entries.  Slots outside that window are the
//! [null block](super::alloc::agf::NULL_AGBLOCK), which is how the tail of the
//! array is distinguished from real blocks.
//!
//! # Invariants
//!
//! * A block in the live window must really be free.  A free list is only as
//!   good as that: a block that is in use and also in the free list will be
//!   handed out twice, and two files will overwrite each other.
//! * Taking a block shrinks the window from the front; giving one back grows it
//!   at the back.  The window is a queue, and moving either end is three fields
//!   in the group header.
//! * When the window is full it cannot grow, and a block returned to it has
//!   nowhere to go.  That is the point at which the free space btrees have to be
//!   updated instead, which is a later phase.
//!
//! # Modifying it
//!
//! Changes go through a [`Transaction`](super::super::transaction::Transaction)
//! like every other metadata write, and the group header's window is changed in
//! the same transaction, because a free list entry without its window, or a
//! window without its entry, is a group that will hand out the same block twice.

use byteorder::{BigEndian, ByteOrder, LittleEndian};
use crc::{Crc, CRC_32_ISCSI};

use super::{
    super::{
        definitions::{XfsAgblock, XfsFsblock},
        error::{no_entry, FsError, FsResult},
    },
    agf::NULL_AGBLOCK,
};

/// The magic number that opens a free list.
pub const XFS_AGFL_MAGIC: u32 = 0x5841_464c; // "XAFL"

/// Byte offsets of the free list's header.
mod offset {
    pub const MAGIC: usize = 0;
    pub const SEQNO: usize = 4;
    pub const UUID: usize = 8;
    pub const LSN: usize = 24;
    /// The checksum, on a file system that has one.  It follows the log
    /// sequence number, and it is what pushes the array down by a slot: a file
    /// system with checksums has four more header bytes than one without, and
    /// the array starts after the header either way.
    pub const CRC: usize = 32;
    /// Where the array of block numbers begins on a file system that has no
    /// checksum.
    pub const ARRAY_NO_CRC: usize = 32;
    /// Where it begins on a file system that has one.
    pub const ARRAY: usize = 36;
}

/// One allocation group's free list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Agfl {
    bytes:      Box<[u8]>,
    has_crc:    bool,
    has_header: bool,
    /// Where the array of block numbers begins, which is one slot later on a
    /// file system that has a checksum in the header.
    array_at:   usize,
    entries:    u32,
}

impl Agfl {
    /// Take ownership of a free list's bytes.
    ///
    /// A free list does not have to have been written yet: a group that has
    /// never run out of free blocks may hold a block that is still free space
    /// and was never given a free list.  Such a block does not open with the
    /// magic number, and that is not an error -- it is an empty free list, and
    /// the caller refills it from the free space btrees.  The magic number is
    /// still recorded so that a free list that *has* been written can be
    /// recognised, and so that one can be created in a block that has not.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>, has_crc: bool) -> FsResult<Self> {
        let bytes = bytes.into();
        // The array starts after the header, and a file system with checksums
        // has four more header bytes than one without.
        // Whether the block carries a free list header decides where its array
        // starts, and there are both kinds: one with a header, and one that is
        // nothing but the array.
        //
        // **The file systems here are the second kind.**  A block of a group
        // with a header starts with the magic; these blocks hold a bare array of
        // block numbers, and the tool reports them as one.  Assuming a header
        // where there is none puts the array thirty-two bytes too far in, which
        // is not a subtle corruption: the header's own magic ends up being read
        // as entry zero, and the header's sequence number as entry one.
        let has_header = bytes.len() >= 4
            && u32::from_be_bytes(bytes[offset::MAGIC..offset::MAGIC + 4].try_into().unwrap())
                == XFS_AGFL_MAGIC;
        let array_at = match (has_header, has_crc) {
            (true, true) => offset::ARRAY,
            (true, false) => offset::ARRAY_NO_CRC,
            // No header, so the array is the whole block.
            (false, _) => 0,
        };
        if bytes.len() < array_at + 4 {
            return Err(FsError::corrupt(format!(
                "an allocation group free list needs at least {} bytes, got {}",
                array_at + 4,
                bytes.len()
            )));
        }
        let entries = ((bytes.len() - array_at) / 4) as u32;
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
            has_crc,
            has_header,
            array_at,
            entries,
        })
    }

    /// The free list's bytes, as they are on the image.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consume the free list and take its bytes.
    pub fn into_bytes(self) -> Box<[u8]> {
        self.bytes
    }

    /// How many block numbers the array can hold.
    /// How many entries can be live at once.
    ///
    /// The whole array.  Giving a block back refuses to step past the end, so a
    /// list that has reached this many entries is full and refuses the next one --
    /// which is what `a_full_free_list_refuses_a_returned_block` checks by
    /// filling until one is refused.
    pub const fn capacity(&self) -> u32 {
        self.entries
    }

    /// Does this block hold a free list that has been written?
    ///
    /// A group that has never had to record a free block leaves its free list
    /// block alone, so a block with no magic number is one that has to be
    /// treated as empty rather than as damaged.
    pub fn is_written(&self) -> bool {
        BigEndian::read_u32(&self.bytes[offset::MAGIC..]) == XFS_AGFL_MAGIC
    }

    /// Write the header that says this block is a free list.
    ///
    /// The array is blanked to the null block at the same time, so that a block
    /// that used to hold something else cannot be read as free blocks.
    /// Write the header that says this block is a free list, and empty it.
    ///
    /// A block that has no header gets no header written to it.  The array is the
    /// whole block there, so writing a magic at the front would put the header
    /// where entry zero belongs -- which is exactly what makes a list unreadable
    /// rather than merely wrong.
    pub fn initialise(&mut self, seqno: u32, uuid: &[u8; 16]) {
        if self.has_header {
            BigEndian::write_u32(&mut self.bytes[offset::MAGIC..], XFS_AGFL_MAGIC);
            BigEndian::write_u32(&mut self.bytes[offset::SEQNO..], seqno);
            self.bytes[offset::UUID..offset::UUID + 16].copy_from_slice(uuid);
        }
        self.blank_array();
    }

    /// Blank every slot above `last`, so a block being written as a list again
    /// does not carry its old contents forward as free blocks.
    pub fn blank_beyond(&mut self, last: u32) {
        for i in last.saturating_add(1)..self.entries {
            self.set_entry(i, NULL_AGBLOCK);
        }
    }

    /// Whether this block carries a free list header.
    pub const fn has_header(&self) -> bool {
        self.has_header
    }

    /// Whether a slot holds a block that could actually be used.
    ///
    /// Not the null block, and not zero either.  A free list block that was
    /// never written reads as zeroes, and a zero entry is *block 0* -- the very
    /// first block of the group, which holds its headers and is never free space.
    /// Believing one is how the superblock ends up being handed out as a b-tree
    /// node.
    pub fn holds_block(&self, i: u32) -> bool {
        self.entry(i) != NULL_AGBLOCK && self.entry(i) != 0
    }

    /// Set every slot of the array to the null block.
    pub fn blank_array(&mut self) {
        for i in 0..self.entries {
            self.set_entry(i, NULL_AGBLOCK);
        }
    }

    fn set_entry(&mut self, i: u32, block: XfsAgblock) {
        let at = self.array_at + 4 * i as usize;
        BigEndian::write_u32(&mut self.bytes[at..], block);
    }

    /// The block number in slot `i`, or the null block if the slot is empty.
    pub fn entry(&self, i: u32) -> XfsAgblock {
        if i >= self.entries {
            return NULL_AGBLOCK;
        }
        let at = self.array_at + 4 * i as usize;
        BigEndian::read_u32(&self.bytes[at..])
    }

    /// The number of live slots, from the group header.
    ///
    /// The free list does not record its own length: the group header's window
    /// does, and that is where an allocator looks, because that is also what
    /// has to be changed when the window moves.
    pub fn window(&self, first: u32, last: u32, count: u32) -> AgflWindow {
        AgflWindow { first, last, count }
    }

    /// Take the block at the front of the window.
    ///
    /// Returns the block, and the window that is left.  The window is not
    /// applied: the caller writes it to the group header in the same
    /// transaction, because a free list that has lost a block while its header
    /// still offers it will hand the same block out again.
    pub fn take_front(&mut self, window: &mut AgflWindow) -> FsResult<XfsAgblock> {
        if window.count == 0 || window.first > window.last {
            return Err(no_entry(b"the group free list is empty"));
        }
        if window.first >= self.entries {
            return Err(FsError::Corrupt {
                what: format!(
                    "the group free list window starts at slot {}, past its {} slots",
                    window.first, self.entries
                ),
            });
        }
        let block = self.entry(window.first);
        if block == NULL_AGBLOCK {
            return Err(FsError::Corrupt {
                what: "the group free list has a null block in its live window".into(),
            });
        }
        self.set_entry(window.first, NULL_AGBLOCK);
        window.first += 1;
        window.count -= 1;
        Ok(block)
    }

    /// Put a block at the back of the window.
    ///
    /// Returns the window that is left.  As with taking a block, the caller
    /// writes the window to the group header in the same transaction.
    pub fn give_back(
        &mut self,
        window: &mut AgflWindow,
        block: XfsAgblock,
    ) -> FsResult<AgflWindow> {
        if block == NULL_AGBLOCK {
            return Err(FsError::invalid(
                libc::EINVAL,
                "the null block is not a block to free",
            ));
        }
        if window.count == 0 {
            // An empty window starts over at the bottom of the array: there is
            // no reason to keep a window that has been emptied by taking, and
            // reusing the array from its start is how the group gets its free
            // list back after a burst of allocation.
            window.first = 0;
            window.last = 0;
        } else if window.last + 1 >= self.entries {
            return Err(FsError::NoSpace);
        } else {
            window.last += 1;
        }
        if window.first > window.last {
            window.first = window.last;
        }
        self.set_entry(window.last, block);
        window.count += 1;
        Ok(*window)
    }

    /// Is the free list's checksum correct?
    ///
    /// A block that has never been written as a free list has no checksum, and
    /// reports as correct rather than as damaged, for the same reason
    /// [`Agfl::is_written`] exists.
    pub fn verify_crc(&self) -> bool {
        if !self.has_crc || !self.is_written() {
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

    /// Recompute the free list's checksum, if it has one.
    pub fn update_crc(&mut self) {
        if !self.has_crc || !self.is_written() {
            return;
        }
        let crc = self.computed_crc();
        LittleEndian::write_u32(&mut self.bytes[offset::CRC..], crc);
    }

    /// The blocks in a window, ignoring the null block.
    pub fn window_blocks(&self, window: &AgflWindow) -> Vec<XfsAgblock> {
        (window.first..=window.last)
            .map(|i| self.entry(i))
            .filter(|b| *b != NULL_AGBLOCK)
            .collect()
    }
}

/// The live part of a group's free list, as the group header records it.
///
/// The three numbers describe one queue, and are kept together so that they
/// cannot be changed one at a time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgflWindow {
    /// The first live slot.
    pub first: u32,
    /// The last live slot, inclusive.
    pub last:  u32,
    /// How many slots are live.
    pub count: u32,
}

impl AgflWindow {
    /// An empty window.
    pub const fn empty() -> Self {
        AgflWindow {
            first: 0,
            last:  0,
            count: 0,
        }
    }
}

/// A file system block number for a block named within its group.
pub const fn agfl_block_to_fsb(agno: u32, agblocks: u32, block: XfsAgblock) -> XfsFsblock {
    (agno as u64 * agblocks as u64 + block as u64) as XfsFsblock
}

#[cfg(test)]
mod t {
    use super::*;

    /// The first 56 bytes of the free list in the first group of
    /// `resources/xfs_4kn.img`: its header, its checksum, and the five slots
    /// the group's live window covers.  What `xfs_db` prints for the same block is
    /// `bno[0]=null 1:9 2:10 3:11 4:12`, and the expectations below come from
    /// there, so this code is being checked against the reference
    /// implementation rather than against itself.
    const V5_AGFL: &str = concat!(
        "58 41 46 4c 00 00 00 00 8d 0c 39 d3 96 de 47 ef ",
        "a4 76 1c 07 14 0c b9 36 00 00 00 00 00 00 00 00 ",
        "e8 c8 51 6d ff ff ff ff 00 00 00 09 00 00 00 0a ",
        "00 00 00 0b 00 00 00 0c ff ff ff ff ff ff ff ff ",
    );

    /// Pad a block out to its full size.
    ///
    /// The padding is 0xff rather than zeroes because that is what the real
    /// block holds past its live entries -- a free list fills the slots outside
    /// its window with the null block -- and the checksum covers the whole
    /// block, so padding with anything else would change it.
    fn block_of(hex_rows: &str, blocksize: usize) -> Vec<u8> {
        let mut out: Vec<u8> = hex_rows
            .split_whitespace()
            .map(|byte| u8::from_str_radix(byte, 16).expect("two hex digits per byte"))
            .collect();
        out.resize(blocksize, 0xff);
        out
    }

    /// A real free list, decoded, must agree with the reference about its
    /// header and about the blocks it holds.
    /// Giving blocks to a list that has never been written puts them where the
    /// window says, and they can be read back.
    ///
    /// This is the smallest version of what stocking the free list does, with
    /// nothing else in it: a blank block, the window an unwritten list should
    /// start from, three blocks given back, and the entries read back out.  It is
    /// here because the full version failed on a real image with entries that
    /// read as a block number and as zero, and it is not knowable from that
    /// whether the list or the code feeding it was wrong.
    #[test]
    fn an_unwritten_list_takes_blocks_from_an_empty_window() {
        let bs = 512usize;
        // A block of zeroes is a list with *no header*, which is what every
        // file system here has: its free list is a bare array of block numbers
        // and nothing else.  So initialising it must not put a magic at the
        // front, which is where entry zero lives.
        let mut list =
            Agfl::from_bytes(vec![0u8; bs], false).expect("a blank block is long enough");
        assert!(
            !list.has_header(),
            "a block of zeroes claims to carry a header"
        );
        list.initialise(7, &[0xab; 16]);
        assert!(
            !list.has_header(),
            "initialising put a header on a block that has none, over entry zero"
        );

        // A blank list has no live entries, so its window starts empty however
        // the header names one.
        let mut window = list.window(0, 0, 0);
        for b in [900u32, 901, 902] {
            window = list.give_back(&mut window, b).expect("the list has room");
        }
        assert_eq!(window.count, 3, "the window did not count what went in");
        for (i, want) in [900u32, 901, 902].iter().enumerate() {
            let slot = window.first as usize + i;
            assert_eq!(
                list.entry(slot as u32),
                *want,
                "entry {slot} does not hold the block that was given back"
            );
        }
        assert!(
            (0..list.entries).all(|i| list.entry(i) == NULL_AGBLOCK || i < window.count as u32),
            "entries outside the window were written"
        );
    }

    #[test]
    fn real_v5_agfl_decodes() {
        let agfl = Agfl::from_bytes(block_of(V5_AGFL, 4096), true).unwrap();
        assert!(agfl.is_written());
        assert!(agfl.verify_crc(), "a real free list must verify");
        assert_eq!(agfl.entry(0), NULL_AGBLOCK, "slot 0 is empty");
        assert_eq!(agfl.entry(1), 9);
        assert_eq!(agfl.entry(2), 10);
        assert_eq!(agfl.entry(3), 11);
        assert_eq!(agfl.entry(4), 12);
        assert_eq!(agfl.entry(5), NULL_AGBLOCK);
        // A 4 KiB block holds a thousand block numbers, less the header the
        // checksum makes room for.
        assert_eq!(agfl.capacity(), (4096 - 36) / 4);
        // Past the end is the null block rather than a read past the array.
        assert_eq!(agfl.entry(agfl.capacity() + 5), NULL_AGBLOCK);
    }

    /// The group's live window, as the group header records it, selects the
    /// blocks that are really there.
    #[test]
    fn the_window_selects_the_blocks() {
        let agfl = Agfl::from_bytes(block_of(V5_AGFL, 4096), true).unwrap();
        let window = agfl.window(1, 4, 4);
        assert_eq!(agfl.window_blocks(&window), vec![9, 10, 11, 12]);
    }

    /// A block that was never written as a free list is an empty free list, not
    /// a damaged one: a group that has never had to record a free block may
    /// still have a free block sitting in the space its free list would have
    /// used.
    #[test]
    fn an_unwritten_block_is_an_empty_free_list() {
        let mut agfl = Agfl::from_bytes(vec![0xffu8; 512], false).unwrap();
        assert!(!agfl.is_written());
        assert!(agfl.verify_crc(), "there is no checksum to be wrong about");
        let mut window = agfl.window(0, 0, 0);
        let err = agfl.take_front(&mut window).unwrap_err();
        assert_eq!(err.errno(), libc::ENOENT);
    }

    /// Taking a block shrinks the window and empties the slot, so the same
    /// block cannot be taken twice.
    #[test]
    fn taking_moves_the_window_and_empties_the_slot() {
        let mut agfl = Agfl::from_bytes(block_of(V5_AGFL, 4096), true).unwrap();
        let mut window = agfl.window(1, 4, 4);

        assert_eq!(agfl.take_front(&mut window).unwrap(), 9);
        assert_eq!(window, agfl.window(2, 4, 3));
        assert_eq!(
            agfl.entry(1),
            NULL_AGBLOCK,
            "the taken slot must be emptied"
        );
        assert_eq!(agfl.take_front(&mut window).unwrap(), 10);
        assert_eq!(agfl.window_blocks(&window), vec![11, 12]);

        // And a window that says a block is there when it is not has to be
        // refused rather than handing out the null block.
        let mut lying = agfl.window(1, 4, 4);
        let err = agfl.take_front(&mut lying).unwrap_err();
        assert_eq!(err.errno(), crate::libxfuse::EUCLEAN);
    }

    /// Giving a block back grows the window, and an empty window starts again
    /// at the bottom of the array.
    #[test]
    fn giving_back_grows_the_window() {
        let mut agfl = Agfl::from_bytes(block_of(V5_AGFL, 4096), true).unwrap();
        let mut window = agfl.window(2, 4, 3);
        agfl.give_back(&mut window, 4096).unwrap();
        assert_eq!(window, agfl.window(2, 5, 4));
        assert_eq!(agfl.entry(5), 4096);

        let mut empty = agfl.window(0, 0, 0);
        agfl.give_back(&mut empty, 4097).unwrap();
        assert_eq!(empty, agfl.window(0, 0, 1));
        assert_eq!(agfl.entry(0), 4097);
    }

    /// The array is a fixed size, and a full free list has to refuse a returned
    /// block rather than write past the end of the block.
    #[test]
    fn a_full_free_list_refuses_a_returned_block() {
        let mut agfl = Agfl::from_bytes(block_of(V5_AGFL, 4096), true).unwrap();
        // Fill it by giving blocks back until one is refused, rather than to a
        // computed capacity: the point of the test is that a full list refuses,
        // and working out how full "full" is has already been wrong once.
        let mut window = agfl.window(0, 0, 0);
        let mut refused = None;
        for block in 1..=10_000u32 {
            match agfl.give_back(&mut window, block) {
                Ok(_) => {}
                Err(e) => {
                    refused = Some(e);
                    break;
                }
            }
        }
        let err = refused.expect("a list that never refuses is not a list");
        assert_eq!(err.errno(), libc::ENOSPC);
        // And it refused because it ran out of array, not a little short of it:
        // the whole array holds entries, and a list holding all of them is full.
        assert_eq!(
            window.count, agfl.entries,
            "a list that refused should be holding every entry it can"
        );
    }

    /// Changes have to survive being written out and read back, checksum and
    /// all, because that is what happens to every metadata block.
    #[test]
    fn changes_survive_a_round_trip() {
        let mut agfl = Agfl::from_bytes(block_of(V5_AGFL, 4096), true).unwrap();
        let mut window = agfl.window(1, 4, 4);
        let taken = agfl.take_front(&mut window).unwrap();
        agfl.update_crc();

        let bytes = agfl.into_bytes();
        let again = Agfl::from_bytes(bytes, true).unwrap();
        assert!(again.verify_crc(), "a changed free list must still verify");
        assert!(again.is_written());
        assert_eq!(taken, 9);
        assert_eq!(again.entry(1), NULL_AGBLOCK, "the taken slot stays empty");
        assert_eq!(again.entry(2), 10, "the rest of the window is untouched");
    }

    /// A free list written into a block that held something else has to be
    /// blanked, or the old contents would be read as free blocks.
    #[test]
    fn initialising_blanks_the_array() {
        let mut agfl = Agfl::from_bytes(vec![0xa5u8; 512], true).unwrap();
        assert!(!agfl.has_header());
        agfl.initialise(3, &[7u8; 16]);
        // A headerless list has no header to write, so none appears; what
        // matters is that the old contents do not survive as blocks.
        let blocks: Vec<_> = (0..agfl.capacity()).map(|i| agfl.entry(i)).collect();
        assert!(
            blocks.iter().all(|b| *b == NULL_AGBLOCK),
            "initialising must not leave the old contents readable as blocks"
        );
    }
}
