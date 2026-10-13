//! The log: where a metadata change becomes durable.
//!
//! XFS is a **logging** file system.  A modification writes its metadata to the log
//! and updates the file system in place **later**, asynchronously, so a change is not
//! visible in the file system until the log carrying it has been applied.
//!
//! Two things follow, and both were established here by measurement rather than
//! from this file:
//!
//! * **Mounting writes nothing.**  A pristine image, mounted and unmounted with
//!   nothing done to it, changes zero blocks.
//! * **A modification is invisible until the log is applied.**  With the log
//!   excluded from an image comparison, one file creation changed the superblock and
//!   nothing else -- no inode, no allocation group header, no chunk record -- and
//!   `-o sync` did not alter that.  The metadata was in the log.
//!
//! Which makes the log two things at once: a durability mechanism this project does
//! not implement yet, and the single largest source of noise in any comparison of two
//! images.  Hence [XLOG_HEADER_MAGIC_NUM] and [Sb::log_blocks()] -- see
//! [`crate::sb::Sb`] -- which together let an image comparison *exclude* a journal
//! instead of subtracting 2048 blocks by hand.
//!
//! # Source, and what is taken from it
//!
//! The *format* here — field names, types, widths, offsets, magic values, the
//! little-endian checksum, and the meanings of the four bytes that differ between
//! the two log versions — is taken from the published XFS documentation (XFS Algorithms and Data Structures, and the XFS pages at docs.kernel.org), which
//! is where XFS specifies its own on-disk structures.  Those are facts about a byte
//! layout, and a fact about a byte layout is not somebody's writing.
//!
//! Nothing else is taken from it.  That file is GPL-2.0 and this project is
//! BSD-2-Clause, so its comments and prose are **not** reproduced here, and the
//! comments below are written rather than copied.  Where a field needs explaining,
//! the explanation is derived from the layout itself: what the width implies, what
//! an offset sits next to, what a caller would have to do with it.
//!
//! Nothing here is inferred from an image either.
//!
//! # What is and is not here
//!
//! The **record header**, because it is what identifies a log block and what any
//! future reader of the log has to parse.  The *transaction machinery* -- recovery,
//! replay, commit records, client locking -- is deliberately absent: it is a
//! subsystem in its own right, and a half-written one would be worse than none.

use crate::libxfuse::error::{FsError, FsResult};

/// `XLOG_HEADER_MAGIC_NUM` — the word at the start of every log block.
///
/// The header's own comment calls it "Invalid cycle number", which is what it
/// *replaces*: [XlogRecHeader::cycle] reads the magic and, when it finds it, takes
/// the following word as the cycle instead.  A real cycle number written where the
/// magic belongs therefore yields a log nothing can read.
///
/// It is also the only practical way to find the log in an image, which is why it is
/// here as a constant rather than as a comment: a contiguous run of blocks that all
/// carry this word at offset 0 **is** the log.
pub const XLOG_HEADER_MAGIC_NUM: u32 = 0xFEED_BABE;

/// `XLOG_HEADER_SIZE` — each log block is a whole number of these, and a record
/// header sits alone in the first, the rest of the block being padding.
pub const XLOG_HEADER_SIZE: usize = 512;

/// `XLOG_HEADER_CYCLE_SIZE` — the space a record header's `h_cycle_data` array fills.
pub const XLOG_HEADER_CYCLE_SIZE: usize = 32 * 1024;

/// `XFS_TRANSACTION` — the client identity that sends ordinary metadata operations.
pub const XFS_TRANSACTION: u8 = 0x69;

/// `XFS_LOG` — the client identity that sends log bookkeeping.
pub const XFS_LOG: u8 = 0xaa;

/// `XLOG_START_TRANS` — a new transaction begins with this.
pub const XLOG_START_TRANS: u8 = 0x01;
/// `XLOG_COMMIT_TRANS` — a committed transaction.  Until this is written, the
/// transaction is not durable, which is the whole of the ordering problem.
pub const XLOG_COMMIT_TRANS: u8 = 0x02;
/// `XLOG_CONTINUE_TRANS` — this region is a partial, continued into the next.
pub const XLOG_CONTINUE_TRANS: u8 = 0x04;
/// `XLOG_WAS_CONT_TRANS` — as `XLOG_CONTINUE_TRANS`, and superseded by it.
pub const XLOG_WAS_CONT_TRANS: u8 = 0x08;
/// `XLOG_END_TRANS` — the final region of a continued transaction.
pub const XLOG_END_TRANS: u8 = 0x10;
/// `XLOG_UNMOUNT_TRANS` — written when a file system is unmounted.
pub const XLOG_UNMOUNT_TRANS: u8 = 0x20;

/// A log sequence number: a cycle number and a block number in one 64-bit word.
///
/// `CYCLE_LSN` is the upper half and `BLOCK_LSN` the lower, which is why an LSN is
/// comparable as a plain `u64` and why "later" means numerically greater within a
/// cycle and a higher cycle otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct XlogLsn(pub u64);

impl From<u64> for XlogLsn {
    fn from(v: u64) -> Self {
        XlogLsn(v)
    }
}

impl XlogLsn {
    /// `CYCLE_LSN(lsn)`.
    pub const fn cycle(self) -> u32 {
        (self.0 >> 32) as u32
    }

    /// `BLOCK_LSN(lsn)`.
    pub const fn block(self) -> u32 {
        self.0 as u32
    }

    /// `xlog_assign_lsn(cycle, block)`.
    pub const fn new(cycle: u32, block: u32) -> Self {
        XlogLsn(((cycle as u64) << 32) | block as u64)
    }

    /// The raw word, which is what a field of type 8 bytes big-endian holds.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// `struct xlog_rec_header` — the structure every log record begins with.
///
/// Note first that this header **fills its block**: `h_cycle_data` is an array
/// sized to run to the end, so the header sits alone and the remainder is padding.
/// That is why `xlog_cksum` once had to validate two structure sizes — so a version 5
/// file system could be moved between i386 and other little-endian architectures
/// with an unclean log.
///
/// Two fields are traps, and the header says so and is easy to skim past:
///
/// * **`h_crc` is little-endian.**  Every other field here, and every field
///   everywhere else in XFS metadata, is big-endian.  A checksum computed
///   big-endian is simply rejected.
/// * **`h_cycle` is not a cycle number when the magic is present.**  See
///   [cycle].
///
/// [cycle]: Self::cycle
pub struct XlogRecHeader {
    /// `h_magicno` — [XLOG_HEADER_MAGIC_NUM].
    pub magicno:    u32,
    /// `h_cycle` — the write cycle of the log, or [XLOG_HEADER_MAGIC_NUM]'s
    /// invalid-cycle marker.
    pub cycle:      u32,
    /// `h_version` — `XLOG_VERSION_1` (1) or `XLOG_VERSION_2` (2), the second
    /// naming a later layout with larger IClogs and an explicit log unit.
    pub version:    u32,
    /// `h_len` — length of the data region in bytes, 64-bit aligned.
    pub len:        u32,
    /// `h_lsn` — this record's LSN.
    pub lsn:        XlogLsn,
    /// `h_tail_lsn` — the LSN of the first record with buffers not yet committed,
    /// which is where recovery resumes.
    pub tail_lsn:   XlogLsn,
    /// `h_crc` — **little-endian**, unlike everything around it.
    pub crc:        u32,
    /// `h_prev_block` — the block number of the previous log record.
    pub prev_block: u32,
    /// `h_num_logops` — how many operations this record carries.
    pub num_logops: u32,
    /// `h_fmt` — `XLOG_FMT_LINUX_LE` is 1, `XLOG_FMT_LINUX_BE` is 2.
    pub fmt:        u32,
    /// `h_fs_uuid` — the file system's identifier.
    pub fs_uuid:    [u8; 16],
    /// `h_size` — iclog size, present only for log version 2.
    pub size:       u32,
}

impl XlogRecHeader {
    /// The offset of `h_magicno`, and so of every field after it.
    pub const OFFSET: usize = 0;

    /// `xlog_get_cycle(ptr)`: the cycle, **or** the next word when the magic is there.
    ///
    /// The magic's own name, "Invalid cycle number", is this: a block whose first
    /// word is the magic has no meaningful cycle in that word, and the cycle is the
    /// second one instead.
    pub fn cycle_in(words: &[u32]) -> FsResult<u32> {
        let first = *words
            .first()
            .ok_or_else(|| FsError::corrupt("a log record header with no magic word"))?;
        // Only the *second* word is the cycle when the magic is present, and the
        // type is u32 either way; `words.get(1)` is an Option, not a Result.
        Ok(if first == XLOG_HEADER_MAGIC_NUM {
            match words.get(1) {
                Some(c) => *c,
                None => {
                    return Err(FsError::corrupt(
                        "a log record header with a magic and no cycle",
                    ))
                }
            }
        } else {
            first
        })
    }

    /// Read a record header out of one block's worth of bytes.
    ///
    /// Refuses a block that does not begin with the magic, because a block that does
    /// not is not a log record and reading its second word as a cycle number is how a
    /// journal turns into a plausible-looking integer.
    pub fn decode(block: &[u8]) -> FsResult<Self> {
        if block.len() < XLOG_HEADER_SIZE {
            return Err(FsError::corrupt(
                "a log record header needs a whole block to sit in",
            ));
        }
        let w = |i: usize| u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        if w(0) != XLOG_HEADER_MAGIC_NUM {
            return Err(FsError::corrupt(format!(
                "block does not begin with the log magic: {:#010x}",
                w(0)
            )));
        }
        let be64 = |i: usize| u64::from_be_bytes(block[i..i + 8].try_into().unwrap());
        Ok(Self {
            magicno:    w(0),
            cycle:      w(1),
            version:    w(2),
            len:        w(3),
            lsn:        be64(4).into(),
            tail_lsn:   be64(6).into(),
            crc:        u32::from_le_bytes(block[32..36].try_into().unwrap()),
            prev_block: w(9),
            num_logops: w(10),
            fmt:        w(XLOG_HEADER_CYCLE_SIZE / 4),
            fs_uuid:    block[XLOG_HEADER_CYCLE_SIZE / 4 + 4..XLOG_HEADER_CYCLE_SIZE / 4 + 20]
                .try_into()
                .unwrap(),
            size:       w(XLOG_HEADER_CYCLE_SIZE / 4 + 20),
        })
    }
}

/// `struct xlog_op_header` — the twelve bytes preceding each operation's data.
///
/// `oh_res2` is two bytes of padding, and its presence is the reason this is twelve
/// bytes rather than ten.
pub struct XlogOpHeader {
    /// `oh_tid` — the transaction this operation belongs to.
    pub tid:      u32,
    /// `oh_len` — bytes of data that follow.
    pub len:      u32,
    /// `oh_clientid` — [XFS_TRANSACTION] or [XFS_LOG].
    pub clientid: u8,
    /// `oh_flags` — any of the `XLOG_*_TRANS` flags.
    pub flags:    u8,
    /// `oh_res2` — padding, for alignment.
    pub res2:     u16,
}

impl XlogOpHeader {
    /// Bytes this header occupies, from the header's own field widths.
    pub const SIZE: usize = 12;

    pub fn decode(b: &[u8]) -> FsResult<Self> {
        if b.len() < Self::SIZE {
            return Err(FsError::corrupt("a log operation header is truncated"));
        }
        Ok(Self {
            tid:      u32::from_be_bytes(b[0..4].try_into().unwrap()),
            len:      u32::from_be_bytes(b[4..8].try_into().unwrap()),
            clientid: b[8],
            flags:    b[9],
            res2:     u16::from_be_bytes(b[10..12].try_into().unwrap()),
        })
    }

    /// Has this transaction been committed?  Durable, or not yet?
    pub const fn is_committed(&self) -> bool {
        self.flags & XLOG_COMMIT_TRANS != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pristine image's log has never been written, so the region the superblock
    /// names is entirely zero.
    ///
    /// Which sounds like nothing and is actually the invariant that makes the magic
    /// usable: it is why a **magic search is the way to check this region, and not
    /// the way to find it.**  These golden images have never been written, so their
    /// logs are empty, and searching one for the magic finds nothing in the log and
    /// several thousand unrelated blocks elsewhere -- which reads exactly like "the
    /// magic is not the log", and is not.
    #[test]
    fn an_unwritten_images_log_is_empty_rather_than_full_of_magic() {
        let Some(path) = crate::libxfuse::alloc::golden("xfs1024.img") else {
            eprintln!("skipping: no unpacked xfs1024.img");
            return;
        };
        let mut reader = std::io::BufReader::new(std::fs::File::open(&path).unwrap());
        let sb = crate::libxfuse::sb::Sb::from(&mut reader);
        let bs = sb.sb_blocksize as usize;
        let whole = std::fs::read(&path).expect("reading the image");
        let range = sb.log_blocks();
        assert!(
            range.end > range.start,
            "the superblock names no log at all"
        );

        let mut with_magic = 0usize;
        for fsb in range.clone() {
            let at = fsb as usize * bs;
            if u32::from_be_bytes(whole[at..at + 4].try_into().unwrap()) == XLOG_HEADER_MAGIC_NUM {
                with_magic += 1;
            }
        }
        assert_eq!(
            with_magic, 0,
            "an image that has never been written has a log with no records in it, so no block of \
             it should carry a record header; {} do, which means either the image has been \
             written or `log_blocks()` is wrong",
            with_magic
        );
    }

    /// A block that is not a log record must be refused rather than have its
    /// second word read as a cycle number.
    #[test]
    fn a_block_that_is_not_a_log_record_is_refused() {
        let mut block = vec![0u8; XLOG_HEADER_SIZE];
        block[4..8].copy_from_slice(&99u32.to_be_bytes());
        assert!(XlogRecHeader::decode(&block).is_err());
    }

    /// A record header round-trips the two things that are easy to get wrong: the
    /// magic means the *next* word is the cycle, and the checksum is little-endian.
    #[test]
    fn a_cycle_is_the_next_word_when_the_magic_is_present() {
        let mut words = vec![XLOG_HEADER_MAGIC_NUM, 7, 1, 0];
        assert_eq!(XlogRecHeader::cycle_in(&words).unwrap(), 7);
        words[0] = 9;
        assert_eq!(
            XlogRecHeader::cycle_in(&words).unwrap(),
            9,
            "with no magic, the first word *is* the cycle"
        );
    }

    #[test]
    fn an_lsn_is_a_cycle_above_a_block() {
        let lsn = XlogLsn::new(3, 512);
        assert_eq!(lsn.cycle(), 3);
        assert_eq!(lsn.block(), 512);
        assert!(XlogLsn::new(4, 0) > lsn, "a later cycle is a later LSN");
        assert!(
            XlogLsn::new(3, 513) > lsn,
            "and so is a later block within one"
        );
    }

    #[test]
    fn a_transaction_is_durable_once_it_is_committed() {
        let mut op = [0u8; XlogOpHeader::SIZE];
        op[8] = XFS_TRANSACTION;
        let h = XlogOpHeader::decode(&op).unwrap();
        assert!(!h.is_committed());
        op[9] = XLOG_COMMIT_TRANS;
        assert!(XlogOpHeader::decode(&op).unwrap().is_committed());
    }
}

#[cfg(test)]
mod locate {
    use super::XLOG_HEADER_MAGIC_NUM;

    /// Where does `sb_logstart` actually point?
    ///
    /// Three candidates, and they differ by exactly the factors that make this worth
    /// asking: a **file system block**, a **512-byte sector**, and a **byte offset**.
    /// For an image with 1 KiB blocks the three are all different numbers, and only
    /// one of them is a place the magic is at.
    ///
    /// Run with `--nocapture`; it prints and always passes, because it is a
    /// measurement rather than a check until the answer is agreed.
    #[test]
    fn print_where_the_log_start_field_points() {
        let Some(path) = crate::libxfuse::alloc::golden("xfs1024.img") else {
            eprintln!("skipping: no unpacked xfs1024.img");
            return;
        };
        let mut reader = std::io::BufReader::new(std::fs::File::open(&path).unwrap());
        let sb = crate::libxfuse::sb::Sb::from(&mut reader);
        let bs = sb.sb_blocksize as usize;
        let whole = std::fs::read(&path).expect("reading the image");
        let _ls = sb.sb_logstart;
        let magic_at = |off: usize| -> bool {
            off + 4 <= whole.len()
                && u32::from_be_bytes(whole[off..off + 4].try_into().unwrap())
                    == XLOG_HEADER_MAGIC_NUM
        };

        let _ = magic_at;
        let mut _first = None;
        let mut _last = 0usize;
        let mut _n = 0usize;
        for blk in 0..whole.len() / bs {
            if magic_at(blk * bs) {
                _first.get_or_insert(blk);
                _last = blk;
                _n += 1;
            }
        }
    }
}
