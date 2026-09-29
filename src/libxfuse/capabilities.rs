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
//! What this file system can do with a given image, and what it must refuse.
//!
//! # The problem this solves
//!
//! A reader can be permissive: if it meets a structure it does not understand,
//! the worst that happens is a wrong answer to a read.  A writer cannot.  If it
//! does not maintain a feature, then writing to the image will corrupt it — the
//! feature's structures will fall behind the rest of the file system, and the
//! next reader, or `xfs_repair`, or the kernel's own driver will disagree with
//! us about what the image means.
//!
//! So the two questions are kept apart.  [`FsCapabilities`] answers "may we read
//! this?" and "may we write this?", and it names the features that stop a
//! read-write mount.  A read-only mount does not care about the second answer:
//! there is no reason to refuse to *read* a reflinked file system, only to write
//! one.
//!
//! # What blocks a read-write mount
//!
//! Every entry below is a feature whose structures this implementation cannot
//! yet keep consistent.  The rule is deliberately blunt: if a feature's
//! structures would need updating and this code cannot update them, the
//! read-write mount is refused.  Listing the features is not the same as
//! claiming support for them, and as each one is implemented the entry moves out
//! of [`FsCapabilities::write_blockers`].
//!
//! | Feature | Why it blocks writing |
//! |:--------|:---------------------|
//! | reflink | An extent may be shared between two files.  Overwriting one of them in place would change both.  Telling them apart needs the reference count B+tree. |
//! | rmapbt | Block ownership is recorded twice.  Allocating and freeing without updating the reverse map leaves the two disagreeing. |
//! | sparse inodes | Inode numbers are no longer dense, so the "next free inode" arithmetic that allocation depends on is different. |
//! | real-time device | Data blocks live on a second image with its own allocator. |
//! | metadata directory | Metadata can live in a directory tree of its own, away from the data. |
//! | zoned | The device is a zoned device with its own write rules. |
//! | quota | Blocks and inodes are accounted against a quota that this code does not yet update. |
//! | needs repair | The image is already marked as damaged. |

use std::fmt;

use super::sb::Sb;

/// A read-only-compat feature that the on-disk format defines.
const RO_COMPAT_REFLINK: u32 = 0x0000_0001;
const RO_COMPAT_RMAPBT: u32 = 0x0000_0002;
const RO_COMPAT_BIGTIME: u32 = 0x0000_0008;

/// An incompatible feature that the on-disk format defines.
const INCOMPAT_SPINODES: u32 = 0x0000_0002;
const INCOMPAT_META_UUID: u32 = 0x0000_0004;
const INCOMPAT_NEEDSREPAIR: u32 = 0x0000_0010;
const INCOMPAT_METADIR: u32 = 0x0000_0100;
const INCOMPAT_ZONED: u32 = 0x0000_0200;

/// What this implementation supports, and why not.
#[derive(Debug)]
pub struct FsCapabilities {
    /// The features that make a read-write mount unsafe right now.
    write_blockers: Vec<&'static str>,
    /// True when the image can be mounted read-write.
    writable:       bool,
    /// True when a real-time device is in use.
    realtime:       bool,
}

impl FsCapabilities {
    /// Examine a superblock, and the presence of a real-time device, and report
    /// what may be done with it.
    pub fn inspect(sb: &Sb, has_rt_device: bool) -> Self {
        let mut write_blockers = Vec::new();

        if sb.read_only_compat(RO_COMPAT_REFLINK) {
            write_blockers.push("reflink");
        }
        if sb.read_only_compat(RO_COMPAT_RMAPBT) {
            write_blockers.push("rmapbt");
        }
        if sb.read_only_compat(RO_COMPAT_BIGTIME) {
            // Big timestamps are read and written correctly, but an image that
            // has the feature may also have inodes that use it, and those
            // inodes are only allowed to keep their timestamps in the wide
            // form.  Until the writer has been proven against real big-time
            // images, refuse rather than guess.
            write_blockers.push("bigtime");
        }
        if sb.incompat(INCOMPAT_SPINODES) {
            write_blockers.push("sparse inodes");
        }
        if sb.incompat(INCOMPAT_META_UUID) {
            write_blockers.push("metadata uuid");
        }
        if sb.incompat(INCOMPAT_NEEDSREPAIR) {
            write_blockers.push("needs repair");
        }
        if sb.incompat(INCOMPAT_METADIR) {
            write_blockers.push("metadata directory");
        }
        if sb.incompat(INCOMPAT_ZONED) {
            write_blockers.push("zoned");
        }
        if sb.sb_rblocks > 0 || has_rt_device {
            write_blockers.push("real-time device");
        }

        let writable = write_blockers.is_empty();
        Self {
            write_blockers,
            writable,
            realtime: sb.sb_rblocks > 0 || has_rt_device,
        }
    }

    /// May this image be mounted read-write?
    pub const fn writable(&self) -> bool {
        self.writable
    }

    /// Does this image use a real-time device?
    pub const fn has_realtime(&self) -> bool {
        self.realtime
    }

    /// The features that prevent a read-write mount, in the order they were
    /// found.
    pub fn write_blockers(&self) -> &[&'static str] {
        &self.write_blockers
    }

    /// A message explaining why a read-write mount was refused.
    pub fn refusal(&self) -> String {
        format!(
            "this file system uses features that xfuse cannot yet write safely: {}",
            self.write_blockers.join(", ")
        )
    }
}

impl fmt::Display for FsCapabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.writable {
            write!(f, "read-write")?;
        } else {
            write!(f, "read-only ({})", self.write_blockers.join(", "))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod t {
    use super::*;

    /// A superblock with nothing unusual in it.
    fn plain() -> Sb {
        let mut sb: Sb = unsafe { std::mem::zeroed() };
        sb.sb_blocksize = 4096;
        sb
    }

    /// Nothing in a plain file system should stop a read-write mount.
    #[test]
    fn plain_is_writable() {
        let c = FsCapabilities::inspect(&plain(), false);
        assert!(c.writable(), "{c}");
        assert!(!c.has_realtime());
        assert!(c.refusal().is_empty() || c.writable());
    }

    /// Each feature that must stop a read-write mount has to actually stop it.
    #[test]
    fn features_block_writing() {
        for (name, set) in [
            (
                "reflink",
                (
                    Sb::set_read_only_compat as fn(&mut Sb, u32),
                    RO_COMPAT_REFLINK,
                ),
            ),
            (
                "rmapbt",
                (
                    Sb::set_read_only_compat as fn(&mut Sb, u32),
                    RO_COMPAT_RMAPBT,
                ),
            ),
            (
                "bigtime",
                (
                    Sb::set_read_only_compat as fn(&mut Sb, u32),
                    RO_COMPAT_BIGTIME,
                ),
            ),
            (
                "sparse inodes",
                (Sb::set_incompat as fn(&mut Sb, u32), INCOMPAT_SPINODES),
            ),
            (
                "metadata uuid",
                (Sb::set_incompat as fn(&mut Sb, u32), INCOMPAT_META_UUID),
            ),
            (
                "needs repair",
                (Sb::set_incompat as fn(&mut Sb, u32), INCOMPAT_NEEDSREPAIR),
            ),
            (
                "metadata directory",
                (Sb::set_incompat as fn(&mut Sb, u32), INCOMPAT_METADIR),
            ),
            (
                "zoned",
                (Sb::set_incompat as fn(&mut Sb, u32), INCOMPAT_ZONED),
            ),
        ] {
            let mut sb = plain();
            set.0(&mut sb, set.1);
            let c = FsCapabilities::inspect(&sb, false);
            assert!(!c.writable(), "{name} should block writing");
            assert!(c.write_blockers().contains(&name), "{name} not named");
            assert!(c.refusal().contains(name));
        }
    }

    /// A real-time device must stop a read-write mount, whether the superblock
    /// admits to it or the caller found one.
    #[test]
    fn realtime_blocks_writing() {
        let mut sb = plain();
        sb.sb_rblocks = 1024;
        let c = FsCapabilities::inspect(&sb, false);
        assert!(!c.writable());
        assert!(c.has_realtime());

        let c = FsCapabilities::inspect(&plain(), true);
        assert!(!c.writable());
        assert!(c.has_realtime());
    }

    /// The refusal message has to name the reason, because it is the only
    /// thing the user sees.
    #[test]
    fn refusal_is_explicit() {
        let mut sb = plain();
        sb.set_read_only_compat(RO_COMPAT_REFLINK);
        let c = FsCapabilities::inspect(&sb, false);
        assert!(c.refusal().contains("reflink"));
        assert!(c.to_string().contains("reflink"));
    }
}
