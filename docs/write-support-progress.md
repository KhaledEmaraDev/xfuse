# Write support progress

This document is the running report for the effort to extend `xfuse` from a
read-only XFS implementation into a read/write one.  It is updated at the end of
every phase, and it records what is *actually* implemented and *actually*
tested — never what is planned.

Licensing and provenance for the work are tracked separately in
[`licensing.md`](licensing.md).

## How to read this document

* **Status** values are `done`, `in progress`, `not started`.
* A phase is only `done` when it has an implementation, unit tests, integration
  tests, documentation, error handling, and a license/provenance review.
* "Baseline" below is the state of the repository before any write support was
  added.

---

## Baseline

Recorded on the development machine described in [Environment](#environment).

| Check | Result |
|:------|:-------|
| `cargo build` | succeeds |
| `cargo test --bins` | 21 passed, 1 ignored |
| `cargo clippy --bins -- -D warnings` | clean |
| `cargo fmt -- --check` (nightly, as CI runs it) | clean |
| `cargo test` (whole suite) | **fails to compile on Linux**, see below |
| read-only mount of a golden image | works |

### Environment

The development container is Linux (Ubuntu 26.04 on WSL2), while the project's
CI and its historic home are FreeBSD.  Three consequences:

* `pkg-config` and `libfuse-dev` are absent, so `fuser`'s `libfuse` feature
  cannot find `fuse.pc`.  Builds in this container use a local stand-in for
  `pkg-config` plus a symlink to the installed `libfuse3.so.3`; nothing about
  the repository changes for this.  On FreeBSD, where `fusefs-libs` and
  `pkgconf` are installed, `cargo build` works directly.
* `tests/integration.rs` and `benches/read-amplification.rs` only compile on
  FreeBSD (`require_fusefs!` is defined for `target_os = "freebsd"` only, and
  they use the FreeBSD `sysctl::Statfs` API).  This is pre-existing and is left
  alone.  New write tests therefore live in their own test target,
  `tests/write.rs`, which is portable.
* `mdconfig(8)` needs root, so loop devices are not available.  The write tests
  work on plain image *files* instead, which is exactly how the golden images
  work, and they use the same helpers as the existing tests.

Native XFS tooling 6.18 (`mkfs.xfs`, `xfs_db`, `xfs_repair`, `xfs_metadump`,
`xfs_mdrestore`, `xfs_bmap`, `xfs_logprint`) is available and is used as a
black box to generate images and to check results.

### Current supported XFS features (read side)

| Area | Supported |
|:-----|:----------|
| Filesystem version | 4 and 5 |
| Block size | 512 B and larger |
| Inode size | 256 B (v1/v2 inodes) and 512 B (v3 inodes) |
| Inode formats | `dev`, `local`, `extents`, `btree` |
| Directory formats | shortform, block, leaf, node, btree, and the single-block "btree with one leaf" case |
| Extended attributes | shortform, extents, leaf, node, btree; both v1 and v2 (v5) attribute formats |
| Features | `ftype`, `attr2`, `crc`, `projid32`, `align`, `sparse inodes`, `large extent counts` (NREXT64), `parent pointer` (read) |
| Rejected at mount | `meta_uuid`, `needsrepair`, `metadir` (with an RT device), `zoned` (with an RT device), unknown `features_incompat` bits |
| Real-time devices | read only, with a separate RT device argument |

### Current FUSE operations

Implemented: `init`, `lookup`, `forget`, `getattr`, `readlink`, `open`,
`read`, `lseek`, `opendir`, `readdir`, `statfs`, `getxattr`, `listxattr`.

Not implemented: `write`, `setattr`, `create`, `mkdir`, `unlink`, `rmdir`,
`rename`, `flush`, `fsync`, `release`, `setxattr`, `removexattr`, `fallocate`,
`link`, `symlink`, `mknod`, `access`, `chown`-family.

`FUSE_NO_OPEN_SUPPORT` and `FUSE_NO_OPENDIR_SUPPORT` are negotiated, so the
kernel usually does not send `open`/`opendir` at all.

### Current limitations relevant to write support

1. The device is opened read-only (`File::options().read(true).write(false)`),
   and `BlockReader` is a forward-only, seek-based readahead buffer with no
   write path at all.
2. Metadata is parsed straight out of the device with no buffer cache, so
   nothing can be modified in place, and there is no transaction concept.
3. Inodes, directories, and extended attributes are decoded into in-memory
   structures with no serializer, so nothing can be written back.
4. `Dinode::from` and friends `panic!` on corrupt or unsupported input, which
   is unacceptable on a write path where corruption must become an error.
5. `SUPERBLOCK` is a process-wide `OnceLock`, so only one image can be mounted
   at a time, and every extent lookup goes through it.
6. The mount is unconditionally `ro`; there is no capability check that
   distinguishes "readable" from "writable".

### Test coverage at baseline

* 21 unit tests: B+tree pointer gap arithmetic, extent lookup, `BlockReader`
  seek behaviour, one ignored placeholder.
* `tests/integration.rs` (FreeBSD only): golden-image directory trees, file
  contents, sizes, xattrs, symlinks, `lseek`, `statfs`, block-device nodes.
* `benches/read-amplification.rs` (FreeBSD only).

Not tested at all at baseline: anything that writes.

### Licensing status at baseline

BSD-2-Clause project; 142 transitive dependencies, all permissively licensed;
no GPL code.  See [`licensing.md`](licensing.md).

---

## Phase status

| Phase | Subject | Status |
|:------|:--------|:-------|
| 1 | Writable block-device abstraction | done |
| 2 | Block cache with explicit dirty state | done |
| 3 | Transaction abstraction | done |
| 4 | Mutable inode state and serialization | done |
| 5 | Reusable extent map | done |
| 6 | Overwrite of already allocated extents, FUSE write path, capability gate | done |
| 7-18 | allocation, extension, BMBT mutation, truncation, inode allocation, directories, namespace, metadata, xattrs, journal, recovery, crash testing | not started |
Phase notes follow.

### Phase 1 — writable block-device abstraction

`src/libxfuse/block_device.rs` introduces `BlockDevice`: an owned handle to the
image with `read_at`, `write_at`, `flush`, `size`, and `sectorsize`.  It is
opened read-only or read-write explicitly, performs positional I/O only (no
shared file cursor), and never buffers.  `BlockReader` was refactored to sit on
top of it, so the read path's buffering and ownership semantics are unchanged and
all existing tests pass unchanged.  The reader gained `invalidate()`, which
drops the readahead window; the write path calls it after committing so that no
stale read-side copy of a physical block can survive a mutation.

Rationale: one open handle and one `pread`/`pwrite` path means the filesystem
can never hold two independent writable copies of the same physical block.

### Phase 2 — block cache

`src/libxfuse/block_cache.rs` holds fixed-size filesystem blocks keyed by
device offset, each with an explicit state: `Clean`, `Dirty`, `Logged`, or
`Committed`.  `Logged` and `Committed` are not yet produced by anything, but
they exist so that the journal implementation described in phase 16 does not
have to change this interface.  The cache can read a block, hand out a mutable
reference (marking it dirty), enumerate and write back dirty blocks, discard
everything, and refuse to grow past a configurable limit.

### Phase 3 — transactions

`src/libxfuse/transaction.rs` provides `Transaction`, created with
`Transaction::begin`.  It exposes `read_block`, `modify_block`, `write_data`,
`write_block`, `commit`, and `abort`.  All of the block-level I/O a filesystem
operation performs goes through it; nothing above it writes to the device
directly.  Two commit strategies exist behind one interface:

* `CommitMode::ReadOnly` refuses any mutation with `EROFS`.  This is what a
  normal mount uses.
* `CommitMode::Direct` writes dirty blocks straight to the device and is
  **experimental and not crash-safe**.  It is reachable only through
  `--experimental-rw`, and the mount prints a warning.

`abort` throws away every change, and so does dropping a transaction without
finishing it, because a caller that lost track of a transaction is exactly the
case in which silently keeping the changes would be wrong.

A transaction's read path goes through the block cache rather than straight to
the device, so a transaction that has already changed part of a block sees its
own change when it reads the rest of that block.

`src/libxfuse/error.rs` adds `FsError`, which maps the errors the write path
needs (`EIO`, `ENOSPC`, `EROFS`, `EINVAL`, `EEXIST`, `ENOENT`, `ENOTEMPTY`,
`EFBIG`, `EUCLEAN`, `ENOSYS`, `EXDEV`, `EISDIR`, `ENOTDIR`, `EPERM`, `EDQUOT`)
and is used instead of `panic!` on the new code paths.

### Phase 4 — mutable inodes

`src/libxfuse/inode.rs` adds `RawDinode`, which owns the inode's exact byte
image and exposes typed accessors.  In-memory state and the serialized image are
deliberately separate: a modification patches the byte image, and the read path
decodes that same image, so there is exactly one interpretation of the format.

`RawDinode` can write back `mtime`, `ctime`, `atime`, `size`, `mode`, `uid`,
`gid`, `nlink`, and the data-fork extent list.  It recomputes the inode
checksum for version 3 inodes (CRC-32C over the whole inode with the checksum
field zeroed), maintains the change counter, and clears the log sequence number
in direct-write mode.  Timestamps are stored in the representation the inode
itself selects: 32-bit seconds and nanoseconds normally, and the 64-bit
`(seconds << 32) + nanoseconds` form when the inode has the big-time flag set.

`Dinode::from` was refactored to read the inode's bytes and then decode them, so
the parser and the serializer cannot drift apart.

### Phase 5 — extent map

`src/libxfuse/extent.rs` adds `ExtentMap`, built from either an in-core extent
list or a B+tree, and answering `lookup(file_block)`.  Both the read path and the
new write path use it, so there is still only one implementation of "where does
logical block *n* live".  The extent record type used by the write path
(`Extent`) can be encoded back to its on-disk form, which is the groundwork for
phase 9.

### Phase 6 — overwrite, FUSE write path, capability gate

`FsCapabilities` in `src/libxfuse/capabilities.rs` inspects the superblock and
reports, separately, what is supported for reading and what is supported for
writing, together with the list of features that block a read-write mount.  A
read-only mount ignores the write restrictions; a read-write mount is refused
with a message naming the offending feature.

Currently writable, when the filesystem has no blocking feature: overwriting
bytes that are already inside an allocated, written extent of an existing
regular file, at any offset below the end of the file, including partial
blocks.  Writing into a hole, past the end of the file, or to a directory,
symlink, or device node is refused rather than approximated, because those need
phases 8 to 13.

New FUSE operations: `open` (returning a real file handle), `write`, `flush`,
`fsync`, and `release`.  `Volume` now keeps an `OpenFile` table, so writes are
rejected on a descriptor that was not opened, and `fh` is honoured.

`main.rs` gained `--experimental-rw`.  Without it the mount is read-only
exactly as before.

### Integration tests

`tests/write.rs` is a new test target, and covers:

* overwrite of an existing file's data, in a copy of a golden image, verified by
  unmounting, remounting, and re-reading;
* writes of a whole block at several alignments, each verified **through a
  second, read-only mount** rather than through the mount that wrote: the
  kernel's page cache would otherwise hand back the bytes that were written
  rather than the bytes that were stored, and a file system that put a write in
  the wrong place in the image would pass a test that reads its own writes
  back.  This is not hypothetical: an early version of this test passed while
  the write path was dropping the offset within a block.
* partial-block writes, and the bytes around them;
* writes to several files of different shapes: 512-byte blocks, a megabyte in
  many extents, a file in a different directory format;
* the modification time moving on the image, checked through a read-only mount;
* rejection of a read-write mount of an image with a blocking feature, with the
  refusal naming the feature;
* the last byte of a file being writable, and a write past the end of the file
  being refused with `EFBIG` and leaving the file byte-for-byte as it was;
* a write to a directory being refused;
* `xfs_repair -n` reporting no unexpected problems after the writes, and the
  data still reading back correctly.

### What the tests do not cover, and why

Writing into a hole, or into preallocated-but-unwritten space, is not tested end
to end.  The image that would test it is `xfs_preallocated.img`, whose
`files/preallocated` is a single 8 MiB unwritten extent, but that is a version 5
image with reflink, rmapbt and big-time, and the capability gate refuses all
three for writing.  The behaviour itself is covered where it can be: `ExtentMap`
reports an unwritten extent as a hole and a hole as no block at all, and a
volume write of a block with no physical block behind it is `ENXIO`.  The
end-to-end test belongs in this file once an image with a writable feature set
has a hole in it.

### How the results were checked by something other than xfuse

`xfs_repair -n` is the strongest witness available without root and a loop
device, and it reports the image clean after the writes.  Two further checks
were run by hand on a written image and are worth repeating as tests when
there is a place to put them:

* `xfs_db` reads the inode we wrote and agrees with it: the modification and
  change times it prints are the ones we stored, down to the nanosecond, and the
  size, block count and extent count are unchanged.
* Reading the image at the file's extent offset, computed by hand, returns the
  bytes that were written where they were written.

Neither of these is a substitute for mounting the image with the kernel's own
XFS driver, which is the check that this file's "definition of done" still owes.

Two facts that the tests turned up, and that the golden images do not make
obvious:

* `files/hello.txt` and `files/hello2.txt` are two names for **one** inode
  (a hard link), so a test that writes to both is testing the same file twice.
* The golden image's `hello.txt` has a modification time in 1982, which is what
  a fresh image from a build script ends up with.  A test that checks "the time
  moved" has to know that, or it will pass on an unchanged file.

### What a read-write mount does about the kernel's caches

A read-only mount tells the kernel to cache attributes and directory entries
forever, because nothing in it ever changes.  A read-write mount tells the
kernel to cache them not at all.  A write changes the modification time of the
file it wrote, and a cached attribute would go on reporting the old one; the
proper fix is to tell the kernel to throw the inode away, which needs a notifier
that this version of the FUSE library does not hand to a file system, so the
cache is switched off instead.  That costs one round trip per attribute, which
is the right trade while the write path is experimental.

---

## Definition of done for initial read-write support

| Item | Status |
|:-----|:-------|
| existing XFS images still mount read-only | done |
| existing read tests still pass | done |
| writable mount can be explicitly requested | done |
| unsupported XFS features cause a read-write mount rejection | done |
| existing allocated file data can be overwritten | done |
| files can be extended | not started |
| files can be truncated | not started |
| sparse files work | read only |
| files can be created | not started |
| directories can be created | not started |
| files can be unlinked | not started |
| directories can be removed | not started |
| files and directories can be renamed | not started |
| metadata updates work | partial (timestamps only) |
| transactions are used for metadata updates | done |
| journal and recovery work | not started |
| crash tests pass | not started |
| `xfs_repair` reports no unexpected corruption | done for the implemented subset |
| native XFS can read files modified by `xfuse` | pending (needs root and a loop device) |
| `xfuse` can read files created by native XFS | read side only |
| licensing audit confirms no GPL-derived source | done |

## Licensing review of the phases so far

No GPL source was consulted, copied, translated, or adapted while doing any of
this.  The format details came from the published XFS on-disk format, and every
uncertainty was settled by experiment against images produced by `mkfs.xfs` and
read back with `xfs_db` -- notably:

* where the extent count lives for an inode that does not use 64-bit counts
  (a 32-bit field beside the attribute fork's count, not the 16-bit one above
  it), settled by changing the bytes and watching which field moved;
* that a big-time timestamp is a nanosecond count from 1901-12-13 20:45:52 UTC,
  settled by decoding a real inode two ways and checking which matched what
  `xfs_db` printed;
* that the version 3 inode checksum and the superblock checksum are stored
  least significant byte first, and that the superblock's covers one 512-byte
  sector with the checksum field itself zeroed.

The two inodes embedded in `src/libxfuse/inode.rs`'s tests as byte strings came
from the golden images by reading them with a script; they are test data, and
they are there so that a change to the field layout shows up as a failing test
rather than as a wrong write.

See [`licensing.md`](licensing.md) for the dependency audit.
