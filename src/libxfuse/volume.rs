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
    collections::HashMap,
    ffi::OsStr,
    io::{self, Read},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
        Mutex,
        OnceLock,
    },
    time::Duration,
};

use fuser::{
    BsdFileFlags,
    Errno,
    FileHandle,
    Filesystem,
    FopenFlags,
    Generation,
    INodeNo,
    InitFlags,
    KernelConfig,
    LockOwner,
    OpenFlags,
    RenameFlags,
    ReplyAttr,
    ReplyBmap,
    ReplyCreate,
    ReplyDirectory,
    ReplyEmpty,
    ReplyEntry,
    ReplyLseek,
    ReplyOpen,
    ReplyStatfs,
    ReplyWrite,
    ReplyXattr,
    Request,
    WriteFlags,
};
use libc::{ERANGE, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK};
// fallocate(2) mode flags (from linux/falloc.h)
const FALLOC_FL_KEEP_SIZE: i32 = 0x01;
const FALLOC_FL_PUNCH_HOLE: i32 = 0x02;
const FALLOC_FL_COLLAPSE_RANGE: i32 = 0x08;
const FALLOC_FL_ZERO_RANGE: i32 = 0x10;
const FALLOC_FL_INSERT_RANGE: i32 = 0x20;
const FALLOC_FL_UNSHARE_RANGE: i32 = 0x40;
const FALLOC_FL_WRITE_ZEROES: i32 = 0x80;
use tracing::{debug, warn};

use super::{
    attr::Attr,
    block_device::{Access, BlockDevice},
    block_reader::BlockReader,
    capabilities::FsCapabilities,
    definitions::{XfsFsblock, XfsIno, XFS_DINODE_MAGIC},
    dinode::Dinode,
    dinode_core::XfsDinodeFmt,
    dir3::{
        Dir3,
        XFS_DIR3_FT_BLKDEV,
        XFS_DIR3_FT_CHRDEV,
        XFS_DIR3_FT_DIR,
        XFS_DIR3_FT_FIFO,
        XFS_DIR3_FT_REG_FILE,
        XFS_DIR3_FT_SOCK,
        XFS_DIR3_FT_SYMLINK,
        XFS_DIR3_FT_UNKNOWN,
    },
    dir3_sf::{ShortformDirectory, SF_OFFSET_STEP},
    error::{no_entry, FsError, FsResult},
    inode::RawDinode,
    sb::Sb,
    transaction::{CommitMode, Transaction, TransactionContext},
};
use crate::libxfuse::{
    alloc::free_space::FreeRun,
    bmbt_rec::BmbtRec,
    btree::{
        BmbtInteriorBlock,
        BmbtKey,
        BmbtLeafBlock,
        BmbtLeafRecord,
        BmdrBlock,
        Btree,
        BtreeLblockHdr,
        BtreeRoot,
        BMBT_NULL_PTR,
    },
};

/// We must store the Superblock in a global variable.  This is unfortunate, and limits us to only
/// opening one disk image at a time, but it's necessary in order to use information from the
/// superblock within a Decode::decode implementation.
pub(super) static SUPERBLOCK: OnceLock<Sb> = OnceLock::new();

/// The superblock of the image being read, if one has been opened.
///
/// Several structures here cannot be decoded without it, because their *shape*
/// depends on the file system's layout: whether a directory entry carries a type
/// follows `ftype`, whether an inode number is 64 bits follows `nrext64`, and how
/// far into an inode a fork starts follows the inode size.  A decoder that cannot
/// have it has to say so.
///
/// The alternative was `SUPERBLOCK.get().unwrap()` at nine places, which is what
/// this replaces.  It converts "nobody opened an image in this process" -- a
/// mistake by whoever is reading, not a fault in the image -- into a panic, inside
/// a program whose whole job is reading images other people wrote, and it did so
/// on the very first attempt to read a directory in a unit test.
///
/// `ENODEV` is the error: there is no device, because there is no image.  It is
/// not `EUCLEAN`, which would blame the file system for the caller having no
/// context for it.
pub(super) fn try_superblock() -> Option<&'static Sb> {
    SUPERBLOCK.get()
}

/// The decode-time answer to "there is no image in this process".
pub const NO_IMAGE: &str = "no image has been opened in this process";

#[derive(Debug)]
struct OpenInode {
    dinode: Dinode,
    count:  u64,
}

/// An open file, as FUSE sees it.
///
/// FUSE asks the file system to open a file and then refers to that open file
/// by the handle it returns.  The handle is what tells a write that it is being
/// asked to modify a file that was actually opened, rather than a file number
/// that the kernel made up.
///
/// The file's position is not here.  FUSE sends the offset of every read and
/// write, so the kernel holds the position, and a second copy of it could only
/// disagree with the first.
#[derive(Debug)]
struct OpenFile {
    ino:   u64,
    flags: i32,
}

/// All mutable state of a mounted volume.
///
/// This is kept inside a `Mutex` so that `Volume` can implement
/// `Filesystem` with `&self` methods (required by fuser 0.18).  Locking
/// `state` serializes FUSE operations; the read-only fields (`sb`,
/// `writable`, `no_open`, `no_opendir`) are outside the lock to avoid
/// holding it when the value is not needed.
#[derive(Debug)]
struct State {
    device:      BlockReader,
    rt_device:   Option<BlockReader>,
    sb:          Sb,
    open_files:  HashMap<u64, OpenInode>,
    handles:     HashMap<u64, OpenFile>,
    next_handle: u64,
    tx:          TransactionContext,
    writable:    bool,
}

/// All the state a mounted XFS image needs to answer FUSE requests.
#[derive(Debug)]
pub struct Volume {
    state:      Mutex<State>,
    no_open:    AtomicBool,
    no_opendir: AtomicBool,
}

impl Volume {
    const TTL_RO: Duration = Duration::from_secs(u64::MAX);
    /// How long the kernel may cache an attribute or a directory entry.
    ///
    /// A read-only mount can hand out entries that never expire, because
    /// nothing in it ever changes, and that is what it does.
    ///
    /// A read-write mount cannot cache at all.  A write updates the modification
    /// time of the file it wrote, and the kernel would go on reporting the old
    /// one out of its own cache.  The proper fix is to tell the kernel to throw
    /// the inode away when it changes, which needs a notifier that this
    /// version of the FUSE library does not hand to a file system, so the cache
    /// is simply switched off.  That costs one round trip per attribute, which
    /// is the right trade for a file system that is still experimental: being
    /// right is worth more here than being quick.
    const TTL_RW: Duration = Duration::ZERO;

    /// Open an image.
    ///
    /// `writable` asks for a read-write mount.  It is refused, rather than
    /// quietly downgraded, if the image has features that this implementation
    /// cannot keep up to date, because a read-write mount that silently ignores
    /// a feature corrupts the image.
    pub fn new(
        device_name: &Path,
        rt_device_name: Option<&PathBuf>,
        writable: bool,
    ) -> FsResult<Volume> {
        let access = if writable {
            Access::ReadWrite
        } else {
            Access::ReadOnly
        };
        let block_device = Arc::new(BlockDevice::open(device_name, access)?);
        let mut device = BlockReader::from_device(Arc::clone(&block_device));
        let rt_device = rt_device_name.map(|n| BlockReader::open(n)).transpose()?;

        let superblock = Sb::from(device.by_ref());
        let capabilities = FsCapabilities::inspect(&superblock, rt_device.is_some());
        if writable && !capabilities.writable() {
            return Err(FsError::read_only(capabilities.refusal()));
        }
        if superblock.is_read_only() {
            return Err(FsError::read_only(
                "the superblock says that this file system is read-only",
            ));
        }
        SUPERBLOCK.set(superblock).map_err(|_| FsError::Corrupt {
            what: "a second image was opened in the same process".into(),
        })?;

        if let Some(rtdev) = &rt_device {
            let rtdev_blocks = rtdev.size / u64::from(superblock.sb_blocksize);
            if rtdev_blocks != superblock.sb_rblocks {
                warn!(
                    "realtime device size mismatch.  Expected {} blocks; found {}",
                    superblock.sb_rblocks, rtdev_blocks
                );
            }
        }

        if superblock.sb_rootino == 0 {
            return Err(FsError::Corrupt {
                what: "the superblock has no root inode".into(),
            });
        }
        let root_inode = Dinode::from(device.by_ref(), &superblock, superblock.sb_rootino);
        let mut open_files = HashMap::new();
        open_files.insert(
            1,
            OpenInode {
                dinode: root_inode,
                count:  1,
            },
        );

        debug!(
            "mounting {device_name:?}: {capabilities}, real-time device: {}, {} allocation \
             groups, {} bytes per block",
            capabilities.has_realtime(),
            superblock.agcount(),
            superblock.sb_blocksize
        );

        let mode = if writable {
            CommitMode::Direct
        } else {
            CommitMode::ReadOnly
        };
        let tx = TransactionContext::new(block_device, &superblock, mode);

        Ok(Volume {
            state:      Mutex::new(State {
                device,
                rt_device,
                sb: superblock,
                open_files,
                handles: HashMap::new(),
                next_handle: 1,
                tx,
                writable,
            }),
            no_open:    AtomicBool::new(false),
            no_opendir: AtomicBool::new(false),
        })
    }

    /// Test-only accessor: delegates to `State::add_dirent`.
    #[allow(dead_code)]
    pub fn add_dirent(&self, parent: u64, name: &[u8], ino: u64, ftype: u8) -> FsResult<()> {
        self.state
            .lock()
            .unwrap()
            .add_dirent(parent, name, ino, ftype)
    }

    /// Test-only accessor: delegates to `State::remove_dirent`.
    #[allow(dead_code)]
    pub fn remove_dirent(&self, parent: u64, name: &[u8]) -> FsResult<()> {
        self.state.lock().unwrap().remove_dirent(parent, name)
    }
}

impl State {
    /// How long the kernel may cache things, given whether this mount can
    /// change them.
    fn ttl(&self) -> Duration {
        if self.writable {
            Volume::TTL_RW
        } else {
            Volume::TTL_RO
        }
    }

    /// Answer with the attributes of an inode the kernel has already looked up,
    /// for the paths that have changed one and must say what it now is.
    fn reply_attr_of(&mut self, ino: u64, reply: ReplyAttr) {
        let ttl = self.ttl();
        match self.open_files.get(&ino) {
            Some(oi) => match oi.dinode.di_core.stat(ino) {
                Ok(attr) => reply.attr(&ttl, &attr),
                Err(e) => reply.error(Errno::from_i32(e)),
            },
            None => reply.error(Errno::ENOENT),
        }
    }

    /// The image's inode number for a FUSE inode number.
    ///
    /// FUSE insists that the root directory be inode 1, and XFS does not agree,
    /// so one file has two names here.  Everything that goes to the image needs
    /// the XFS one.
    fn xfs_ino(&self, ino: u64) -> XfsIno {
        if ino == 1 {
            self.sb.sb_rootino
        } else {
            ino as XfsIno
        }
    }

    fn open_inode(&mut self, ino: u64) -> &mut OpenInode {
        let sb = &self.sb;
        let xfs_ino = if ino == 1 {
            sb.sb_rootino
        } else {
            ino as XfsIno
        };
        self.open_files
            .entry(ino)
            .and_modify(|e| e.count += 1)
            .or_insert_with(|| {
                self.device.set_bufsize(sb.inode_size());
                let dinode = Dinode::from(self.device.by_ref(), sb, xfs_ino);
                OpenInode { dinode, count: 1 }
            })
    }

    /// Begin a transaction on the data device.
    fn begin(&mut self) -> Transaction<'_> {
        self.tx.begin()
    }

    /// Choose an allocation group for a new inode.  Use the same AG as the
    /// parent directory.  A real implementation would round-robin or pick the AG
    /// with the most free inodes.
    fn alloc_group_for_new_inode(&self, parent_ino: XfsIno) -> u32 {
        self.sb.locate_ino(parent_ino).agno
    }

    /// Allocate an inode within an existing chunk in the given group.
    fn allocate_inode_in_group(
        tx: &mut Transaction<'_>,
        sb: &Sb,
        agno: u32,
        mode: u16,
        uid: u32,
        gid: u32,
    ) -> FsResult<XfsIno> {
        use crate::libxfuse::alloc::allocator::allocate_ino;

        let new_xfs_ino = allocate_ino(tx, sb, agno, mode, uid, gid)?.ok_or(FsError::NoSpace)?;
        Ok(new_xfs_ino)
    }

    /// Convert XFS inode number to FUSE inode number (inverse of xfs_ino).
    #[allow(dead_code)]
    fn make_fuse_ino(&self, xfs_ino: XfsIno) -> u64 {
        if xfs_ino == self.sb.sb_rootino {
            1
        } else {
            xfs_ino
        }
    }

    /// Static version of make_fuse_ino that takes the superblock as a parameter.
    fn make_fuse_ino_static(xfs_ino: XfsIno, sb: &Sb) -> u64 {
        if xfs_ino == sb.sb_rootino {
            1
        } else {
            xfs_ino
        }
    }

    /// Current UID from the request context (placeholder: use root).
    fn current_uid(&self) -> u32 {
        0
    }

    /// Current GID from the request context (placeholder: use root).
    fn current_gid(&self) -> u32 {
        0
    }

    /// Overwrite part of an existing file.
    ///
    /// Only bytes that are already inside a written extent of the file may be
    /// written.  Anything else -- past the end of the file, into a hole, into
    /// a preallocated-but-unwritten extent, into a directory -- is refused,
    /// because answering those needs an allocator, and guessing at one would
    /// mean writing blocks that belong to nobody.
    ///
    /// The whole range is checked before any of it is written, so a write that
    /// is refused leaves the file exactly as it was.
    /// Write past the end of a file, making the file big enough to hold it.
    ///
    /// Four things have to happen, and they all have to happen in one
    /// transaction, because a file whose size says it has blocks its extents do
    /// not list is a file that will hand the same block out twice:
    ///
    /// 1. Blocks are allocated.  Only the ones the write needs: the file's
    ///    existing blocks, and any hole between the old end and the write, stay
    ///    unallocated, which is what makes the gap read as zeroes rather than
    ///    as whatever those blocks used to hold.
    /// 2. The new blocks are recorded as an extent.  The allocation starts at
    ///    the first block *after* the old end when the write is contiguous with
    ///    it, so the extent joins the last one the file already has rather
    ///    than overlapping it.
    /// 3. The part of the first new block that is between the old end and the
    ///    write is zeroed, which only matters when there is a gap.
    /// 4. The size becomes the end of the write, and the times move.
    ///
    /// The allocation is lined up on a file system block: a write that starts
    /// part way into a block still has to leave the rest of that block alone,
    /// and the transaction's writes are read-modify-write, so it does.
    fn write_extending(&mut self, ino: u64, offset: u64, end: u64, data: &[u8]) -> FsResult<u32> {
        use crate::libxfuse::alloc::allocator::allocate;

        // Everything needed from the superblock is taken before the
        // transaction starts, which borrows the volume.
        let sb = self.sb;
        let blocksize = u64::from(sb.sb_blocksize);
        let size = {
            let oi = self
                .open_files
                .get_mut(&ino)
                .ok_or_else(|| no_entry(b"an inode the kernel has not looked up"))?;
            let size = u64::try_from(oi.dinode.fsize()).map_err(FsError::from)?;
            if !matches!(
                oi.dinode.di_core.di_format,
                XfsDinodeFmt::Extents | XfsDinodeFmt::Btree
            ) {
                return Err(FsError::unsupported(format!(
                    "growing a file whose data fork is in format {:?}",
                    oi.dinode.di_core.di_format
                )));
            }
            size
        };

        // The blocks the write needs.  The file already owns the blocks below
        // its end, and a gap above it stays unallocated, which is what makes the
        // gap read as zeroes rather than as whatever those blocks used to hold.
        let old_last = size.div_ceil(blocksize);
        let write_first = offset / blocksize;
        let last_needed = end.div_ceil(blocksize);
        let first_new = write_first.max(old_last);
        let needed = last_needed - first_new;

        // The block the write starts in, which the file already has when the
        // write is appended to or overwrites its tail.  The part of the write
        // that falls in it goes to the block the file's extents point at, and
        // the rest goes to what is allocated below.
        let first_fsb = if write_first < old_last {
            let file = self
                .open_files
                .get_mut(&ino)
                .ok_or_else(|| no_entry(b"an inode"))?
                .dinode
                .get_file()
                .map_err(FsError::from)?;
            let (Some(fsb), _) = file
                .lookup(self.device.by_ref(), &sb, write_first, false)
                .map_err(FsError::from)?
            else {
                return Err(FsError::Corrupt {
                    what: format!("a block inside the file at {write_first} has no extent"),
                });
            };
            Some(fsb)
        } else {
            None
        };

        let xfs_ino = self.xfs_ino(ino);
        let inode_offset = sb.inode_offset(xfs_ino);
        let inode_size = sb.inode_size();
        // A file whose extents are already in a tree is read out of it here,
        // **before** the transaction starts, because the walk needs a reader and
        // the transaction holds the only mutable one.  Nothing it reads can change
        // underneath it: it is image content, and this transaction writes none of
        // it until the walk has finished.
        let tree: Option<(Vec<BmbtRec>, Vec<u64>, u64)> = {
            let mut peek = vec![0u8; inode_size];
            self.device
                .device()
                .read_at(&mut peek, inode_offset)
                .map_err(|e| FsError::Corrupt {
                    what: format!("reading inode {xfs_ino} to look at its data fork: {e}"),
                })?;
            let probe = RawDinode::from_bytes(peek)?;
            if probe.format() != 3 {
                None
            } else {
                let mut root = probe.data_btree_root()?;
                root.set_owner(xfs_ino);
                let (bmx, blocks) = root.all_extents(self.device.by_ref())?;
                let ptr = *root.ptrs.first().ok_or_else(|| {
                    FsError::corrupt("a data fork's b-tree root points at nothing")
                })?;
                Some((bmx.extents().to_vec(), blocks, ptr))
            }
        };

        let now = std::time::SystemTime::now();

        // A file grows where it is, so the groups are tried from the one its
        // last block is in, and then round upwards.
        let mut agno = 0;
        if let Some(fsb) = first_fsb {
            agno = (fsb / u64::from(sb.sb_agblocks)) as u32;
        } else if size > 0 {
            let file = self
                .open_files
                .get_mut(&ino)
                .ok_or_else(|| no_entry(b"an inode"))?
                .dinode
                .get_file()
                .map_err(FsError::from)?;
            let (Some(fsb), _) = file
                .lookup(self.device.by_ref(), &sb, (size - 1) / blocksize, false)
                .map_err(FsError::from)?
            else {
                return Err(FsError::Corrupt {
                    what: "the block at the end of a file has no extent".into(),
                });
            };
            agno = (fsb / u64::from(sb.sb_agblocks)) as u32;
        }
        agno = agno.min(sb.agcount().saturating_sub(1));

        debug!(
            "extending ino {ino}: size {size} offset {offset} end {end} blocksize {blocksize} \
             first_new {first_new} needed {needed} into_old {}",
            first_fsb
                .map(|_| (first_new * blocksize - offset) as usize)
                .unwrap_or(0)
        );
        let mut tx = self.begin();
        let mut run = None;
        let mut image_offset = None;
        if needed > 0 {
            run = Some(allocate(&mut tx, &sb, agno, needed as u32)?);
            // The gap between the old end and the write, inside the first new
            // block, is not part of the file and has to read as zeroes.  A block
            // that has just been allocated holds whatever it held before.
            let block_start = first_new * blocksize;
            let gap_from = size.max(block_start);
            if gap_from < offset {
                let at = sb.fsb_to_offset(u64::from(run.expect("just allocated").start))
                    + (gap_from - block_start);
                tx.write_bytes(at, &vec![0u8; (offset - gap_from) as usize])?;
            }
            let run = run.expect("just allocated");
            // The allocator's blocks are numbered within the group it was asked
            // about, while a file's extents name blocks in the whole image.  A
            // group-relative number used as an absolute one points at the right
            // block in the *wrong group*, and the file then reads back what that
            // group had there.
            let fsb = sb.ag_block_to_fsb(agno, run.start);
            let base = sb.fsb_to_offset(fsb);
            image_offset = Some(base);
            // A block that has just been given to a file holds whatever it
            // held before -- here, part of a superblock -- and a file must never
            // read back a former directory's contents.  The write below fills
            // the bytes the caller actually wrote; the rest, including the gap a
            // sparse write leaves, has to be zero, and the kernel sends only the
            // bytes that were written, so the file system is the only thing that
            // can do it.
            for i in 0..run.len as u64 {
                tx.write_data(base + i * blocksize, &vec![0u8; blocksize as usize])?;
            }
        }

        // The data.  The part that lands in the block the file already has goes
        // there, and the rest goes to the blocks that were just allocated.  The
        // transaction writes read-modify-write, so whatever else is in that
        // first block is left alone.
        let into_old = first_fsb
            .map(|_| (first_new * blocksize - offset) as usize)
            .unwrap_or(0);
        if into_old > 0 {
            let fsb = first_fsb.expect("the old block, when part of the write is in it");
            let at = sb.fsb_to_offset(fsb) + (offset % blocksize);
            tx.write_data(at, &data[..into_old])?;
        }
        if let Some(image_offset) = image_offset {
            tx.write_data(
                image_offset + (offset + into_old as u64 - first_new * blocksize),
                &data[into_old..],
            )?;
        }

        // The extent, then the size and the times, and then all of it becomes
        // real: the file's size, its extents and the groups' free space have to
        // move together or the file system is describing two different things.
        let mut raw = RawDinode::from_bytes(tx.read_bytes(inode_offset, inode_size)?)?;
        if let Some(run) = run {
            // The extent names two spaces: the file blocks it covers, and the
            // image blocks they live at.
            let fsb = sb.ag_block_to_fsb(agno, run.start);
            // How many blocks the file's data occupies, and how many of them are
            // tree nodes.  Both are counted **before** the fork changes shape,
            // because after a conversion `core_extents` is nothing and a count
            // taken then would be zero -- which is what `xfs_repair` calls
            // `bad nblocks`.
            let data_blocks: u64;
            let mut nodes = 0u64;

            // A file already in a tree: the new extent goes into the leaf the root
            // points at, which is rewritten in place.  The root's shape does not
            // change -- still one key, one pointer -- because there is still one
            // leaf.  A second leaf would need an interior node, and refusing that
            // is honest where silently building a root that names a block nothing
            // wrote is not.
            if let Some((existing, blocks, _)) = tree {
                let mut all = existing;
                all.push(BmbtRec {
                    br_startoff:   first_new,
                    br_startblock: fsb,
                    br_blockcount: u64::from(run.len),
                    br_flag:       false,
                });
                all.sort_by_key(|r| r.br_startoff);
                // How the list is spread over leaves.  One leaf holds `room`; a
                // list that will not fit one is **split in two** and the root grows
                // a second key and pointer, because that is the whole of what an
                // interior node over leaves is.  A third leaf would need a real
                // interior *block*, and a tree deep enough to need one is refused.
                let room = BmbtLeafBlock::max_records(sb.sb_blocksize as usize, sb.has_crc());
                let mut leaves: Vec<Vec<BmbtRec>> = Vec::new();
                if all.len() <= room {
                    leaves.push(all.clone());
                } else {
                    let max_leaves =
                        BmbtInteriorBlock::max_children(sb.sb_blocksize as usize, sb.has_crc());
                    let needed = all.len().div_ceil(room);
                    if needed > max_leaves {
                        return Err(FsError::fork_full(format!(
                            "a file's b-tree needs {} leaves and one interior node of {} blocks \
                             holds {}",
                            needed, sb.sb_blocksize, max_leaves
                        )));
                    }
                    // Spread evenly rather than filling each leaf and letting the
                    // last one hold the remainder: a leaf with a parent may not be
                    // less than half full, and the remainder is exactly what ends
                    // up below that.  `xfs_repair` names the failure -- "bad # of
                    // bmap records (7, min - 15, max - 30)" -- and then calls the
                    // whole fork bad.
                    let n = BmbtLeafBlock::leaves_for(
                        all.len(),
                        sb.sb_blocksize as usize,
                        sb.has_crc(),
                    )
                    .ok_or_else(|| {
                        FsError::fork_full(format!(
                            "{} extents cannot be divided among leaves of {} blocks legally",
                            all.len(),
                            sb.sb_blocksize
                        ))
                    })?;
                    let per = all.len().div_ceil(n);
                    for chunk in all.chunks(per) {
                        leaves.push(chunk.to_vec());
                    }
                }

                // The blocks the leaves live in: the one the root already named,
                // then one more per additional leaf.  Allocated before they are
                // written, because a block that is charged for and never written is
                // the state `xfs_repair` calls a node that belongs to no tree.
                let mut leaf_fsb: Vec<u64> = vec![blocks[0]];
                while leaf_fsb.len() < leaves.len() {
                    let run = allocate(&mut tx, &sb, agno, 1)?;
                    leaf_fsb.push(sb.ag_block_to_fsb(agno, run.start));
                }

                // The chain, so a reader walking siblings finds the leaves in
                // order.  Nulls on the outside, the neighbours inside.
                for (i, recs) in leaves.iter().enumerate() {
                    let leftsib = if i == 0 {
                        BMBT_NULL_PTR
                    } else {
                        leaf_fsb[i - 1]
                    };
                    let rightsib = if i + 1 == leaves.len() {
                        BMBT_NULL_PTR
                    } else {
                        leaf_fsb[i + 1]
                    };
                    let leaf = BmbtLeafBlock {
                        level:   0,
                        records: recs.iter().map(BmbtLeafRecord::from_extent).collect(),
                    };
                    let at = sb.fsb_to_offset(leaf_fsb[i]);
                    let block = if sb.has_crc() {
                        leaf.to_bytes(
                            &BtreeLblockHdr {
                                blkno: leaf_fsb[i],
                                lsn: 0,
                                leftsib,
                                rightsib,
                                owner: xfs_ino,
                                uuid: sb.uuid(),
                            },
                            sb.sb_blocksize as usize,
                        )
                    } else {
                        leaf.to_bytes_v4(leftsib, rightsib, sb.sb_blocksize as usize)
                    };
                    tx.write_bytes(at, &block)?;
                }

                // The root, and the level it sits at.
                //
                // One or two leaves live in the fork itself: the root names them
                // directly.  A third needs somewhere to put the list of children,
                // which is an interior **block** -- allocated, written, and named by
                // a root that then has one key and one pointer again.  The level is
                // what tells a reader which: 1 for leaves straight under the fork,
                // 2 for leaves under an interior block.
                let mut root = raw.data_btree_root()?;
                root.set_owner(xfs_ino);
                if leaves.len() <= 2 {
                    root.ptrs = leaf_fsb.clone();
                    root.keys = leaves
                        .iter()
                        .map(|l| BmbtKey {
                            br_startoff: l[0].br_startoff,
                        })
                        .collect();
                    root.bmdr.bb_level = 1;
                    root.bmdr.bb_numrecs = leaves.len() as u16;
                    raw.set_data_btree_root(&root)?;
                    nodes = leaf_fsb.len() as u64;
                } else {
                    let run = allocate(&mut tx, &sb, agno, 1)?;
                    let interior_fsb = sb.ag_block_to_fsb(agno, run.start);
                    let node = BmbtInteriorBlock {
                        level: 1,
                        keys:  leaves
                            .iter()
                            .map(|l| BmbtKey {
                                br_startoff: l[0].br_startoff,
                            })
                            .collect(),
                        ptrs:  leaf_fsb.clone(),
                    };
                    let block = if sb.has_crc() {
                        node.to_bytes(
                            &BtreeLblockHdr::for_single_leaf(interior_fsb, xfs_ino, sb.uuid()),
                            sb.sb_blocksize as usize,
                        )
                    } else {
                        node.to_bytes_v4(BMBT_NULL_PTR, BMBT_NULL_PTR, sb.sb_blocksize as usize)
                    };
                    tx.write_bytes(sb.fsb_to_offset(interior_fsb), &block)?;

                    root.ptrs = vec![interior_fsb];
                    root.keys = vec![BmbtKey {
                        br_startoff: leaves[0][0].br_startoff,
                    }];
                    root.bmdr.bb_level = 2;
                    root.bmdr.bb_numrecs = 1;
                    raw.set_data_btree_root(&root)?;
                    nodes = leaf_fsb.len() as u64 + 1;
                }

                data_blocks = all.iter().map(|r| r.br_blockcount).sum();
                // `di_nextents` is the file's **total** extent count, not the
                // records in the fork, so it has to follow a conversion and keep
                // following it.  Leaving it at the count the fork held when it
                // converted is what `xfs_repair` calls `bad nextents`.
                raw.set_nextents(all.len() as u64);
                raw.set_size(end as i64);
                raw.set_mtime(now);
                raw.set_ctime(now);
                raw.set_nblocks(data_blocks + nodes);
                raw.finalise();
                tx.write_bytes(inode_offset, raw.as_bytes())?;
                tx.commit()?;
                self.device.invalidate();
                let dinode = Dinode::from(self.device.by_ref(), &sb, xfs_ino);
                if let Some(oi) = self.open_files.get_mut(&ino) {
                    oi.dinode = dinode;
                }
                return Ok(data.len() as u32);
            }

            match raw.add_extent(first_new, fsb, run.len) {
                Ok(()) => {
                    data_blocks = raw
                        .core_extents()
                        .map(|e| e.iter().map(|r| r.br_blockcount).sum())
                        .unwrap_or(0);
                }
                // The inode's own fork is full, which is not the group running out
                // and not something a retry fixes.  The format's answer is to turn
                // the fork into a B+tree, which needs a block for the records and
                // a root in the inode.
                Err(FsError::ForkFull { .. }) => {
                    let all = {
                        let mut e = raw.core_extents().ok_or_else(|| {
                            FsError::corrupt("a data fork reported itself full and cannot be read")
                        })?;
                        e.push(BmbtRec {
                            br_startoff:   first_new,
                            br_startblock: fsb,
                            br_blockcount: u64::from(run.len),
                            br_flag:       false,
                        });
                        e.sort_by_key(|r| r.br_startoff);
                        e
                    };
                    data_blocks = all.iter().map(|r| r.br_blockcount).sum();
                    nodes = 1;

                    // A block for the records, taken from the group like any other.
                    let node = allocate(&mut tx, &sb, agno, 1)?;
                    let node_fsb = sb.ag_block_to_fsb(agno, node.start);

                    // A leaf that will not hold the list is refused here rather than discovered
                    // as a write past the end of the block: the step after this is a second
                    // leaf and an interior node, which is not built, and saying so is better
                    // than a slice range.
                    let room = BmbtLeafBlock::max_records(sb.sb_blocksize as usize, sb.has_crc());
                    if all.len() > room {
                        return Err(FsError::fork_full(format!(
                            "a file's b-tree needs {} extents in one leaf and a block of {} holds \
                             {}",
                            all.len(),
                            sb.sb_blocksize,
                            room
                        )));
                    }
                    let leaf = BmbtLeafBlock {
                        level:   0,
                        records: all.iter().map(BmbtLeafRecord::from_extent).collect(),
                    };
                    // A file system with checksums and one without have different
                    // block headers, down to the magic, and a version 5 leaf
                    // written into a version 4 file system is what `xfs_repair`
                    // calls `bad magic` and then, further down, a bad data fork.
                    let at = sb.fsb_to_offset(node_fsb);
                    let block = if sb.has_crc() {
                        let hdr = BtreeLblockHdr::for_single_leaf(node_fsb, xfs_ino, sb.uuid());
                        leaf.to_bytes(&hdr, sb.sb_blocksize as usize)
                    } else {
                        leaf.to_bytes_v4(BMBT_NULL_PTR, BMBT_NULL_PTR, sb.sb_blocksize as usize)
                    };
                    tx.write_bytes(at, &block)?;

                    raw.set_data_extents_as_tree(node_fsb, &all)?;
                }
                Err(e) => return Err(e),
            }
            // The file's own count of the blocks it occupies has to follow its
            // extents, or every reader of the file system -- including
            // xfs_repair -- will say the inode disagrees with its own data.
            //
            // It follows the *sum* of the extents' block counts and not the
            // number of extents, so it has to be recomputed even when the new
            // blocks were joined to an extent that was already there rather
            // than added beside it.  A file grown at its end lands next to its
            // own last block, and that is the ordinary case: the extent count
            // does not move, 24 more blocks are covered, and an inode still
            // carrying the old count is one `bad nblocks` away from being
            // repairable.
            // The file's block count is its data blocks **and** any node the tree
            // it now lives in.  Counting only the data leaves a tree charged for
            // nothing, which `xfs_repair` reports and which is how this code got
            // three of its "the header was read before the work was done" bugs.
            raw.set_nblocks(data_blocks + nodes);
        }
        raw.set_size(end as i64);
        raw.set_mtime(now);
        raw.set_ctime(now);
        raw.finalise();
        tx.write_bytes(inode_offset, raw.as_bytes())?;
        tx.commit()?;

        // The read side may be holding a copy of something that just changed, and
        // the cached inode is now behind the image.
        self.device.invalidate();
        self.device.set_bufsize(inode_size);
        let dinode = Dinode::from(self.device.by_ref(), &sb, xfs_ino);
        if let Some(oi) = self.open_files.get_mut(&ino) {
            oi.dinode = dinode;
        }
        Ok(data.len() as u32)
    }

    /// Add one entry to a **shortform** directory.
    ///
    /// The representation is mutated and re-serialised; persistence is the
    /// caller's transaction, which is the shape the rest of this file uses.
    ///
    /// It refuses rather than approximating in three cases, each of which would
    /// otherwise leave a directory `xfs_repair` accepts and that has lost something:
    ///
    /// * a directory that is **not** shortform.  Block, leaf and node directories are
    ///   a different format and are not implemented, which is `ENOSYS` -- not
    ///   `ENOSPC`, which would send the caller looking for space, and not `EINVAL`.
    /// * an **empty** directory, because the offset its first entry takes cannot be
    ///   derived: the base is 48 in one file system here and 96 in another and has
    ///   not been identified, and an empty directory is the one case that cannot show
    ///   it.  Inventing it would produce offsets nothing could explain.
    /// * data that does not fit, which `set_data_bytes` refuses for the same
    ///   reason: a shortform directory that outgrows its inode is a transition this
    ///   project has not built.
    #[allow(dead_code)] // Used as soon as create() and unlink() exist; only reachable then.
    fn add_dirent(
        &mut self,
        parent_ino: u64,
        name: &[u8],
        child_ino: u64,
        ftype: u8,
    ) -> FsResult<()> {
        use crate::libxfuse::dir3_sf::ShortformDirectory;
        let sb = self.sb;
        let inode_offset = sb.inode_offset(self.xfs_ino(parent_ino));
        let mut tx = self.begin();
        let bytes = tx.read_bytes(inode_offset, sb.inode_size())?;
        let mut raw = RawDinode::from_bytes(bytes)?;
        if raw.format() != 1 {
            return Err(FsError::unsupported(format!(
                "adding an entry to a directory whose data fork is in format {}",
                raw.format()
            )));
        }
        let fork = raw.data_bytes();
        let mut dir = ShortformDirectory::decode(&fork, sb.ftype())?;
        if dir.entries.is_empty() {
            return Err(FsError::unsupported(
                "adding the first entry to an empty shortform directory: the offset it takes is \
                 not derivable and has not been measured",
            ));
        }

        // Try to add the entry
        let first_offset = if dir.entries.is_empty() {
            dir.header_len() as u16
        } else {
            dir.entries.last().unwrap().offset + SF_OFFSET_STEP
        };

        // Try to add the entry to a test copy
        let mut test_dir = dir.clone();
        test_dir.add(name, ftype, child_ino, first_offset)?;

        // Check if we need to transition to block directory
        if test_dir.needs_transition(&sb, &raw) {
            // For now, return ENOSYS since full implementation needs block allocation
            // which requires transaction context
            Err(FsError::unsupported(
                "shortform to block directory transition not yet implemented",
            ))
        } else {
            // Just add the entry normally
            dir.add(name, ftype, child_ino, first_offset)?;
            let now = std::time::SystemTime::now();
            raw.set_data_bytes(&dir.serialize())?;
            raw.set_mtime(now);
            raw.set_ctime(now);
            raw.finalise();
            tx.write_bytes(inode_offset, raw.as_bytes())?;
            tx.commit()?;
            self.device.invalidate();
            Ok(())
        }
    }

    /// Remove one entry from a **shortform** directory.
    ///
    /// Every survivor is left byte for byte alone, because that is what the kernel
    /// does: removing the first and last entries of a four-entry directory moved the
    /// survivors from bytes 15 and 24 to 6 and 15 and left their stored offsets at
    /// 112 and 128.  A renumbering implementation would diverge from XFS on the
    /// first removal.
    #[allow(dead_code)] // Used as soon as create() and unlink() exist; only reachable then.
    fn remove_dirent(&mut self, parent_ino: u64, name: &[u8]) -> FsResult<()> {
        use crate::libxfuse::dir3_sf::ShortformDirectory;
        let sb = self.sb;
        let inode_offset = sb.inode_offset(self.xfs_ino(parent_ino));
        let mut tx = self.begin();
        let bytes = tx.read_bytes(inode_offset, sb.inode_size())?;
        let mut raw = RawDinode::from_bytes(bytes)?;
        if raw.format() != 1 {
            return Err(FsError::unsupported(format!(
                "removing an entry from a directory whose data fork is in format {}",
                raw.format()
            )));
        }
        let fork = raw.data_bytes();
        let mut dir = ShortformDirectory::decode(&fork, sb.ftype())?;
        dir.remove(name)?;
        let now = std::time::SystemTime::now();
        raw.set_data_bytes(&dir.serialize())?;
        raw.set_mtime(now);
        raw.set_ctime(now);
        raw.finalise();
        tx.write_bytes(inode_offset, raw.as_bytes())?;
        tx.commit()?;
        self.device.invalidate();
        Ok(())
    }

    /// Shorten a file, giving the blocks it no longer covers back to the group
    /// they came from.
    ///
    /// Growing is a different operation and not this one: a file made longer is
    /// made longer by being written to, and a `truncate` that made it longer would
    /// be inventing zeroed blocks nobody asked for.  So a request for a size at
    /// or above the current one only moves the size, and the space between the old
    /// end and the new one reads as zeroes because there is nothing there.
    ///
    /// Which is also the whole of what a sparse file is, so growing this way is
    /// right rather than a shortcut: the blocks are not allocated and the extents
    /// are not extended.
    ///
    /// The blocks that are given back are the ones whose *whole* extent is above
    /// the new end, plus the tail of the extent that straddles it.  Which blocks
    /// those are is decided from the inode's own extents, not from anything the
    /// caller says, and the two spaces an extent names are converted carefully:
    /// a file names image blocks and the allocator names blocks within a group,
    /// and one used where the other belongs points at the right block in the
    /// wrong group.
    ///
    /// Every count moves in the same transaction: the extents, the size, the
    /// block count and the times, and the group's free space.  An inode that has
    /// lost its extents but not its block count is what `xfs_repair` reports as
    /// `bad nblocks`, and an inode that still lists an extent whose blocks have
    /// been handed to something else is a file that reads another file's data.
    fn truncate(&mut self, ino: u64, new_size: u64) -> FsResult<u32> {
        use crate::libxfuse::alloc::allocator::free_in_group;
        if !self.writable {
            return Err(FsError::read_only("truncate"));
        }
        let sb = self.sb;
        let blocksize = u64::from(sb.sb_blocksize);
        let inode_size = sb.inode_size();
        let xfs_ino = self.xfs_ino(ino);
        let inode_offset = sb.inode_offset(xfs_ino);
        let size = {
            let oi = self
                .open_files
                .get_mut(&ino)
                .ok_or_else(|| no_entry(b"an inode the kernel has not looked up"))?;
            let dinode = &oi.dinode;
            if !matches!(
                dinode.di_core.di_format,
                XfsDinodeFmt::Extents | XfsDinodeFmt::Btree
            ) {
                return Err(FsError::invalid(
                    libc::ENOTSUP,
                    format!(
                        "truncating a file whose data fork is in format {:?}",
                        dinode.di_core.di_format
                    ),
                ));
            }
            u64::try_from(oi.dinode.fsize()).map_err(FsError::from)?
        };
        if new_size == size {
            return Ok(0);
        }
        // A file made longer reads as zeroes past its old end and occupies no
        // blocks for the space it does not have: nothing to do but the size.
        if new_size > size {
            let now = std::time::SystemTime::now();
            let mut tx = self.begin();
            let mut raw = RawDinode::from_bytes(tx.read_bytes(inode_offset, inode_size)?)?;
            raw.set_size(new_size as i64);
            raw.set_mtime(now);
            raw.set_ctime(now);
            raw.finalise();
            tx.write_bytes(inode_offset, raw.as_bytes())?;
            tx.commit()?;
            self.device.invalidate();
            let dinode = Dinode::from(self.device.by_ref(), &sb, xfs_ino);
            if let Some(oi) = self.open_files.get_mut(&ino) {
                oi.dinode = dinode;
            }
            return Ok(0);
        }
        // A file whose extents are in a B+tree is read out of the tree here,
        // **before** the transaction starts, because the walk needs a reader and
        // the transaction holds the only mutable one.  Nothing it reads can change
        // underneath it: it is all image content, and this transaction does not
        // write any of it until the walk is finished.
        let tree: Option<(Vec<crate::libxfuse::bmbt_rec::BmbtRec>, Vec<u64>)> = {
            let mut peek = vec![0u8; inode_size];
            self.device
                .device()
                .read_at(&mut peek, inode_offset)
                .map_err(|e| FsError::Corrupt {
                    what: format!("reading inode {xfs_ino} to look at its data fork: {e}"),
                })?;
            let probe = RawDinode::from_bytes(peek)?;
            if probe.format() == 2 {
                None
            } else {
                let mut root = probe.data_btree_root()?;
                root.set_owner(xfs_ino);
                let (bmx, nodes) = root.all_extents(self.device.by_ref())?;
                Some((bmx.extents().to_vec(), nodes))
            }
        };

        let now = std::time::SystemTime::now();
        let mut tx = self.begin();
        let mut raw = RawDinode::from_bytes(tx.read_bytes(inode_offset, sb.inode_size())?)?;

        // The last file block that still exists.  A file whose new size ends
        // exactly on a block boundary has no last block, and `div_ceil` says so.
        let last_block = new_size.div_ceil(blocksize);

        // The extents above the new end go, and the blocks they named come back.
        // `drop_extents_above` does the surgery and says which blocks that was,
        // because the extent list's shape is not the caller's business.
        //
        // The two spaces an extent names are converted carefully: a file names
        // image blocks and the allocator names blocks within a group, and a
        // group-relative number used as an absolute one points at the right block
        // in the *wrong group*.
        //
        // A file whose extents are in the inode is the easy case and the one that
        // has always worked.  A file whose extents are in a B+tree is read out of
        // the tree, cut the same way, and written **back into the inode**, which
        // turns its fork back into an `extents` one.
        //
        // That direction is chosen deliberately rather than because it is easier.
        // The alternative -- rewriting the tree with fewer leaves -- needs a
        // writer this code does not have, and a tree left with the extents it no
        // longer needs would keep a node charged for that names nothing.  Shrinking
        // back to the inode is a transition XFS itself makes when the list fits,
        // and it leaves nothing behind that a later reader cannot follow.
        //
        // If the survivors do **not* fit the fork, `set_core_extents` refuses, and
        // that refusal is the honest answer: the file keeps its tree and its blocks
        // rather than losing either to a list that cannot hold it.
        let (nodes, freed): (Vec<u64>, Vec<(u64, u32)>) = match tree {
            None => (Vec::new(), raw.drop_extents_above(last_block)?),
            Some((extents, nodes)) => {
                let (kept, freed) = RawDinode::split_extents_above(&extents, last_block);
                raw.set_data_extents_from_tree(&kept)?;
                (nodes, freed)
            }
        };
        let mask = u64::from(1u32 << sb.sb_agblklog) - 1;
        let freed: Vec<(u32, FreeRun)> = freed
            .into_iter()
            .map(|(fsb, len)| {
                (
                    (fsb >> sb.sb_agblklog) as u32,
                    FreeRun {
                        start: (fsb & mask) as u32,
                        len,
                    },
                )
            })
            .collect();

        for (ag, run) in freed {
            free_in_group(&mut tx, &sb, ag, run)?;
        }
        // The tree's own blocks, one at a time, because a node is a single block
        // and `free_in_group` is given a run.
        //
        // `agf_btreeblks` does not move: it counts the blocks held in a group's
        // *free space* trees (the published XFS documentation (XFS Algorithms and Data Structures, and the XFS pages at docs.kernel.org): "of blocks held in AGF btrees"), and
        // this is a file's b-map b-tree.  What moves is the file's own count,
        // below, which counts a node like any other block the file uses -- and that
        // is why giving these back and lowering `di_nblocks` are the same operation
        // and have to happen in the same transaction.
        for fsb in nodes {
            let ag = (fsb >> sb.sb_agblklog) as u32;
            let start = (fsb & mask) as u32;
            free_in_group(&mut tx, &sb, ag, FreeRun { start, len: 1 })?;
        }
        let blocks: u64 = raw
            .core_extents()
            .map(|extents| extents.iter().map(|e| e.br_blockcount).sum())
            .unwrap_or(0);
        raw.set_nblocks(blocks);
        raw.set_size(new_size as i64);
        raw.set_mtime(now);
        raw.set_ctime(now);
        raw.finalise();
        tx.write_bytes(inode_offset, raw.as_bytes())?;
        tx.commit()?;

        self.device.invalidate();
        self.device.set_bufsize(inode_size);
        let dinode = Dinode::from(self.device.by_ref(), &sb, xfs_ino);
        if let Some(oi) = self.open_files.get_mut(&ino) {
            oi.dinode = dinode;
        }
        Ok(0)
    }

    fn write_data(&mut self, ino: u64, offset: u64, data: &[u8]) -> FsResult<u32> {
        if !self.writable {
            return Err(FsError::read_only("write"));
        }
        let oi = self
            .open_files
            .get_mut(&ino)
            .ok_or_else(|| no_entry(b"an inode the kernel has not looked up"))?;

        #[allow(clippy::unnecessary_cast)]
        if oi.dinode.di_core.di_mode as u32 & S_IFMT as u32 != S_IFREG as u32 {
            return Err(FsError::invalid(
                libc::EBADF,
                "only regular files can be written to",
            ));
        }
        if oi.dinode.is_realtime() {
            return Err(FsError::unsupported(
                "writing to a file on a real-time device",
            ));
        }
        // The two fork formats that hold a mapping from logical blocks to
        // physical ones.  Anything else -- a local fork, a device inode -- has
        // no mapping to consult, and writing into it would be a guess.
        let format = oi.dinode.di_core.di_format;
        if !matches!(format, XfsDinodeFmt::Extents | XfsDinodeFmt::Btree) {
            return Err(FsError::unsupported(format!(
                "writing to a data fork in format {format:?}"
            )));
        }

        // Writing into a file whose mapping is a B+tree is allowed, and the reason
        // it used not be allowed is worth keeping: a b-map record's data-block
        // field was not attributed, so the reader returned a provisional value and
        // a write through a value known to be provisional is a write to an
        // arbitrary block.
        //
        // It **is** attributed now -- `btree.rs`, two 64-bit words with the block
        // number split across them, checked by re-encoding a leaf and getting the
        // file system's own bytes back -- so the reason has gone and the refusal
        // with it.  What replaces it is not a blank cheque but a test: the write
        // path below resolves the extent through the same reader that reads it, and
        // `xfs_repair` is what says whether the result is a file system.

        let size = u64::try_from(oi.dinode.fsize()).map_err(FsError::from)?;
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| FsError::invalid(libc::EFBIG, "write runs past the end of the file"))?;
        if data.is_empty() {
            return Ok(0);
        }
        if end > size {
            // Past the end of the file the write has to *become* part of the
            // file: blocks allocated, extents recorded, and a size to match.
            //
            // A refusal is logged, because the reasons here are the interesting
            // ones -- "this inode's data fork is full and here are the two numbers"
            // is the difference between a debuggable limit and `ENOSPC` from the
            // void, and it says which of the five candidates in
            // `docs/write-support-progress.md` was actually hit.
            return self
                .write_extending(ino, offset, end, data)
                .inspect_err(|e| {
                    warn!("inode {ino}: a write past its end at {offset}..{end} was refused: {e}");
                });
        }

        // Work out where every block of the write lands before writing any of
        // it.  A range that straddles a hole is refused whole, because
        // writing the part that happens to be mapped would leave the caller
        // believing the whole write happened.
        let blocksize = u64::from(self.sb.sb_blocksize);
        let mut runs: Vec<(u64, u64, u64)> = Vec::new();
        {
            self.device.set_bufsize(self.sb.inode_size());
            let file = oi.dinode.get_file().map_err(FsError::from)?;
            let mut pos = offset;
            while pos < end {
                let dblock = pos / blocksize;
                let within = pos % blocksize;
                let n = std::cmp::min(blocksize - within, end - pos);
                let (start, len) = file
                    .lookup(self.device.by_ref(), &self.sb, dblock, false)
                    .map_err(|errno| FsError::invalid(errno, "extent lookup failed"))?;
                let start = start.ok_or_else(|| {
                    FsError::invalid(
                        libc::ENXIO,
                        format!(
                            "block {dblock} of the file is a hole; writing into a hole needs an \
                             allocator"
                        ),
                    )
                })?;
                // The byte to write is `within` bytes into the block the
                // logical block starts at.
                let image_offset = self.sb.fsb_to_offset(start) + within;
                match runs.last_mut() {
                    // Keep a run going as long as the next piece of the write
                    // is the very next byte of the image.
                    Some(run) if run.0 + run.1 == image_offset && run.1 + n <= len * blocksize => {
                        run.1 += n;
                    }
                    _ => runs.push((image_offset, n, len * blocksize)),
                }
                pos += n;
            }
        }

        debug!(
            "writing {len} bytes to inode {ino} at offset {offset}: {runs:?}",
            len = data.len()
        );

        let inode_offset = self.sb.inode_offset(self.xfs_ino(ino));
        let inode_size = self.sb.inode_size();
        let now = std::time::SystemTime::now();
        let mut tx = self.begin();
        let mut written = 0u64;
        for (at, len, _run) in runs {
            let start = written as usize;
            let end = start + len as usize;
            tx.write_data(at, &data[start..end])?;
            written += len;
        }

        // The inode records when the file was last written, and when its
        // metadata last changed.  Both are now.
        {
            let raw = tx.read_bytes(inode_offset, inode_size)?;
            let mut raw = RawDinode::from_bytes(raw)?;
            raw.set_mtime(now);
            raw.set_ctime(now);
            raw.finalise();
            tx.write_bytes(inode_offset, raw.as_bytes())?;
        }
        tx.commit()?;

        // The read side may be holding a copy of a block that has just
        // changed, and the cached inode holds timestamps that have just
        // changed.  Both are replaced with what is now on the image.
        self.device.invalidate();
        self.device.set_bufsize(self.sb.inode_size());
        let xfs_ino = self.xfs_ino(ino);
        let sb = &self.sb;
        let dinode = Dinode::from(self.device.by_ref(), sb, xfs_ino);
        if let Some(oi) = self.open_files.get_mut(&ino) {
            oi.dinode = dinode;
        }
        Ok(written as u32)
    }
}

impl Filesystem for Volume {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let s = &mut *self.state.lock().unwrap();
        let parent_oi = &mut s.open_files.get_mut(&u64::from(parent)).unwrap();
        let dirsize = s.sb.sb_blocksize << s.sb.sb_dirblklog;
        s.device.set_bufsize(dirsize as usize);
        let dir = parent_oi.dinode.get_dir(s.device.by_ref(), &s.sb);
        match dir.lookup(s.device.by_ref(), &s.sb, name) {
            Ok(ino) => {
                let ttl = s.ttl();
                let oi = s.open_inode(ino);
                match oi.dinode.di_core.stat(ino) {
                    Ok(attr) => {
                        // We don't need to report the inode generation since this is a read-only
                        // file system.  But we'll do it anyway.
                        reply.entry(&ttl, &attr, Generation(oi.dinode.di_core.di_gen.into()))
                    }
                    Err(err) => reply.error(Errno::from_i32(err)),
                }
            }
            Err(err) => reply.error(Errno::from_i32(err)),
        }
    }

    fn lseek(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        let s = &mut *self.state.lock().unwrap();
        let uoffset = if let Ok(offs) = u64::try_from(offset) {
            offs
        } else {
            reply.error(Errno::EINVAL);
            return;
        };

        let oi = &mut s.open_files.get_mut(&u64::from(ino)).unwrap();
        if offset > oi.dinode.fsize() {
            reply.error(Errno::ENXIO);
            return;
        }

        match oi.dinode.lseek(s.device.by_ref(), uoffset, whence) {
            Ok(ofs) => reply.offset(i64::try_from(ofs).unwrap()),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        let s = &mut *self.state.lock().unwrap();
        if ino == INodeNo::ROOT {
            // Special case: since fusefs never does a lookup for the root
            // inode, its FORGETs may be "unmatched"
            return;
        }
        match s.open_files.get_mut(&u64::from(ino)) {
            Some(oi) => {
                oi.count -= nlookup;
                if oi.count == 0 {
                    s.open_files.remove(&u64::from(ino));
                } else {
                    // AFAICT the kernel will never send a partial forget.  Alert the admin if it
                    // ever happens.
                    warn!("Partial forget for ino {}", ino);
                }
            }
            None => warn!("Forget without lookup for inode {}", ino),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let s = &mut *self.state.lock().unwrap();
        let ttl = s.ttl();
        let attr = s
            .open_files
            .get(&u64::from(ino))
            .expect("getattr before lookup")
            .dinode
            .di_core
            .stat(u64::from(ino))
            .expect("Unknown file type");

        reply.attr(&ttl, &attr)
    }

    #[allow(clippy::unnecessary_cast)]
    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let s = &mut *self.state.lock().unwrap();
        if !s.writable {
            reply.error(Errno::EROFS);
            return;
        }
        if parent == INodeNo::ROOT {
            reply.error(Errno::EPERM);
            return;
        }
        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > 255 {
            reply.error(Errno::EINVAL);
            return;
        }
        if name_bytes == b"." || name_bytes == b".." {
            reply.error(Errno::EEXIST);
            return;
        }

        // Extract superblock values needed during the transaction.
        let sb = s.sb;
        let has_ftype = sb.has_ftype();
        let parent_ino = s.xfs_ino(u64::from(parent));

        // 1. Allocate an inode number (within existing chunk if possible)
        let agno = s.alloc_group_for_new_inode(parent_ino);
        let file_mode = (mode as u16) | (S_IFREG as u16);
        let uid = s.current_uid();
        let gid = s.current_gid();

        let mut tx = s.begin();
        let new_xfs_ino =
            match State::allocate_inode_in_group(&mut tx, &sb, agno, file_mode, uid, gid) {
                Ok(ino) => ino,
                Err(e) => {
                    tx.abort();
                    reply.error(e.to_errno());
                    return;
                }
            };
        let new_ino = State::make_fuse_ino_static(new_xfs_ino, &sb);

        // 2. Initialize the new inode
        let inode_offset = sb.inode_offset(new_xfs_ino);
        let inode_size = sb.inode_size();
        let mut raw = RawDinode::unused(inode_size);
        raw.set_version(RawDinode::version_for(inode_size));
        raw.set_mode(file_mode);
        raw.set_nlink(1);
        raw.set_uid(uid);
        raw.set_gid(gid);
        raw.set_size(0);
        raw.set_format(XfsDinodeFmt::Extents as u8);
        raw.set_forkoff(0);
        raw.set_magic(XFS_DINODE_MAGIC);
        // A newly allocated inode must have next_unlinked set to the end-of-list
        // marker (0xffffffff), not zero.  Zero triggers "bad next_unlinked 0x0".
        raw.set_next_unlinked();
        // Attribute fork format: 2 (Extents) to match data fork, with
        // forkoff=0 indicating no attribute fork space is allocated.
        raw.set_aformat(XfsDinodeFmt::Extents as u8);
        let now = std::time::SystemTime::now();
        raw.set_mtime(now);
        raw.set_ctime(now);
        raw.set_atime(now);
        raw.finalise();

        // Write the inode
        if let Err(e) = tx.write_bytes(inode_offset, raw.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // 3. Insert directory entry in parent
        let parent_offset = sb.inode_offset(parent_ino);
        let parent_inode_size = sb.inode_size();
        let parent_bytes = match tx.read_bytes(parent_offset, parent_inode_size) {
            Ok(b) => b,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };
        let mut parent_raw = match RawDinode::from_bytes(parent_bytes) {
            Ok(r) => r,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        // Parent must be a shortform directory
        if parent_raw.format() != XfsDinodeFmt::Local as u8 {
            tx.abort();
            reply.error(Errno::ENOSYS);
            return;
        }

        let mut dir = match ShortformDirectory::decode(&parent_raw.data_bytes(), has_ftype) {
            Ok(d) => d,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        let first_offset = if dir.entries.is_empty() {
            dir.header_len() as u16
        } else {
            dir.entries.last().unwrap().offset + SF_OFFSET_STEP
        };
        if let Err(e) = dir.add(name_bytes, XFS_DIR3_FT_REG_FILE, new_xfs_ino, first_offset) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // Update parent timestamps and size
        parent_raw.set_mtime(now);
        parent_raw.set_ctime(now);
        if let Err(e) = parent_raw.set_data_bytes(dir.serialize().as_slice()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }
        parent_raw.finalise();

        if let Err(e) = tx.write_bytes(parent_offset, parent_raw.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // 5. Commit
        if let Err(e) = tx.commit() {
            reply.error(e.to_errno());
            return;
        }

        let ttl = s.ttl();

        // 4. Add inode to open files (after commit, so we can borrow self again)
        let oi = s.open_inode(new_ino);
        oi.dinode = Dinode::from_bytes(raw.as_bytes(), &sb, new_xfs_ino);

        let attr = oi.dinode.di_core.stat(new_ino).expect("new inode stat");
        reply.created(
            &ttl,
            &attr,
            Generation(0),
            FileHandle(0),
            FopenFlags::FOPEN_KEEP_CACHE,
        );
    }

    #[allow(clippy::unnecessary_cast)]
    fn mknod(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        let s = &mut *self.state.lock().unwrap();
        if !s.writable {
            reply.error(Errno::EROFS);
            return;
        }
        if parent == INodeNo::ROOT {
            reply.error(Errno::EPERM);
            return;
        }
        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > 255 {
            reply.error(Errno::EINVAL);
            return;
        }
        if name_bytes == b"." || name_bytes == b".." {
            reply.error(Errno::EEXIST);
            return;
        }

        // Only device files, FIFOs, and sockets are valid for mknod
        let file_type = mode & S_IFMT as u32;
        if file_type != libc::S_IFBLK as u32
            && file_type != libc::S_IFCHR as u32
            && file_type != libc::S_IFIFO as u32
            && file_type != libc::S_IFSOCK as u32
        {
            reply.error(Errno::EINVAL);
            return;
        }

        let sb = s.sb;
        let has_ftype = sb.has_ftype();
        let parent_ino = s.xfs_ino(u64::from(parent));

        let agno = s.alloc_group_for_new_inode(parent_ino);
        let uid = s.current_uid();
        let gid = s.current_gid();

        let mut tx = s.begin();
        let new_xfs_ino =
            match State::allocate_inode_in_group(&mut tx, &sb, agno, mode as u16, uid, gid) {
                Ok(ino) => ino,
                Err(e) => {
                    tx.abort();
                    reply.error(e.to_errno());
                    return;
                }
            };
        let new_ino = State::make_fuse_ino_static(new_xfs_ino, &sb);

        // Initialize the new inode as a device/FIFO/socket inode
        let inode_offset = sb.inode_offset(new_xfs_ino);
        let inode_size = sb.inode_size();
        let mut raw = RawDinode::unused(inode_size);
        raw.set_version(RawDinode::version_for(inode_size));
        raw.set_mode(mode as u16);
        raw.set_nlink(1);
        raw.set_uid(uid);
        raw.set_gid(gid);
        raw.set_size(0);
        raw.set_format(XfsDinodeFmt::Dev as u8);
        raw.set_forkoff(0);
        raw.set_magic(XFS_DINODE_MAGIC);
        raw.set_next_unlinked();
        raw.set_aformat(XfsDinodeFmt::Extents as u8);
        raw.set_dev(rdev as u64);
        let now = std::time::SystemTime::now();
        raw.set_mtime(now);
        raw.set_ctime(now);
        raw.set_atime(now);
        raw.finalise();

        if let Err(e) = tx.write_bytes(inode_offset, raw.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // Insert directory entry in parent
        let parent_offset = sb.inode_offset(parent_ino);
        let parent_bytes = match tx.read_bytes(parent_offset, inode_size) {
            Ok(b) => b,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };
        let mut parent_raw = match RawDinode::from_bytes(parent_bytes) {
            Ok(r) => r,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        if parent_raw.format() != XfsDinodeFmt::Local as u8 {
            tx.abort();
            reply.error(Errno::ENOSYS);
            return;
        }

        let dir_type = match file_type {
            x if x == libc::S_IFBLK as u32 => XFS_DIR3_FT_BLKDEV,
            x if x == libc::S_IFCHR as u32 => XFS_DIR3_FT_CHRDEV,
            x if x == libc::S_IFIFO as u32 => XFS_DIR3_FT_FIFO,
            x if x == libc::S_IFSOCK as u32 => XFS_DIR3_FT_SOCK,
            _ => XFS_DIR3_FT_UNKNOWN,
        };

        let mut dir = match ShortformDirectory::decode(&parent_raw.data_bytes(), has_ftype) {
            Ok(d) => d,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        let first_offset = if dir.entries.is_empty() {
            dir.header_len() as u16
        } else {
            dir.entries.last().unwrap().offset + SF_OFFSET_STEP
        };
        if let Err(e) = dir.add(name_bytes, dir_type, new_xfs_ino, first_offset) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        parent_raw.set_ctime(now);
        if let Err(e) = parent_raw.set_data_bytes(dir.serialize().as_slice()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }
        parent_raw.finalise();

        if let Err(e) = tx.write_bytes(parent_offset, parent_raw.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        if let Err(e) = tx.commit() {
            reply.error(e.to_errno());
            return;
        }

        let ttl = s.ttl();
        let oi = s.open_inode(new_ino);
        oi.dinode = Dinode::from_bytes(raw.as_bytes(), &sb, new_xfs_ino);
        let attr = oi.dinode.di_core.stat(new_ino).expect("new inode stat");
        reply.entry(&ttl, &attr, Generation(0));
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let s = &mut *self.state.lock().unwrap();
        if !s.writable {
            reply.error(Errno::EROFS);
            return;
        }
        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > 255 {
            reply.error(Errno::EINVAL);
            return;
        }
        if name_bytes == b"." || name_bytes == b".." {
            reply.error(Errno::EINVAL);
            return;
        }

        let sb = s.sb;
        let parent_ino = s.xfs_ino(u64::from(parent));

        // Look up the child inode in the parent directory
        let parent_oi = match s.open_files.get_mut(&u64::from(parent)) {
            Some(oi) => oi,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        // Get the child inode number
        let name_os = OsStr::from_bytes(name_bytes);
        let child_xfs_ino = match parent_oi.dinode.get_dir(s.device.by_ref(), &sb).lookup(
            s.device.by_ref(),
            &sb,
            name_os,
        ) {
            Ok(ino) => ino,
            Err(_) => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let _child_ino = s.make_fuse_ino(child_xfs_ino);

        // Read the child inode
        let child_offset = sb.inode_offset(child_xfs_ino);
        let child_size = sb.inode_size();
        let mut child_bytes = vec![0u8; child_size];
        if s.device
            .device()
            .read_at(&mut child_bytes, child_offset)
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        let mut child_raw = match RawDinode::from_bytes(child_bytes) {
            Ok(r) => r,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        // Check it's a regular file (not a directory)
        let mode = child_raw.mode();
        #[allow(clippy::unnecessary_cast)]
        if (mode as u32 & S_IFMT as u32) == S_IFDIR as u32 {
            reply.error(Errno::EISDIR);
            return;
        }

        let mut tx = s.begin();

        // 1. Remove directory entry from parent
        {
            let inode_offset = sb.inode_offset(parent_ino);
            let bytes = match tx.read_bytes(inode_offset, sb.inode_size()) {
                Ok(b) => b,
                Err(e) => {
                    tx.abort();
                    reply.error(e.to_errno());
                    return;
                }
            };
            let mut raw = match RawDinode::from_bytes(bytes) {
                Ok(r) => r,
                Err(e) => {
                    tx.abort();
                    reply.error(e.to_errno());
                    return;
                }
            };
            let fork = raw.data_bytes();
            let mut dir = match ShortformDirectory::decode(&fork, sb.ftype()) {
                Ok(d) => d,
                Err(e) => {
                    tx.abort();
                    reply.error(e.to_errno());
                    return;
                }
            };
            if let Err(e) = dir.remove(name_bytes) {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
            let now = std::time::SystemTime::now();
            if let Err(e) = raw.set_data_bytes(&dir.serialize()) {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
            raw.set_mtime(now);
            raw.set_ctime(now);
            raw.finalise();
            if let Err(e) = tx.write_bytes(inode_offset, raw.as_bytes()) {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        }

        // 2. Decrement link count
        let nlink = child_raw.nlink().saturating_sub(1);
        child_raw.set_nlink(nlink);

        // 3. If link count reaches 0, free the inode and its blocks
        if nlink == 0 {
            // Free the inode's data blocks
            let format = child_raw.format();
            if format == XfsDinodeFmt::Extents as u8 || format == XfsDinodeFmt::Btree as u8 {
                if let Some(extents) = child_raw.core_extents() {
                    for rec in extents {
                        let start = rec.br_startblock;
                        let len = rec.br_blockcount;
                        if len == 0 {
                            continue;
                        }
                        let run = crate::libxfuse::alloc::free_space::FreeRun {
                            start: start as u32,
                            len:   len as u32,
                        };
                        if let Err(e) = crate::libxfuse::alloc::allocator::free_in_group(
                            &mut tx,
                            &sb,
                            sb.locate_ino(child_xfs_ino).agno,
                            run,
                        ) {
                            tx.abort();
                            reply.error(e.to_errno());
                            return;
                        }
                    }
                }
            }

            // Free the inode in the INOBT (update AGI counts)
            let addr = sb.locate_ino(child_xfs_ino);
            let agno = addr.agno;
            let agi_offset = sb.ag_header_offset(agno, Sb::AGI_SECTOR);
            let agi_bytes = match tx.read_bytes(agi_offset, sb.sb_blocksize as usize) {
                Ok(b) => b,
                Err(e) => {
                    tx.abort();
                    reply.error(e.to_errno());
                    return;
                }
            };
            let mut agi =
                match crate::libxfuse::alloc::agi::Agi::from_bytes(agi_bytes, sb.has_crc()) {
                    Ok(a) => a,
                    Err(e) => {
                        tx.abort();
                        reply.error(e.to_errno());
                        return;
                    }
                };
            if let Err(e) = agi.set_free_inodes(agi.free_inodes() + 1) {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
            if let Err(e) = tx.write_bytes(agi_offset, agi.as_bytes()) {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }

            // Update superblock ifree count
            let mut sector = match tx.read_bytes(0, sb.sb_blocksize as usize) {
                Ok(b) => b,
                Err(e) => {
                    tx.abort();
                    reply.error(e.to_errno());
                    return;
                }
            };
            if let Err(e) = Sb::patch_ifree(&mut sector, 1) {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
            if let Err(e) = tx.write_bytes(0, &sector) {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }

            // Clear the inode's data
            child_raw.set_format(0);
            child_raw.set_size(0);
            child_raw.set_nblocks(0);
            child_raw.set_nlink(0);
            child_raw.set_mode(0);
            child_raw.set_uid(0);
            child_raw.set_gid(0);
            child_raw.set_magic(0);
            child_raw.set_next_unlinked();
        } else {
            // Just update timestamps
            let now = std::time::SystemTime::now();
            child_raw.set_mtime(now);
            child_raw.set_ctime(now);
        }

        // 4. Write the child inode
        let child_offset = sb.inode_offset(child_xfs_ino);
        if let Err(e) = tx.write_bytes(child_offset, child_raw.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // 5. Commit
        if let Err(e) = tx.commit() {
            reply.error(e.to_errno());
            return;
        }

        reply.ok();
    }

    #[allow(clippy::unnecessary_cast)]
    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let s = &mut *self.state.lock().unwrap();
        if !s.writable {
            reply.error(Errno::EROFS);
            return;
        }
        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > 255 {
            reply.error(Errno::EINVAL);
            return;
        }
        if name_bytes == b"." || name_bytes == b".." {
            reply.error(Errno::EEXIST);
            return;
        }

        let sb = s.sb;
        let has_ftype = sb.has_ftype();
        let parent_ino = s.xfs_ino(u64::from(parent));

        // 1. Allocate an inode for the new directory
        let agno = s.alloc_group_for_new_inode(parent_ino);
        let dir_mode = (mode as u16) | (S_IFDIR as u16);
        let uid = s.current_uid();
        let gid = s.current_gid();

        let mut tx = s.begin();
        let new_xfs_ino =
            match State::allocate_inode_in_group(&mut tx, &sb, agno, dir_mode, uid, gid) {
                Ok(ino) => ino,
                Err(e) => {
                    tx.abort();
                    reply.error(e.to_errno());
                    return;
                }
            };
        let new_ino = State::make_fuse_ino_static(new_xfs_ino, &sb);

        // 2. Initialize the new directory inode
        let inode_offset = sb.inode_offset(new_xfs_ino);
        let inode_size = sb.inode_size();
        let mut raw = RawDinode::unused(inode_size);
        raw.set_version(RawDinode::version_for(inode_size));
        raw.set_mode(dir_mode);
        raw.set_nlink(2); // . and ..
        raw.set_uid(uid);
        raw.set_gid(gid);
        raw.set_size(0);
        raw.set_format(XfsDinodeFmt::Local as u8);
        raw.set_forkoff(0);
        raw.set_magic(XFS_DINODE_MAGIC);
        raw.set_aformat(XfsDinodeFmt::Extents as u8);
        let now = std::time::SystemTime::now();
        raw.set_mtime(now);
        raw.set_ctime(now);
        raw.set_atime(now);
        raw.finalise();

        // 3. Create shortform directory with . and .. entries
        let mut dir = ShortformDirectory::new(has_ftype);
        let dot_offset = dir.header_len() as u16;
        if let Err(e) = dir.add(b".", XFS_DIR3_FT_DIR, new_xfs_ino, dot_offset) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }
        let dotdot_offset = dir.entries.last().unwrap().offset + SF_OFFSET_STEP;
        if let Err(e) = dir.add(b"..", XFS_DIR3_FT_DIR, parent_ino, dotdot_offset) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // Write directory data into the inode
        if let Err(e) = raw.set_data_bytes(dir.serialize().as_slice()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // Write the new directory inode
        if let Err(e) = tx.write_bytes(inode_offset, raw.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // 4. Insert directory entry in parent
        let parent_offset = sb.inode_offset(parent_ino);
        let parent_inode_size = sb.inode_size();
        let parent_bytes = match tx.read_bytes(parent_offset, parent_inode_size) {
            Ok(b) => b,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };
        let mut parent_raw = match RawDinode::from_bytes(parent_bytes) {
            Ok(r) => r,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        if parent_raw.format() != XfsDinodeFmt::Local as u8 {
            tx.abort();
            reply.error(Errno::ENOSYS);
            return;
        }

        let mut parent_dir = match ShortformDirectory::decode(&parent_raw.data_bytes(), has_ftype) {
            Ok(d) => d,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        let first_offset = if parent_dir.entries.is_empty() {
            parent_dir.header_len() as u16
        } else {
            parent_dir.entries.last().unwrap().offset + SF_OFFSET_STEP
        };
        if let Err(e) = parent_dir.add(name_bytes, XFS_DIR3_FT_DIR, new_xfs_ino, first_offset) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // Update parent timestamps, size, and nlink (for .. entry)
        parent_raw.set_mtime(now);
        parent_raw.set_ctime(now);
        let parent_nlink = parent_raw.nlink() + 1;
        parent_raw.set_nlink(parent_nlink);
        if let Err(e) = parent_raw.set_data_bytes(parent_dir.serialize().as_slice()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        if let Err(e) = tx.write_bytes(parent_offset, parent_raw.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // 5. Commit
        if let Err(e) = tx.commit() {
            reply.error(e.to_errno());
            return;
        }

        let ttl = s.ttl();
        let oi = s.open_inode(new_ino);
        oi.dinode = Dinode::from_bytes(raw.as_bytes(), &sb, new_xfs_ino);
        let attr = oi.dinode.di_core.stat(new_ino).expect("new inode stat");
        reply.entry(&ttl, &attr, Generation(0));
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let s = &mut *self.state.lock().unwrap();
        if !s.writable {
            reply.error(Errno::EROFS);
            return;
        }
        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > 255 {
            reply.error(Errno::EINVAL);
            return;
        }
        if name_bytes == b"." || name_bytes == b".." {
            reply.error(Errno::EINVAL);
            return;
        }

        let sb = s.sb;
        let parent_ino = s.xfs_ino(u64::from(parent));

        // Look up the child inode in the parent directory
        let parent_oi = match s.open_files.get_mut(&u64::from(parent)) {
            Some(oi) => oi,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let child_xfs_ino = match parent_oi.dinode.get_dir(s.device.by_ref(), &sb).lookup(
            s.device.by_ref(),
            &sb,
            OsStr::from_bytes(name_bytes),
        ) {
            Ok(ino) => ino,
            Err(_) => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        // Read the child inode
        let child_offset = sb.inode_offset(child_xfs_ino);
        let child_size = sb.inode_size();
        let mut child_bytes = vec![0u8; child_size];
        if s.device
            .device()
            .read_at(&mut child_bytes, child_offset)
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        let mut child_raw = match RawDinode::from_bytes(child_bytes) {
            Ok(r) => r,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        // Check it's a directory
        let mode = child_raw.mode();
        #[allow(clippy::unnecessary_cast)]
        if (mode as u32 & S_IFMT as u32) != S_IFDIR as u32 {
            reply.error(Errno::ENOTDIR);
            return;
        }

        // Check directory is empty (only . and .. entries)
        if child_raw.format() != XfsDinodeFmt::Local as u8 {
            reply.error(Errno::ENOTEMPTY);
            return;
        }
        let child_dir = match ShortformDirectory::decode(&child_raw.data_bytes(), sb.ftype()) {
            Ok(d) => d,
            Err(e) => {
                reply.error(e.to_errno());
                return;
            }
        };
        if child_dir.entries.len() != 2 {
            reply.error(Errno::ENOTEMPTY);
            return;
        }

        let mut tx = s.begin();

        // 1. Remove directory entry from parent
        {
            let inode_offset = sb.inode_offset(parent_ino);
            let bytes = match tx.read_bytes(inode_offset, sb.inode_size()) {
                Ok(b) => b,
                Err(e) => {
                    tx.abort();
                    reply.error(e.to_errno());
                    return;
                }
            };
            let mut raw = match RawDinode::from_bytes(bytes) {
                Ok(r) => r,
                Err(e) => {
                    tx.abort();
                    reply.error(e.to_errno());
                    return;
                }
            };
            let fork = raw.data_bytes();
            let mut dir = match ShortformDirectory::decode(&fork, sb.ftype()) {
                Ok(d) => d,
                Err(e) => {
                    tx.abort();
                    reply.error(e.to_errno());
                    return;
                }
            };
            if let Err(e) = dir.remove(name_bytes) {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
            let now = std::time::SystemTime::now();
            if let Err(e) = raw.set_data_bytes(&dir.serialize()) {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
            raw.set_mtime(now);
            raw.set_ctime(now);
            // Decrement parent nlink (the .. entry from child is gone)
            let parent_nlink = raw.nlink().saturating_sub(1);
            raw.set_nlink(parent_nlink);
            raw.finalise();
            if let Err(e) = tx.write_bytes(inode_offset, raw.as_bytes()) {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        }

        // 2. Free the directory's data blocks (none for shortform)
        // 3. Free the inode in the INOBT (update AGI counts)
        let addr = sb.locate_ino(child_xfs_ino);
        let agno = addr.agno;
        let agi_offset = sb.ag_header_offset(agno, Sb::AGI_SECTOR);
        let agi_bytes = match tx.read_bytes(agi_offset, sb.sb_blocksize as usize) {
            Ok(b) => b,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };
        let mut agi = match crate::libxfuse::alloc::agi::Agi::from_bytes(agi_bytes, sb.has_crc()) {
            Ok(a) => a,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };
        if let Err(e) = agi.set_free_inodes(agi.free_inodes() + 1) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }
        if let Err(e) = tx.write_bytes(agi_offset, agi.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // Update superblock ifree count
        let mut sector = match tx.read_bytes(0, sb.sb_blocksize as usize) {
            Ok(b) => b,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };
        if let Err(e) = Sb::patch_ifree(&mut sector, 1) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }
        if let Err(e) = tx.write_bytes(0, &sector) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // Clear the inode's data
        child_raw.set_format(0);
        child_raw.set_size(0);
        child_raw.set_nblocks(0);
        child_raw.set_nlink(0);
        child_raw.set_mode(0);
        child_raw.set_uid(0);
        child_raw.set_gid(0);
        child_raw.set_magic(0);
        child_raw.set_next_unlinked();

        // 4. Write the child inode
        if let Err(e) = tx.write_bytes(child_offset, child_raw.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // 5. Commit
        if let Err(e) = tx.commit() {
            reply.error(e.to_errno());
            return;
        }

        reply.ok();
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let s = &mut *self.state.lock().unwrap();
        if !s.writable {
            reply.error(Errno::EROFS);
            return;
        }
        let name_bytes = name.as_bytes();
        let newname_bytes = newname.as_bytes();
        if name_bytes.is_empty()
            || name_bytes.len() > 255
            || newname_bytes.is_empty()
            || newname_bytes.len() > 255
        {
            reply.error(Errno::EINVAL);
            return;
        }
        if name_bytes == b"."
            || name_bytes == b".."
            || newname_bytes == b"."
            || newname_bytes == b".."
        {
            reply.error(Errno::EINVAL);
            return;
        }

        let sb = s.sb;
        let has_ftype = sb.has_ftype();

        // Extract inode numbers before starting transaction to avoid borrow conflicts
        let src_parent_ino = s.xfs_ino(u64::from(parent));
        let dst_parent_ino = s.xfs_ino(u64::from(newparent));

        let mut tx = s.begin();

        // Read source parent directory
        let src_parent_offset = sb.inode_offset(src_parent_ino);
        let parent_inode_size = sb.inode_size();
        let src_parent_bytes = match tx.read_bytes(src_parent_offset, parent_inode_size) {
            Ok(b) => b,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };
        let mut src_parent_raw = match RawDinode::from_bytes(src_parent_bytes) {
            Ok(r) => r,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        if src_parent_raw.format() != XfsDinodeFmt::Local as u8 {
            tx.abort();
            reply.error(Errno::ENOSYS);
            return;
        }

        let mut src_dir = match ShortformDirectory::decode(&src_parent_raw.data_bytes(), has_ftype)
        {
            Ok(d) => d,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        // Find the entry to move
        let entry_idx = match src_dir.entries.iter().position(|e| e.name == name_bytes) {
            Some(idx) => idx,
            None => {
                tx.abort();
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let entry = src_dir.entries[entry_idx].clone();

        // 2. Read destination parent directory
        let dst_parent_offset = sb.inode_offset(dst_parent_ino);
        let dst_parent_bytes = match tx.read_bytes(dst_parent_offset, parent_inode_size) {
            Ok(b) => b,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };
        let mut dst_parent_raw = match RawDinode::from_bytes(dst_parent_bytes) {
            Ok(r) => r,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        if dst_parent_raw.format() != XfsDinodeFmt::Local as u8 {
            tx.abort();
            reply.error(Errno::ENOSYS);
            return;
        }

        let mut dst_dir = match ShortformDirectory::decode(&dst_parent_raw.data_bytes(), has_ftype)
        {
            Ok(d) => d,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        // Check if destination name already exists
        if dst_dir.entries.iter().any(|e| e.name == newname_bytes) {
            tx.abort();
            reply.error(Errno::EEXIST);
            return;
        }

        // 3. Remove from source directory
        if let Err(e) = src_dir.remove(name_bytes) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // 4. Add to destination directory with new name
        let first_offset = if dst_dir.entries.is_empty() {
            dst_dir.header_len() as u16
        } else {
            dst_dir.entries.last().unwrap().offset + SF_OFFSET_STEP
        };
        if let Err(e) = dst_dir.add(newname_bytes, entry.ftype, entry.inumber, first_offset) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        let now = std::time::SystemTime::now();

        // 5. Update source parent
        src_parent_raw.set_mtime(now);
        src_parent_raw.set_ctime(now);
        if let Err(e) = src_parent_raw.set_data_bytes(src_dir.serialize().as_slice()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }
        if let Err(e) = tx.write_bytes(src_parent_offset, src_parent_raw.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // 6. Update destination parent
        dst_parent_raw.set_mtime(now);
        dst_parent_raw.set_ctime(now);
        if let Err(e) = dst_parent_raw.set_data_bytes(dst_dir.serialize().as_slice()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }
        if let Err(e) = tx.write_bytes(dst_parent_offset, dst_parent_raw.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // 7. Commit
        if let Err(e) = tx.commit() {
            reply.error(e.to_errno());
            return;
        }

        reply.ok();
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let s = &mut *self.state.lock().unwrap();
        if !s.writable {
            reply.error(Errno::EROFS);
            return;
        }
        let newname_bytes = newname.as_bytes();
        if newname_bytes.is_empty() || newname_bytes.len() > 255 {
            reply.error(Errno::EINVAL);
            return;
        }
        if newname_bytes == b"." || newname_bytes == b".." {
            reply.error(Errno::EEXIST);
            return;
        }

        let sb = s.sb;
        let has_ftype = sb.has_ftype();
        let target_xfs_ino = s.xfs_ino(u64::from(ino));
        let newparent_xfs_ino = s.xfs_ino(u64::from(newparent));

        let mut tx = s.begin();

        // 1. Read target inode
        let target_offset = sb.inode_offset(target_xfs_ino);
        let inode_size = sb.inode_size();
        let target_bytes = match tx.read_bytes(target_offset, inode_size) {
            Ok(b) => b,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };
        let mut target_raw = match RawDinode::from_bytes(target_bytes) {
            Ok(r) => r,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        // Check it's not a directory (can't hardlink directories)
        let mode = target_raw.mode();
        #[allow(clippy::unnecessary_cast)]
        if (mode as u32 & S_IFMT as u32) == S_IFDIR as u32 {
            tx.abort();
            reply.error(Errno::EPERM);
            return;
        }

        // 2. Read new parent directory
        let parent_offset = sb.inode_offset(newparent_xfs_ino);
        let parent_bytes = match tx.read_bytes(parent_offset, inode_size) {
            Ok(b) => b,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };
        let mut parent_raw = match RawDinode::from_bytes(parent_bytes) {
            Ok(r) => r,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        if parent_raw.format() != XfsDinodeFmt::Local as u8 {
            tx.abort();
            reply.error(Errno::ENOSYS);
            return;
        }

        let mut dir = match ShortformDirectory::decode(&parent_raw.data_bytes(), has_ftype) {
            Ok(d) => d,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        // Check if name already exists
        if dir.entries.iter().any(|e| e.name == newname_bytes) {
            tx.abort();
            reply.error(Errno::EEXIST);
            return;
        }

        // 3. Add directory entry
        // On FreeBSD, S_IF* constants are u16; we need u32 for bitwise operations.
        #[allow(clippy::unnecessary_cast)]
        const S_IFMT_U32: u32 = S_IFMT as u32;
        #[allow(clippy::unnecessary_cast)]
        const S_IFREG_U32: u32 = S_IFREG as u32;
        #[allow(clippy::unnecessary_cast)]
        const S_IFDIR_U32: u32 = S_IFDIR as u32;
        #[allow(clippy::unnecessary_cast)]
        const S_IFLNK_U32: u32 = S_IFLNK as u32;
        #[allow(clippy::unnecessary_cast)]
        const S_IFCHR_U32: u32 = S_IFCHR as u32;
        #[allow(clippy::unnecessary_cast)]
        const S_IFBLK_U32: u32 = S_IFBLK as u32;
        #[allow(clippy::unnecessary_cast)]
        const S_IFIFO_U32: u32 = S_IFIFO as u32;
        #[allow(clippy::unnecessary_cast)]
        const S_IFSOCK_U32: u32 = S_IFSOCK as u32;

        let file_type = (mode as u32) & S_IFMT_U32;
        let ftype = if file_type == S_IFREG_U32 {
            XFS_DIR3_FT_REG_FILE
        } else if file_type == S_IFDIR_U32 {
            XFS_DIR3_FT_DIR
        } else if file_type == S_IFLNK_U32 {
            XFS_DIR3_FT_SYMLINK
        } else if file_type == S_IFCHR_U32 {
            XFS_DIR3_FT_CHRDEV
        } else if file_type == S_IFBLK_U32 {
            XFS_DIR3_FT_BLKDEV
        } else if file_type == S_IFIFO_U32 {
            XFS_DIR3_FT_FIFO
        } else if file_type == S_IFSOCK_U32 {
            XFS_DIR3_FT_SOCK
        } else {
            XFS_DIR3_FT_UNKNOWN
        };
        let first_offset = if dir.entries.is_empty() {
            dir.header_len() as u16
        } else {
            dir.entries.last().unwrap().offset + SF_OFFSET_STEP
        };
        if let Err(e) = dir.add(newname_bytes, ftype, target_xfs_ino, first_offset) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // 4. Update target inode: increment nlink, update ctime
        let new_nlink = target_raw.nlink() + 1;
        target_raw.set_nlink(new_nlink);
        let now = std::time::SystemTime::now();
        target_raw.set_ctime(now);
        target_raw.finalise();

        if let Err(e) = tx.write_bytes(target_offset, target_raw.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // 5. Update parent directory
        parent_raw.set_mtime(now);
        parent_raw.set_ctime(now);
        if let Err(e) = parent_raw.set_data_bytes(dir.serialize().as_slice()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }
        if let Err(e) = tx.write_bytes(parent_offset, parent_raw.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // 6. Commit
        if let Err(e) = tx.commit() {
            reply.error(e.to_errno());
            return;
        }

        // Return entry for new link
        let ttl = s.ttl();
        // Need to decode target inode with Dinode for stat
        let target_dinode = Dinode::from_bytes(target_raw.as_bytes(), &sb, target_xfs_ino);
        let attr = target_dinode
            .di_core
            .stat(target_xfs_ino)
            .expect("target inode stat");
        reply.entry(&ttl, &attr, Generation(0));
    }

    #[allow(clippy::unnecessary_cast)]
    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let s = &mut *self.state.lock().unwrap();
        if !s.writable {
            reply.error(Errno::EROFS);
            return;
        }
        let name_bytes = name.as_bytes();
        let target_bytes = target.as_os_str().as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > 255 {
            reply.error(Errno::EINVAL);
            return;
        }
        if name_bytes == b"." || name_bytes == b".." {
            reply.error(Errno::EEXIST);
            return;
        }
        if target_bytes.len() >= 1024 {
            reply.error(Errno::ENAMETOOLONG);
            return;
        }

        let sb = s.sb;
        let has_ftype = sb.has_ftype();
        let parent_ino = s.xfs_ino(u64::from(parent));

        // 1. Allocate an inode for the symlink
        let agno = s.alloc_group_for_new_inode(parent_ino);
        let symlink_mode = (S_IFLNK | 0o777) as u16; // symlinks typically have 777 permissions
        let uid = s.current_uid();
        let gid = s.current_gid();

        let mut tx = s.begin();
        let new_xfs_ino =
            match State::allocate_inode_in_group(&mut tx, &sb, agno, symlink_mode, uid, gid) {
                Ok(ino) => ino,
                Err(e) => {
                    tx.abort();
                    reply.error(e.to_errno());
                    return;
                }
            };
        let new_ino = State::make_fuse_ino_static(new_xfs_ino, &sb);

        // 2. Initialize the new symlink inode
        let inode_offset = sb.inode_offset(new_xfs_ino);
        let inode_size = sb.inode_size();
        let mut raw = RawDinode::unused(inode_size);
        raw.set_version(RawDinode::version_for(inode_size));
        raw.set_mode(symlink_mode);
        raw.set_nlink(1);
        raw.set_uid(uid);
        raw.set_gid(gid);
        raw.set_size(target_bytes.len() as i64);
        raw.set_format(XfsDinodeFmt::Local as u8);
        raw.set_forkoff(0);
        raw.set_magic(XFS_DINODE_MAGIC);
        raw.set_aformat(XfsDinodeFmt::Extents as u8);
        let now = std::time::SystemTime::now();
        raw.set_mtime(now);
        raw.set_ctime(now);
        raw.set_atime(now);

        // Store symlink target in data fork (shortform)
        if let Err(e) = raw.set_data_bytes(target_bytes) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }
        raw.finalise();

        // Write the inode
        if let Err(e) = tx.write_bytes(inode_offset, raw.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // 3. Insert directory entry in parent
        let parent_offset = sb.inode_offset(parent_ino);
        let parent_inode_size = sb.inode_size();
        let parent_bytes = match tx.read_bytes(parent_offset, parent_inode_size) {
            Ok(b) => b,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };
        let mut parent_raw = match RawDinode::from_bytes(parent_bytes) {
            Ok(r) => r,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        if parent_raw.format() != XfsDinodeFmt::Local as u8 {
            tx.abort();
            reply.error(Errno::ENOSYS);
            return;
        }

        let mut dir = match ShortformDirectory::decode(&parent_raw.data_bytes(), has_ftype) {
            Ok(d) => d,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        let first_offset = if dir.entries.is_empty() {
            dir.header_len() as u16
        } else {
            dir.entries.last().unwrap().offset + SF_OFFSET_STEP
        };
        if let Err(e) = dir.add(name_bytes, XFS_DIR3_FT_SYMLINK, new_xfs_ino, first_offset) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // Update parent timestamps and size
        parent_raw.set_mtime(now);
        parent_raw.set_ctime(now);
        if let Err(e) = parent_raw.set_data_bytes(dir.serialize().as_slice()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        if let Err(e) = tx.write_bytes(parent_offset, parent_raw.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // 4. Commit
        if let Err(e) = tx.commit() {
            reply.error(e.to_errno());
            return;
        }

        let ttl = s.ttl();
        let oi = s.open_inode(new_ino);
        oi.dinode = Dinode::from_bytes(raw.as_bytes(), &sb, new_xfs_ino);
        let attr = oi.dinode.di_core.stat(new_ino).expect("new symlink stat");
        reply.entry(&ttl, &attr, Generation(0));
    }

    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        // Open handles are only useful, and only correct, if the kernel sends
        // them.  A read-only mount has no reason to keep any, so it still asks
        // for the zero-message form and gets out of the open call entirely.
        let s = &mut *self.state.lock().unwrap();
        if !s.writable {
            if config
                .add_capabilities(InitFlags::FUSE_NO_OPEN_SUPPORT)
                .is_ok()
            {
                self.no_open.store(true, Ordering::Release);
            }
            if config
                .add_capabilities(InitFlags::FUSE_NO_OPENDIR_SUPPORT)
                .is_ok()
            {
                self.no_opendir.store(true, Ordering::Release);
            }
        }
        let _ =
            config.add_capabilities(InitFlags::FUSE_ASYNC_READ | InitFlags::FUSE_EXPORT_SUPPORT);
        Ok(())
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: fuser::ReplyData) {
        let s = &mut *self.state.lock().unwrap();
        s.device.set_bufsize(s.sb.sb_blocksize as usize);
        reply.data(
            s.open_files
                .get(&u64::from(ino))
                .expect("readlink before lookup")
                .dinode
                .get_link_data(s.device.by_ref(), &s.sb)
                .as_bytes(),
        );
    }

    /// Open a file, handing back a handle that later operations use.
    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let s = &mut *self.state.lock().unwrap();
        if self.no_open.load(Ordering::Acquire) {
            reply.error(Errno::ENOSYS);
            return;
        }
        let handle = s.next_handle;
        s.next_handle += 1;
        s.handles.insert(
            handle,
            OpenFile {
                ino:   u64::from(ino),
                flags: flags.0,
            },
        );
        reply.opened(FileHandle(handle), FopenFlags::FOPEN_KEEP_CACHE)
    }

    /// Write into a file that is already open.
    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let s = &mut *self.state.lock().unwrap();
        // A write has to be against a file the kernel actually opened for
        // writing.  Anything else is a request that does not make sense, and
        // answering it would mean writing to a file nobody asked us to write.
        match s.handles.get(&u64::from(fh)) {
            Some(of) if of.ino == u64::from(ino) => {
                if of.flags & libc::O_ACCMODE == libc::O_RDONLY {
                    reply.error(Errno::EBADF);
                    return;
                }
            }
            Some(_) => {
                reply.error(Errno::EBADF);
                return;
            }
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        }
        match s.write_data(u64::from(ino), offset, data) {
            Ok(n) => reply.written(n),
            Err(e) => {
                warn!(
                    "write of {} bytes to inode {ino} at {offset} failed: {e}",
                    data.len()
                );
                reply.error(e.to_errno())
            }
        }
    }

    /// Change a file's attributes.
    ///
    /// Only the size is honoured, and only as a truncation or a sparse extension:
    /// everything else in a `setattr` -- the mode, the owner, the times -- is a
    /// thing this file system does not do yet, and saying so is better than
    /// acknowledging the call and changing nothing.
    ///
    /// It answers `ENOTSUP` rather than `EOPNOTSUPP`'s more specific siblings so
    /// that a caller which does not ask for anything specific is told plainly that
    /// the operation is not available, rather than being left to guess which part
    /// of it was refused.
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let s = &mut *self.state.lock().unwrap();
        let _ = fh;
        // The kernel sends only the fields the caller actually asked to change, so
        // a request that mentions one of them is a request for it.  What this file
        // system cannot do is refused rather than acknowledged and ignored, which
        // would leave the caller believing something it did not get.
        //
        // The three timestamps it does not keep -- creation, attribute change and
        // backup -- are fields a version 4 inode does not have in the first place,
        // so a caller that sets them on this image is asking for something the
        // on-disk format cannot hold.
        let mut refused = Vec::new();
        for (asked, what) in [
            (flags.is_some(), "the flags"),
            (_ctime.is_some(), "the status change time"),
            (_crtime.is_some(), "the creation time"),
            (_chgtime.is_some(), "the attribute change time"),
            (_bkuptime.is_some(), "the backup time"),
        ] {
            if asked {
                refused.push(what);
            }
        }
        if !refused.is_empty() {
            warn!(
                "inode {ino}: cannot set {} on a file system whose inodes do not carry them; \
                 refusing the request",
                refused.join(", ")
            );
            return reply.error(Errno::ENOTSUP);
        }

        // A size change is the one field with real work behind it, and it refuses
        // what it cannot do itself -- a file whose mapping is a B+tree, for one --
        // so its error is the caller's error rather than this wrapper's.
        if let Some(wanted) = size {
            let current = match s.open_files.get_mut(&u64::from(ino)) {
                Some(oi) => oi.dinode.fsize() as u64,
                None => return reply.error(Errno::ENOENT),
            };
            if wanted != current {
                if let Err(e) = s.truncate(u64::from(ino), wanted) {
                    warn!("truncating inode {ino} to {wanted} failed: {e}");
                    return reply.error(e.to_errno());
                }
            }
        }

        // The rest is an inode update: the fields the caller named, and the status
        // change time that goes with every one of them.  `SystemTime::now()` for a
        // `TimeOrNow::Now`, and the named instant otherwise.
        let now = std::time::SystemTime::now();
        let stamp = |t: fuser::TimeOrNow| match t {
            fuser::TimeOrNow::SpecificTime(s) => s,
            fuser::TimeOrNow::Now => now,
        };
        let wants_inode =
            mode.is_some() || uid.is_some() || gid.is_some() || atime.is_some() || mtime.is_some();
        if wants_inode {
            if !s.open_files.contains_key(&u64::from(ino)) && ino != INodeNo::ROOT {
                return reply.error(Errno::ENOENT);
            }
            let xfs_ino = s.xfs_ino(u64::from(ino));
            let at = s.sb.inode_offset(xfs_ino);
            let inode_size = s.sb.inode_size();
            let mut tx = s.begin();
            let bytes = match tx.read_bytes(at, inode_size) {
                Ok(b) => b,
                Err(e) => {
                    warn!("inode {ino}: reading it at {at} failed: {e}");
                    return reply.error(e.to_errno());
                }
            };
            let mut raw = match RawDinode::from_bytes(bytes) {
                Ok(r) => r,
                Err(e) => {
                    warn!("inode {ino}: its {inode_size} bytes are not an inode: {e}");
                    return reply.error(e.to_errno());
                }
            };
            if let Some(m) = mode {
                // The inode's mode field carries the **file type as well as** the
                // permissions, and `xfs_repair` checks it: writing the permission
                // bits on their own leaves `0o1234` where `0o101234` was, and repair
                // then reports `bad inode type 0` and would clear the inode.  So the
                // type is kept from what the inode already had and only the
                // permissions are taken from the request -- which is also what
                // stops a `chmod` from turning a file into a directory.
                let type_bits = raw.mode() & !0o7777;
                raw.set_mode(type_bits | ((m as u16) & 0o7777));
                raw.set_ctime(now);
            }
            if let Some(u) = uid {
                raw.set_uid(u);
                raw.set_ctime(now);
            }
            if let Some(g) = gid {
                raw.set_gid(g);
                raw.set_ctime(now);
            }
            if let Some(t) = atime {
                raw.set_atime(stamp(t));
                raw.set_ctime(now);
            }
            if let Some(t) = mtime {
                raw.set_mtime(stamp(t));
                raw.set_ctime(now);
            }
            raw.finalise();
            if let Err(e) = tx.write_bytes(at, raw.as_bytes()) {
                warn!("inode {ino}: writing it back failed: {e}");
                return reply.error(e.to_errno());
            }
            if let Err(e) = tx.commit() {
                warn!("inode {ino}: committing the change failed: {e}");
                return reply.error(e.to_errno());
            }
            s.device.invalidate();
            s.device.set_bufsize(inode_size);
            let fresh = Dinode::from(s.device.by_ref(), &s.sb, xfs_ino);
            if let Some(oi) = s.open_files.get_mut(&u64::from(ino)) {
                oi.dinode = fresh;
            }
        }
        s.reply_attr_of(u64::from(ino), reply)
    }

    /// Called on every close of a file descriptor.  Nothing is left to do: each
    /// write was committed before the kernel was told it had succeeded, so by
    /// the time the last descriptor is closed there is nothing in flight.
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok()
    }

    /// Flush the image, if the caller wants the data to have reached the
    /// underlying storage.
    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let s = &mut *self.state.lock().unwrap();
        let result = s.tx.device().flush();
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    /// Flush directory changes, the directory analog of [`fsync`](Self::fsync).
    /// With no journal, flushing the underlying device is all there is to do.
    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let s = &mut *self.state.lock().unwrap();
        let result = s.tx.device().flush();
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    /// Close a file handle.
    #[allow(clippy::too_many_arguments)]
    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let s = &mut *self.state.lock().unwrap();
        s.handles.remove(&u64::from(fh));
        reply.ok()
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: fuser::ReplyData,
    ) {
        let s = &mut *self.state.lock().unwrap();
        let oi = &mut s.open_files.get_mut(&u64::from(ino)).unwrap();
        s.device.set_bufsize(s.sb.sb_blocksize as usize);

        let rtdev = if oi.dinode.is_realtime() {
            if let Some(rtd) = &mut s.rt_device {
                rtd.set_bufsize(s.sb.sb_blocksize as usize);
                Some(rtd.by_ref())
            } else {
                warn!("Realtime device not mounted");
                reply.error(Errno::ENXIO);
                return;
            }
        } else {
            None
        };
        match oi
            .dinode
            .read(s.device.by_ref(), rtdev, offset as i64, size)
        {
            Ok((v, ignore)) => reply.data(&v[ignore..]),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if self.no_opendir.load(Ordering::Acquire) {
            reply.error(Errno::ENOSYS)
        } else {
            reply.opened(FileHandle(0), FopenFlags::FOPEN_CACHE_DIR)
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let s = &mut *self.state.lock().unwrap();
        let dirsize = s.sb.sb_blocksize << s.sb.sb_dirblklog;
        s.device.set_bufsize(dirsize as usize);
        let oi = &mut s.open_files.get_mut(&u64::from(ino)).unwrap();

        let dir = oi.dinode.get_dir(s.device.by_ref(), &s.sb);

        let mut off = offset as i64;
        loop {
            let res = dir.next(s.device.by_ref(), &s.sb, off);
            match res {
                Ok((ino, offset, kind, name)) => {
                    // FUSE requires the file system's root directory to have a
                    // fixed inode number.
                    let ino = if ino == s.sb.sb_rootino { 1 } else { ino };
                    let kind = match kind {
                        Some(kind) => kind,
                        None => {
                            // This is very inefficient.  Frequently, getattr will be called for
                            // every entry returned by readdir.  In such cases, this code will read
                            // the inode twice.  The best solution is for everybody to use the
                            // ftype option in their XFS format.
                            s.device.set_bufsize(s.sb.inode_size());
                            let dinode = Dinode::from(
                                s.device.by_ref(),
                                &s.sb,
                                if ino == 1 {
                                    s.sb.sb_rootino
                                } else {
                                    ino as XfsIno
                                },
                            );
                            match dinode.di_core.stat(ino) {
                                Ok(attr) => attr.kind,
                                Err(e) => {
                                    reply.error(Errno::from_i32(e));
                                    return;
                                }
                            }
                        }
                    };
                    let res = reply.add(INodeNo(ino), offset as u64, kind, name);
                    if res {
                        reply.ok();
                        return;
                    }
                    off = offset;
                }
                // Errors other than ENOENT should be reported to the caller.
                Err(e) => {
                    if e == libc::ENOENT {
                        reply.ok();
                    } else {
                        reply.error(Errno::from_i32(e));
                    }
                    return;
                }
            }
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let s = &mut *self.state.lock().unwrap();
        reply.statfs(
            s.sb.sb_dblocks - u64::from(s.sb.sb_logblocks),
            s.sb.sb_fdblocks,
            s.sb.sb_fdblocks,
            s.sb.sb_icount,
            s.sb.sb_ifree,
            s.sb.sb_blocksize,
            255,
            s.sb.sb_blocksize,
        )
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let s = &mut *self.state.lock().unwrap();
        let mut nameparts = name.as_bytes().splitn(2, |c| *c == b'.');
        let _namespace = nameparts.next().unwrap();
        let name = OsStr::from_bytes(nameparts.next().unwrap());

        let oi = &mut s.open_files.get_mut(&u64::from(ino)).unwrap();
        s.device.set_bufsize(s.sb.sb_blocksize as usize);
        match oi.dinode.get_attrs(s.device.by_ref(), &s.sb) {
            Some(attrs) => match attrs.get(s.device.by_ref(), &s.sb, name) {
                Ok(value) => {
                    let len: u32 = value.len().try_into().unwrap();
                    if size == 0 {
                        reply.size(len);
                    } else if len > size {
                        reply.error(Errno::from_i32(ERANGE));
                    } else {
                        reply.data(value.as_slice())
                    }
                }
                Err(e) => reply.error(Errno::from_i32(e)),
            },
            None => {
                reply.error(Errno::from_i32(crate::libxfuse::ENOATTR));
            }
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let s = &mut *self.state.lock().unwrap();
        let oi = &mut s
            .open_files
            .get_mut(&u64::from(ino))
            .expect("listxattr before lookup");
        match oi.dinode.get_attrs(s.device.by_ref(), &s.sb) {
            Some(ref mut attrs) => {
                let attrs_size = attrs.get_total_size(s.device.by_ref(), &s.sb);

                if size == 0 {
                    reply.size(attrs_size);
                    return;
                }

                if attrs_size > size {
                    reply.error(Errno::from_i32(ERANGE));
                    return;
                }

                let list = attrs.list(s.device.by_ref(), &s.sb);
                // Assert that we calculated the list size correctly.  This assertion is only
                // safe since we're a read-only file system.
                assert_eq!(
                    list.len(),
                    attrs_size as usize,
                    "size calculation was wrong!"
                );
                reply.data(list.as_slice());
            }
            None => {
                // An inode with no attribute fork has an empty list, and the two
                // ways of asking for an empty list are not the same reply.  A
                // caller that passed a buffer is asking for the names, and the
                // answer is "there are none": an empty list.  Replying with a
                // *size* instead is what `getxattr` above does only when the
                // caller passed no buffer at all, and sending it either way makes
                // Linux's `fuse_listxattr_write` fail the copy out and report
                // EIO -- which is what `lsextattr::empty` saw, for a file that
                // has no attributes to get wrong.
                if size == 0 {
                    reply.size(0);
                } else {
                    reply.data(&[]);
                }
            }
        }
    }

    fn fallocate(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        let s = &mut *self.state.lock().unwrap();
        if !s.writable {
            reply.error(Errno::EROFS);
            return;
        }

        if length == 0 {
            reply.ok();
            return;
        }

        // Compute xfs_ino before borrowing self mutably via open_files.
        let xfs_ino = s.xfs_ino(u64::from(ino));
        let sb = s.sb;
        let blocksize = u64::from(sb.sb_blocksize);
        let inode_size = sb.inode_size();
        let inode_offset = sb.inode_offset(xfs_ino);

        let oi = match s.open_files.get_mut(&u64::from(ino)) {
            Some(oi) => oi,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let dinode = &oi.dinode;
        let format = dinode.di_core.di_format;
        if !matches!(format, XfsDinodeFmt::Extents | XfsDinodeFmt::Btree) {
            reply.error(Errno::ENOTSUP);
            return;
        }

        if dinode.is_realtime() {
            reply.error(Errno::ENOTSUP);
            return;
        }

        // Handle mutually exclusive modes
        let keep_size = (mode & FALLOC_FL_KEEP_SIZE) != 0;
        let punch_hole = (mode & FALLOC_FL_PUNCH_HOLE) != 0;
        let zero_range = (mode & FALLOC_FL_ZERO_RANGE) != 0;
        let collapse_range = (mode & FALLOC_FL_COLLAPSE_RANGE) != 0;
        let insert_range = (mode & FALLOC_FL_INSERT_RANGE) != 0;
        let unshare_range = (mode & FALLOC_FL_UNSHARE_RANGE) != 0;
        let write_zeroes = (mode & FALLOC_FL_WRITE_ZEROES) != 0;

        // Only KEEP_SIZE and PUNCH_HOLE are implemented
        if collapse_range || insert_range || unshare_range || write_zeroes {
            reply.error(Errno::ENOTSUP);
            return;
        }

        // ZERO_RANGE requires KEEP_SIZE
        if zero_range && !keep_size {
            reply.error(Errno::EINVAL);
            return;
        }

        // PUNCH_HOLE requires KEEP_SIZE (per Linux fallocate API) and cannot
        // be combined with ZERO_RANGE
        if punch_hole && (!keep_size || zero_range) {
            reply.error(Errno::EINVAL);
            return;
        }

        let end = offset.saturating_add(length);

        // Convert byte offsets to file block offsets
        let startoff = offset.div_ceil(blocksize);
        let endoff = end.div_ceil(blocksize);

        if punch_hole {
            // Punch hole: free blocks in [offset, end)
            // Read the inode first (before transaction)
            let mut peek = vec![0u8; inode_size];
            if let Err(e) = s.device.device().read_at(&mut peek, inode_offset) {
                reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                return;
            }

            if dinode.di_core.di_format == XfsDinodeFmt::Btree {
                let mut raw_inode = match RawDinode::from_bytes(peek) {
                    Ok(ri) => ri,
                    Err(e) => {
                        reply.error(e.to_errno());
                        return;
                    }
                };

                // Get the BtreeRoot from the inode
                let btree_root_result = raw_inode.data_btree_root();
                let mut btree_root = match btree_root_result {
                    Ok(mut root) => {
                        root.set_owner(xfs_ino);
                        root
                    }
                    Err(e) => {
                        reply.error(e.to_errno());
                        return;
                    }
                };

                // Now start transaction and do the deletion
                // Get device Arc before transaction to avoid borrow conflict
                let device_arc = s.device.device();
                let mut buf_reader = BlockReader::from_device(device_arc);
                let mut tx = s.begin();

                // Delete extents in range
                let freed = match btree_root.delete_extents_in_range(
                    &mut buf_reader,
                    startoff,
                    endoff,
                    &sb,
                    &mut tx,
                ) {
                    Ok(f) => f,
                    Err(e) => {
                        tx.abort();
                        reply.error(Errno::from_i32(e));
                        return;
                    }
                };

                // Free the blocks in the allocator
                let mask = u64::from(1u32 << sb.sb_agblklog) - 1;
                for (fsb, len) in freed {
                    let ag = (fsb >> sb.sb_agblklog) as u32;
                    let start = (fsb & mask) as u32;
                    if let Err(e) = crate::libxfuse::alloc::allocator::free_in_group(
                        &mut tx,
                        &sb,
                        ag,
                        crate::libxfuse::alloc::free_space::FreeRun { start, len },
                    ) {
                        tx.abort();
                        reply.error(e.to_errno());
                        return;
                    }
                }

                // Update inode
                // Recalculate blocks from the btree extent map (through tx to see uncommitted writes)
                let (bmx, nodes) = match btree_root.all_extents_tx(&mut tx) {
                    Ok(tuple) => tuple,
                    Err(e) => {
                        tx.abort();
                        reply.error(Errno::from_i32(e));
                        return;
                    }
                };
                let data_blocks: u64 = bmx.extents().iter().map(|e| e.br_blockcount).sum();
                raw_inode.set_nblocks(data_blocks + nodes.len() as u64);
                raw_inode.set_nextents(bmx.extents().len() as u64);
                raw_inode.set_mtime(std::time::SystemTime::now());
                raw_inode.set_ctime(std::time::SystemTime::now());
                raw_inode.finalise();

                if let Err(e) = tx.write_bytes(inode_offset, raw_inode.as_bytes()) {
                    tx.abort();
                    reply.error(e.to_errno());
                    return;
                }

                if let Err(e) = tx.commit() {
                    reply.error(e.to_errno());
                    return;
                }
                s.device.invalidate();
                s.device.set_bufsize(inode_size);
                let dinode = Dinode::from(s.device.by_ref(), &sb, xfs_ino);
                if let Some(oi) = s.open_files.get_mut(&u64::from(ino)) {
                    oi.dinode = dinode;
                }
                reply.ok();
            } else if dinode.di_core.di_format == XfsDinodeFmt::Extents {
                // Extents format: punch hole by modifying extent list in inode
                let mut raw_inode = match RawDinode::from_bytes(peek) {
                    Ok(ri) => ri,
                    Err(e) => {
                        reply.error(e.to_errno());
                        return;
                    }
                };

                // Get current extents
                let extents = match raw_inode.core_extents() {
                    Some(e) => e,
                    None => {
                        reply.error(Errno::from_i32(crate::libxfuse::ENOATTR));
                        return;
                    }
                };

                // Split extents at startoff and endoff
                let (kept, freed) = RawDinode::split_extents_in_range(&extents, startoff, endoff);
                if freed.is_empty() {
                    reply.ok();
                    return;
                }

                // Start transaction
                let mut tx = s.begin();

                // Free the blocks in the allocator
                let mask = u64::from(1u32 << sb.sb_agblklog) - 1;
                for (fsb, len) in &freed {
                    let ag = (*fsb >> sb.sb_agblklog) as u32;
                    let start = (*fsb & mask) as u32;
                    if let Err(e) = crate::libxfuse::alloc::allocator::free_in_group(
                        &mut tx,
                        &sb,
                        ag,
                        crate::libxfuse::alloc::free_space::FreeRun { start, len: *len },
                    ) {
                        tx.abort();
                        reply.error(e.to_errno());
                        return;
                    }
                }

                // Write kept extents back to inode
                if let Err(e) = raw_inode.set_core_extents(&kept) {
                    tx.abort();
                    reply.error(e.to_errno());
                    return;
                }

                // Update inode metadata
                raw_inode.set_mtime(std::time::SystemTime::now());
                raw_inode.set_ctime(std::time::SystemTime::now());
                raw_inode.finalise();

                if let Err(e) = tx.write_bytes(inode_offset, raw_inode.as_bytes()) {
                    tx.abort();
                    reply.error(e.to_errno());
                    return;
                }

                if let Err(e) = tx.commit() {
                    reply.error(e.to_errno());
                    return;
                }
                s.device.invalidate();
                s.device.set_bufsize(inode_size);
                let dinode = Dinode::from(s.device.by_ref(), &sb, xfs_ino);
                if let Some(oi) = s.open_files.get_mut(&u64::from(ino)) {
                    oi.dinode = dinode;
                }
                reply.ok();
            } else {
                reply.error(Errno::ENOTSUP);
            }
            return;
        }

        // KEEP_SIZE (with or without ZERO_RANGE): allocate blocks for [offset, end)
        // without changing file size
        // Read the inode first (before transaction)
        let mut peek = vec![0u8; inode_size];
        if let Err(e) = s.device.device().read_at(&mut peek, inode_offset) {
            reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
            return;
        }

        if dinode.di_core.di_format == XfsDinodeFmt::Btree {
            let mut raw_inode = match RawDinode::from_bytes(peek) {
                Ok(ri) => ri,
                Err(e) => {
                    reply.error(e.to_errno());
                    return;
                }
            };

            let btree_root_result = raw_inode.data_btree_root();
            let mut btree_root = match btree_root_result {
                Ok(mut root) => {
                    root.set_owner(xfs_ino);
                    root
                }
                Err(e) => {
                    reply.error(e.to_errno());
                    return;
                }
            };

            let device_arc = s.device.device();
            let mut buf_reader = BlockReader::from_device(device_arc);

            // Now start transaction and do the allocation
            let mut tx = s.begin();

            // Allocate a chunk for each block in the range (simplified)
            for fblock in startoff..endoff {
                // Check if this block already exists
                match btree_root.map_block(&mut buf_reader, fblock, false) {
                    Ok((Some(_), _)) => continue, // Block already allocated
                    Ok((None, _)) => {
                        // Need to allocate - use the allocator
                        // Allocate in the same AG as the file's inode
                        let agno = sb.locate_ino(xfs_ino).agno;
                        let run = match crate::libxfuse::alloc::allocator::allocate(
                            &mut tx, &sb, agno, 1,
                        ) {
                            Ok(r) => r,
                            Err(e) => {
                                tx.abort();
                                reply.error(e.to_errno());
                                return;
                            }
                        };
                        // Convert group-relative block to filesystem block: agno * sb_agblocks + agblock
                        let ag_base = u64::from(agno) * u64::from(sb.sb_agblocks);
                        let fsblock = ag_base + run.start as u64;

                        // Create extent record
                        let extent = crate::libxfuse::bmbt_rec::BmbtRec {
                            br_startoff:   fblock,
                            br_startblock: fsblock,
                            br_blockcount: 1,
                            br_flag:       true, // unwritten (preallocated)
                        };

                        // Insert into btree
                        if let Err(e) =
                            btree_root.insert_extent(&mut buf_reader, extent, &sb, &mut tx)
                        {
                            tx.abort();
                            reply.error(Errno::from_i32(e));
                            return;
                        }
                    }
                    Err(e) => {
                        tx.abort();
                        reply.error(Errno::from_i32(e));
                        return;
                    }
                }
            }

            // Update inode
            // Recalculate blocks from the btree extent map (through tx to see uncommitted writes)
            let (bmx, nodes) = match btree_root.all_extents_tx(&mut tx) {
                Ok(tuple) => tuple,
                Err(e) => {
                    tx.abort();
                    reply.error(Errno::from_i32(e));
                    return;
                }
            };
            let data_blocks: u64 = bmx.extents().iter().map(|e| e.br_blockcount).sum();
            raw_inode.set_nblocks(data_blocks + nodes.len() as u64);
            raw_inode.set_nextents(bmx.extents().len() as u64);
            raw_inode.set_mtime(std::time::SystemTime::now());
            raw_inode.set_ctime(std::time::SystemTime::now());
            raw_inode.finalise();

            if let Err(e) = tx.write_bytes(inode_offset, raw_inode.as_bytes()) {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }

            if let Err(e) = tx.commit() {
                reply.error(e.to_errno());
                return;
            }
            s.device.invalidate();
            s.device.set_bufsize(inode_size);
            let dinode = Dinode::from(s.device.by_ref(), &sb, xfs_ino);
            if let Some(oi) = s.open_files.get_mut(&u64::from(ino)) {
                oi.dinode = dinode;
            }
            reply.ok();
        } else if dinode.di_core.di_format == XfsDinodeFmt::Extents {
            // Extents format: allocate blocks by adding to extent list in inode
            let mut raw_inode = match RawDinode::from_bytes(peek) {
                Ok(ri) => ri,
                Err(e) => {
                    reply.error(e.to_errno());
                    return;
                }
            };

            // Get current extents
            let extents = raw_inode.core_extents().unwrap_or_default();

            // Start transaction
            let mut tx = s.begin();

            // Allocate blocks and build new extent list
            let mut new_extents = Vec::new();
            let mut extents_idx = 0;

            for fblock in startoff..endoff {
                // Find the extent covering this block (if any)
                while extents_idx < extents.len() {
                    let e = extents[extents_idx];
                    let end = e.br_startoff + e.br_blockcount;
                    if end <= fblock {
                        // This extent is before the block - keep it
                        new_extents.push(e);
                        extents_idx += 1;
                    } else if e.br_startoff <= fblock && fblock < end {
                        // Block already allocated in this extent - keep extent
                        new_extents.push(e);
                        extents_idx += 1;
                        break;
                    } else {
                        // Next extent is after this block
                        break;
                    }
                }

                // Check if we already added an extent covering this block
                let already_allocated = new_extents
                    .iter()
                    .any(|e| e.br_startoff <= fblock && fblock < e.br_startoff + e.br_blockcount);

                if already_allocated {
                    continue;
                }

                // Need to allocate a block for this file block
                // Allocate in the same AG as the file's inode
                let agno = sb.locate_ino(xfs_ino).agno;
                let run = match crate::libxfuse::alloc::allocator::allocate(&mut tx, &sb, agno, 1) {
                    Ok(r) => r,
                    Err(e) => {
                        tx.abort();
                        reply.error(e.to_errno());
                        return;
                    }
                };
                let ag_base = u64::from(agno) * u64::from(sb.sb_agblocks);
                let fsblock = ag_base + run.start as u64;

                // Create extent record (try to coalesce with previous if adjacent)
                let extent = crate::libxfuse::bmbt_rec::BmbtRec {
                    br_startoff:   fblock,
                    br_startblock: fsblock,
                    br_blockcount: 1,
                    br_flag:       true, // unwritten (preallocated)
                };

                // Simple coalescing: if last extent is adjacent, extend it
                if let Some(last) = new_extents.last_mut() {
                    if last.br_startoff + last.br_blockcount == extent.br_startoff
                        && last.br_startblock + last.br_blockcount == extent.br_startblock
                    {
                        last.br_blockcount += 1;
                    } else {
                        new_extents.push(extent);
                    }
                } else {
                    new_extents.push(extent);
                }
            }

            // Add any remaining extents
            while extents_idx < extents.len() {
                new_extents.push(extents[extents_idx]);
                extents_idx += 1;
            }

            // Write new extent list back to inode
            if let Err(e) = raw_inode.set_core_extents(&new_extents) {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }

            // Update inode metadata
            raw_inode.set_mtime(std::time::SystemTime::now());
            raw_inode.set_ctime(std::time::SystemTime::now());
            raw_inode.finalise();

            if let Err(e) = tx.write_bytes(inode_offset, raw_inode.as_bytes()) {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }

            if let Err(e) = tx.commit() {
                reply.error(e.to_errno());
                return;
            }
            s.device.invalidate();
            s.device.set_bufsize(inode_size);
            let dinode = Dinode::from(s.device.by_ref(), &sb, xfs_ino);
            if let Some(oi) = s.open_files.get_mut(&u64::from(ino)) {
                oi.dinode = dinode;
            }
            reply.ok();
        } else {
            reply.error(Errno::ENOTSUP);
        }
    }

    fn bmap(&self, _req: &Request, ino: INodeNo, blocksize: u32, idx: u64, reply: ReplyBmap) {
        let s = &mut *self.state.lock().unwrap();
        if blocksize == 0 {
            reply.error(Errno::EINVAL);
            return;
        }

        let oi = match s.open_files.get(&u64::from(ino)) {
            Some(oi) => oi,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let dinode = &oi.dinode;
        let format = dinode.di_core.di_format;
        if !matches!(format, XfsDinodeFmt::Extents | XfsDinodeFmt::Btree) {
            reply.error(Errno::ENOTSUP);
            return;
        }

        if dinode.is_realtime() {
            reply.error(Errno::ENOTSUP);
            return;
        }

        let sb = s.sb;
        let xfs_ino = s.xfs_ino(u64::from(ino));
        let inode_offset = sb.inode_offset(xfs_ino);
        let inode_size = sb.inode_size();

        // Convert file block index (idx) to file block offset
        // The FUSE bmap blocksize may differ from the filesystem blocksize
        let fs_blocksize = u64::from(sb.sb_blocksize);
        let file_block = (idx * u64::from(blocksize)) / fs_blocksize;

        let phys_block: Result<Option<XfsFsblock>, i32> = {
            let mut result = Err(libc::ENOTSUP);
            match dinode.di_core.di_format {
                XfsDinodeFmt::Extents => {
                    let mut peek = vec![0u8; inode_size];
                    if let Err(_e) = s.device.device().read_at(&mut peek, inode_offset) {
                        result = Err(libc::EIO);
                    } else {
                        let mut dinode = Dinode::from_bytes(&peek, &sb, xfs_ino);
                        let file = dinode.get_file();
                        if let Ok(file) = file {
                            result = match file.lookup(s.device.by_ref(), &sb, file_block, false) {
                                Ok((Some(block), _)) => Ok(Some(block)),
                                Ok((None, _)) => Ok(None),
                                Err(errno) => Err(errno),
                            };
                        } else if let Err(e) = file {
                            result = Err(e);
                        }
                    }
                }
                XfsDinodeFmt::Btree => {
                    let mut peek = vec![0u8; inode_size];
                    if let Err(_e) = s.device.device().read_at(&mut peek, inode_offset) {
                        result = Err(libc::EIO);
                    } else {
                        match RawDinode::from_bytes(peek) {
                            Ok(raw_inode) => {
                                let root = match raw_inode.data_btree_root() {
                                    Ok(mut r) => {
                                        r.set_owner(xfs_ino);
                                        r
                                    }
                                    Err(e) => {
                                        result = Err(e.errno());
                                        BtreeRoot::new(
                                            BmdrBlock::default(),
                                            Vec::new(),
                                            Vec::new(),
                                            None,
                                        )
                                    }
                                };
                                if result.is_ok() {
                                    let mut buf_reader =
                                        BlockReader::from_device(s.device.device());
                                    let map_result =
                                        root.map_block(&mut buf_reader, file_block, false);
                                    result = map_result.map(|(block, _)| block);
                                }
                            }
                            Err(e) => {
                                result = Err(e.errno());
                            }
                        }
                    }
                }
                _ => {
                    result = Err(libc::ENOTSUP);
                }
            }
            result
        };

        let phys_block = match phys_block {
            Ok(block) => block,
            Err(e) => {
                reply.error(Errno::from_i32(e));
                return;
            }
        };

        let Some(phys_fsblock) = phys_block else {
            // Hole in the file
            reply.error(Errno::ENOENT);
            return;
        };

        // Convert filesystem block to device sector (512-byte sectors for BMAP)
        let sector_size = 512u64;
        let phys_offset = sb.fsb_to_offset(phys_fsblock);
        let sector = phys_offset / sector_size;

        reply.bmap(sector);
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let s = &mut *self.state.lock().unwrap();
        if !s.writable {
            reply.error(Errno::EROFS);
            return;
        }

        let mut nameparts = name.as_bytes().splitn(2, |c| *c == b'.');
        let _namespace = nameparts.next().unwrap();
        let name = OsStr::from_bytes(nameparts.next().unwrap());

        // Get the inode and do attribute operations before starting the transaction
        let (xfs_ino, attr_bytes) = {
            let xfs_ino = s.xfs_ino(u64::from(ino));
            let oi = &mut s.open_files.get_mut(&u64::from(ino)).unwrap();
            s.device.set_bufsize(s.sb.sb_blocksize as usize);

            // Get the attribute fork
            let attrs = match oi.dinode.get_attrs(s.device.by_ref(), &s.sb) {
                Some(attrs) => attrs,
                None => {
                    reply.error(Errno::from_i32(crate::libxfuse::ENOATTR));
                    return;
                }
            };

            // Set the attribute
            match attrs.set(s.device.by_ref(), &s.sb, name, value, flags as u32) {
                Ok(_) => {}
                Err(e) => {
                    reply.error(Errno::from_i32(e));
                    return;
                }
            }

            // Serialize the attribute fork
            let attr_bytes = match attrs.serialize(&s.sb) {
                Ok(b) => b,
                Err(e) => {
                    reply.error(Errno::from_i32(e));
                    return;
                }
            };

            (xfs_ino, attr_bytes)
        };

        // Extract superblock values needed during the transaction.
        let sb = s.sb;
        let inode_offset = sb.inode_offset(xfs_ino);
        let inode_size = sb.inode_size();

        // Start a transaction
        let mut tx = s.begin();

        // Read the raw inode bytes
        let raw_inode_bytes = match tx.read_bytes(inode_offset, inode_size) {
            Ok(b) => b,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        // Create a RawDinode from the bytes
        let mut raw_inode = match RawDinode::from_bytes(raw_inode_bytes) {
            Ok(ri) => ri,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        // Write the attribute bytes back to the inode
        if let Err(e) = raw_inode.set_attr_bytes(&attr_bytes) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // Write the inode back to disk
        if let Err(e) = tx.write_bytes(inode_offset, raw_inode.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        if let Err(e) = tx.commit() {
            reply.error(e.to_errno());
            return;
        }
        reply.ok();
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let s = &mut *self.state.lock().unwrap();
        if !s.writable {
            reply.error(Errno::EROFS);
            return;
        }

        let mut nameparts = name.as_bytes().splitn(2, |c| *c == b'.');
        let _namespace = nameparts.next().unwrap();
        let name = OsStr::from_bytes(nameparts.next().unwrap());

        // Get the inode and do attribute operations before starting the transaction
        let (xfs_ino, attr_bytes) = {
            let xfs_ino = s.xfs_ino(u64::from(ino));
            let oi = &mut s.open_files.get_mut(&u64::from(ino)).unwrap();
            s.device.set_bufsize(s.sb.sb_blocksize as usize);

            // Get the attribute fork
            let attrs = match oi.dinode.get_attrs(s.device.by_ref(), &s.sb) {
                Some(attrs) => attrs,
                None => {
                    reply.error(Errno::from_i32(crate::libxfuse::ENOATTR));
                    return;
                }
            };

            // Remove the attribute
            match attrs.remove(s.device.by_ref(), &s.sb, name) {
                Ok(_) => {}
                Err(e) => {
                    reply.error(Errno::from_i32(e));
                    return;
                }
            }

            // Serialize the attribute fork
            let attr_bytes = match attrs.serialize(&s.sb) {
                Ok(b) => b,
                Err(e) => {
                    reply.error(Errno::from_i32(e));
                    return;
                }
            };

            (xfs_ino, attr_bytes)
        };

        // Extract superblock values needed during the transaction.
        let sb = s.sb;
        let inode_offset = sb.inode_offset(xfs_ino);
        let inode_size = sb.inode_size();

        // Start a transaction
        let mut tx = s.begin();

        // Read the raw inode bytes
        let raw_inode_bytes = match tx.read_bytes(inode_offset, inode_size) {
            Ok(b) => b,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        // Create a RawDinode from the bytes
        let mut raw_inode = match RawDinode::from_bytes(raw_inode_bytes) {
            Ok(ri) => ri,
            Err(e) => {
                tx.abort();
                reply.error(e.to_errno());
                return;
            }
        };

        // Write the attribute bytes back to the inode
        if let Err(e) = raw_inode.set_attr_bytes(&attr_bytes) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        // Write the inode back to disk
        if let Err(e) = tx.write_bytes(inode_offset, raw_inode.as_bytes()) {
            tx.abort();
            reply.error(e.to_errno());
            return;
        }

        if let Err(e) = tx.commit() {
            reply.error(e.to_errno());
            return;
        }
        reply.ok();
    }
}

#[cfg(test)]
mod dirent_tests {
    use std::io::Write as _;

    use super::Volume;
    use crate::libxfuse::error::FsError;

    /// Inode numbers in `xfsv4.img`, measured with `xfs_db` from its root.
    const ROOT: u64 = 32;
    /// `sf` -- shortform, two files.
    const SF: u64 = 35;
    /// `block` -- 32 files in one directory block, so **not** shortform.
    const BLOCK: u64 = 65_568;

    fn open_golden(name: &str) -> Option<(tempfile::NamedTempFile, Volume)> {
        let golden = crate::libxfuse::alloc::golden(name)?;
        let mut copy = tempfile::NamedTempFile::new().ok()?;
        {
            let mut src = std::fs::File::open(&golden).ok()?;
            std::io::copy(&mut src, &mut copy).ok()?;
        }
        copy.flush().ok()?;
        let v = Volume::new(copy.path(), None, true).ok()?;
        Some((copy, v))
    }

    /// `add_dirent` refuses a directory whose data fork is not shortform.
    ///
    /// **Refuses, and writes nothing.**  Block, leaf and node directories are a
    /// different format; putting a shortform entry into one produces a directory
    /// `xfs_repair` rejects while it appears to work.  The refusal is `ENOSYS`,
    /// not `ENOSPC` -- the group may be nearly empty and no retry helps -- and not
    /// `EINVAL`, which would claim the caller asked for something malformed rather
    /// than something unimplemented.
    #[test]
    fn adding_an_entry_to_a_non_shortform_directory_is_refused() {
        let Some((_copy, v)) = open_golden("xfsv4.img") else {
            eprintln!("skipping: no unpacked xfsv4.img");
            return;
        };
        assert_ne!(ROOT, BLOCK, "these inodes should be distinct");
        match v.add_dirent(BLOCK, b"newfile", SF, 1) {
            Err(FsError::Unsupported { .. }) => {}
            Err(other) => panic!("wrong error for a non-shortform directory: {other}"),
            Ok(()) => panic!("adding an entry to a block directory was allowed"),
        }
    }

    /// `remove_dirent` refuses a name that is not there, with `ENOENT`.
    #[test]
    fn removing_an_entry_that_is_not_there_is_enoent() {
        let Some((_copy, v)) = open_golden("xfsv4.img") else {
            eprintln!("skipping: no unpacked xfsv4.img");
            return;
        };
        match v.remove_dirent(SF, b"definitely-not-here") {
            Err(FsError::Invalid { errno, .. }) if errno == libc::ENOENT => {}
            Err(other) => panic!("wrong error for a missing name: {other}"),
            Ok(()) => panic!("removing a name that is not there was allowed"),
        }
    }

    /// Neither primitive touches a directory whose format it does not handle.
    ///
    /// The refusal has to be **before** any write, not after one, because a
    /// directory mutated in two different formats is worse than one untouched --
    /// there is no undo at this level, and the transaction commits as a unit.
    #[test]
    fn a_refused_operation_leaves_the_image_byte_identical() {
        let Some((copy, v)) = open_golden("xfsv4.img") else {
            eprintln!("skipping: no unpacked xfsv4.img");
            return;
        };
        let before = std::fs::read(copy.path()).expect("reading the image");
        let _ = v.add_dirent(BLOCK, b"newfile", SF, 1);
        let _ = v.remove_dirent(SF, b"definitely-not-here");
        let after = std::fs::read(copy.path()).expect("reading the image");
        assert_eq!(before, after, "a refused operation modified the image");
    }
}
