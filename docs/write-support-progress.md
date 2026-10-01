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

### Phase 7 — allocation groups, part one: the group header and free list

The first piece of the allocation machinery is the read side of what a group
knows about itself, because nothing can be allocated until that is understood
and every offset in it has to be right before anything writes to it.

`src/libxfuse/alloc/` holds it:

* [`agf::Agf`] — the group file.  Where a group's free space is indexed, how
  many blocks it believes are free, how long its longest free run is, and which
  slice of the free list is live.
* [`agfl::Agfl`] — the group free list: a flat array of block numbers known to
  be free, with the operations an allocator needs (take a block from the front,
  give one back at the back) and the invariants that go with them.

`Sb` gained the rules for *where* a group's headers are.  They are at fixed
sectors within the group — the superblock at sector 0, the group file at 1, the
group inode header at 2, the free list at 3 — and a sector is the file system's
basic block, which is not always a whole file system block.  A file system with
4 KiB blocks on 512-byte sectors keeps four headers in its first block.  Getting
this wrong means reading a group header out of the middle of a file's data, and
the layout is not something to infer from the block size: it was checked against
every golden image in the repository.

The two structures keep their own bytes and are changed in place, like an inode,
so that the fields this code has no opinion about — the reverse-mapping and
reference-count roots, the reserved space — survive a write by construction.
A group header is also where the free list's live window lives, and it is kept
in the same transaction as any change to the list, because a free list entry
without its window, or a window without its entry, is a group that will hand
out the same block twice.

Fifteen unit tests decode the real group header and free list of two golden
images and compare every field with what `xfs_db` prints for the same
structures, so the expectations come from the reference implementation rather
than from this code.  The version 5 checksum is covered by the same convention
the inode and the superblock use: CRC-32C over the whole block with the
checksum field read as zeroes, stored least significant byte first.

* [`free_space::FreeSpaceNode`] — one node of a btree of free space, and the
  runs it holds.

The free space itself is not a bitmap but a list of *runs*, kept in a B+tree, and
there are two of them per group: one keyed by where each run starts, one keyed by
how long each run is.  They answer the two questions an allocation actually
asks.  A node's header, its records, and its checksum are read here.

That header is a different size on different file systems, which is the kind of
detail that is easy to get wrong: a version 4 node's records start sixteen bytes
into the block, a version 5 node's fifty-six bytes in, because the checksum, the
owner and the file system's identifier are in between.  Both were checked against
the free space `xfs_db` reports for the same group, and the tests in that module
use those runs as their expectations, so a failure means this code disagrees
with the reference implementation rather than with itself.

Two things about the tree's shape were settled by the format documentation and
then confirmed against the images.  An interior node is a key array followed by
a pointer array, and the pointer array begins after the key region sized for the
block's *maximum* record count -- which lands at 344 in a 512-byte version 4
block and 696 in a 1024-byte version 5 one, exactly where the arithmetic says.
`free_space::walk` follows those pointers to every leaf, checking that each
child is one level shallower, and the result was checked against each group's
own record of its free space, which is a stronger witness than any tool:

```text
xfsv4.img    AG1   29 children  1713 runs  10729 blocks  longest 8954
                       the group header says freeblks=10729, longest=8954
xfs1024.img  AG2    7 children   785 runs  86722 blocks  longest 85879
                       the group header says freeblks=86722, longest=85879
```

`first_run_from` and `first_run_of_at_least` are the two questions an allocation
asks, asked of the two different trees, and each comes back in its own tree's
order.  A node's key is its first record *in that order*, which is why a search
is a scan and not a descent: the size tree's last child in the test group holds
a run of 8954 blocks while its key says 1, the shortest run beneath it.  Taking
it would be right only if nothing to its left were long enough, which is what
the scan establishes; a descent would need the free space bins.

What is *not* here yet is the part that makes an allocation happen: taking a run
out of a tree *through* it, and freeing blocks.  The leaf-level half of the
first is now done -- `take_from_run` and `put_run` add, remove and re-order a
leaf's records, keep the node's count right, refuse an overlapping run, and
refuse a full node rather than dropping the run on the floor.  A model test runs
two hundred random takes and puts against a plain list of what should be left
and compares the whole set after every one, which is where a mutation that is
right for one record and wrong for two hundred shows up; it found three mistakes
in this code before it was finished.

Three things about real trees came out of experiments on the test images, and
all three are now written down in the code rather than in someone's head:

* A leaf in a 512-byte block holds at most 62 records, and `xfs_repair` refuses
  one holding fewer than 31: `bad btree nrecs (30, min=31, max=62) in btbno
  block 1/4`.  The XFS B+tree documentation describes merging as what a tree
  "should" do and derives `minrecs` as `maxrecs / 2`, which reads like a
  management policy rather than a validity rule -- and the first version of this
  work took that reading and dropped merging from the take path.  That reading
  is wrong for our purposes, and the experiment that settled it is in the next
  section: `xfs_repair` enforces the minimum, so an underfull leaf fails the
  check that decides whether an image is acceptable, and merging is required
  after all.  The documentation's "should" is about how XFS chooses to arrange
  a tree; `xfs_repair` is about what it will accept.
* A leaf holds records and *nothing else* -- 62 of them fill the region exactly
  -- so a tree's keys live only in its interior nodes.  The first version of the
  leaf writer kept a second array of keys there, on the assumption that a leaf
  and an interior node differ only in what the records mean, and wrote past the
  end of the block.
* A node's record count lives in two places, the bytes and the field the struct
  was built with, and they have to move together.  That is invisible in a test
  that reads a node back through the same struct that wrote it, and only shows
  up against the image.

An allocation is now bound to a transaction and to the group header.  Every
allocation changes three things -- a leaf in each of the two trees, and the
header's count of what is left -- and all three go into the caller's one
transaction, because a header that says a block is free when its tree says it is
taken is a block that gets handed out twice.  Nothing writes to the image: the
caller commits, or the image is as it was.

The header is read and written as bytes at a computed offset rather than as a
block, and that is not a detail to tidy up later.  A group header sits in the
second *sector* of its group, and with 1 KiB blocks and 512-byte sectors that is
half way through a block -- and in the first group that half of the block is
the superblock.  Reading the header as a block and writing it back as a block
takes the superblock with it; the first version of this did exactly that.

The other half of making a file bigger is an extent, and `RawDinode::add_extent`
adds one: it refuses a run that overlaps a block the file already has, keeps the
list in order, and *joins* a run that lands against an extent to it rather than
storing it beside.  Two extents that touch are one extent, and a file written a
block at a time would otherwise fill the inode with one-block extents and need a
B+tree far sooner than it should.  The same model test as the free space
trees' covers it: three hundred runs in a deterministic order, with the fork
compared against a map of which block belongs where after every one.

Still missing for file extension, and it is the rest of the operation rather
than a detail of it: the volume has to ask the allocator for the blocks, write
the data *and* the new extent list *and* the new size in one transaction, and
line the allocation up on a file system block boundary so that a write starting
part way into a block does not leave the earlier part of that block unwritten.  A
file whose data fork is a B+tree cannot be extended yet either, because that is
the extent mutation of the next phase; such a file is refused with a message
that says so rather than half-written.

### The merge bug, and why merging is no longer on the critical path

The allocator used to merge a short leaf into a sibling, and the merge was wrong
in three separate ways, each of which had to be found on its own:

* `remove_child` shifted the child's array the wrong way.  It wrote slot *i+1*
  from slot *i+2*, which is the shift for removing index-1, so the child that was
  supposed to be removed stayed in its slot and the *last* child was dropped
  instead.  The tree still answered searches, with the leaf that should have
  gone, which is why nothing about it looked like corruption.
* The merge picked the wrong side.  A child merged into the sibling on its left
  comes *after* that sibling's records, and a child with nothing to its left
  comes *before* the one on its right.  Both were inverted, which leaves a tree
  holding exactly the right records in the wrong order.
* The merge then dropped the child's records on the floor.  It read the sibling,
  wrote it back unchanged, and unlinked the child, so 384 blocks' worth of free
  space simply stopped existing.  This is what the `xfs_repair` count mismatch
  was reporting all along, and reading it as an occupancy rule sent the work
  looking for a rule that was never there.

With the third one fixed the loss is a conservation failure, and a conservation
failure is what the model test is for: it compares what is left in the two trees
after every single allocation, so a mutation that is right for one record and
wrong for two hundred cannot pass.

What replaced the merge, for the moment, is smaller: a child is dropped from its
parent only when it has no records left at all, and an underfull child is joined to
its neighbour instead.  Dropping a child closes the gap it leaves in its level's sibling chain,
which is the part that has to be exactly right -- see below.

That is enough to be correct about the *loss* and not yet enough to be correct
about the *image*, and the difference is worth keeping straight.  Removing the
merge branch made the conservation failure go away and made the model test pass,
and it also made 13 of the 14 write integration tests pass through a real FUSE
mount.  The fourteenth failed, and the reason is the rule above:

```
bad btree nrecs (30, min=31, max=62) in btbno block 1/4
```

Growing a file takes a run, the leaf it came from drops below half, and the leaf
is left that way.  A pristine `resources/xfsv4.img` passes `xfs_repair -n`
cleanly, so this is the mutation and not the image it started from.  Merging is
therefore back on the list, and the two things the merge has to get right are now
both known: the records have to end up in the right order relative to the
sibling they join, and the parent's separator key has to be recomputed from the
surviving child's actual first record -- whichever sibling that turns out to be.
The sibling chain has to be closed on both sides, which is the part that is
already written and tested.

Freeing a block -- inserting a run and joining it to its neighbours -- is still
missing, and belongs with the operations that free blocks rather than being
written before anything calls it.  So do the group header's count and longest
run in the same transaction, and the allocator's own interface.

### Putting blocks back

Freeing is the shape of allocating backwards, and it has the same three
obligations: both trees record the run as free, the group's own two numbers
follow, and the superblock's total follows too.  All of it goes into the
caller's one transaction.  The group header also needed setters it did not have
-- for the two btree roots and their heights -- because a tree that grows a
level gets a new root block, and a header still naming the old one points at a
node that is now an interior node in the middle of the tree.  The roots are
read back off the blocks rather than assumed, so the header records what the
tree is rather than what it was.

Two runs that touch are one run, so freeing joins a run to the record that ends
where it starts and to the record that begins where it ends, and when there are
both those become one.  A joined record can be *longer* than the one it
replaced, which moves it in the tree keyed by length, so it goes back in where
that tree's order now puts it rather than where the record it replaced sat.
Freeing a run that is already free is refused rather than adding a second copy,
which would be a block handed out twice.

**The joining is done within a leaf, and that is a limit rather than a
oversight.**  The two trees are keyed differently, so the record a run touches
in the tree keyed by start is in a different place from the record it touches in
the tree keyed by length, and finding the latter needs a search this does not
have.  The two available outcomes are two touching records or a record joined to
the wrong neighbour, and the first is correct -- the group's free space is the
same either way -- while the second is not.  So the first is what happens, and
the cross-leaf search is the next thing to build.

### Inserting, and the split that nearly lost a run

Both directions share one rule, and it is the opposite of the obvious one: the
run goes into the leaf's list *first*, and the overflow is what gets split.
Splitting a full leaf and then inserting puts the new run nowhere -- the two
halves describe what the leaf held before, so it belongs to neither, and the
tree loses a run while still looking like a tree.  That is why the sorted list
is a separate step from writing it, so a caller that has to split can split a
list that already contains the new record.

Three more things about a split, all found by the tests that check a split
leaves the tree able to answer a search:

* An interior node has **one more key than children**; the last key is the upper
  bound of the last child's keyspace.  A split gives the separator to *both*
  halves -- the left as its upper bound, the right as the first child's lower
  bound -- and a node can only hold as many children as it has room for keys
  *and* that one extra.
* The two halves start out carrying the links the whole node had, which is wrong
  three ways at once, so the new half has to be put into the chain: the old
  block's right neighbour moves along, and whatever was on the right has to be
  told that its left neighbour is no longer the block it was.
* A tree that grows a level builds its new root out of the *old* root's bytes.
  A node's checksum is taken over its own bytes, including the file system's
  identifier and the group's number, and a block that has never held a node has
  neither, so borrowing the old root's is both correct and the only way to get a
  checksum that verifies.

### Checking freeing against the file system, not against ourselves

Every test of the free path until now compared a hand-built image with this
code's own idea of what a consistent tree looks like, which is no evidence at
all.  Two tests now take a copy of a real image, move blocks through the real
transaction, and ask `xfs_repair -n`.  Both found things.

**Freeing blocks that are already free** left the tree holding them twice, and
repair said so plainly:

```
out-of-order bno btree record 2 (571 2) block 0/4
block (0,571-572) multiply claimed by bno space tree
```

A run that lands *inside* a record the tree already has is not a run to add
beside it: that record has to be cut in two around the part being freed.  Adding
a second copy is how a tree ends up handing the same block out twice, which is
what "multiply claimed" means.  This is what happens when a block is freed
twice, so the case is worth handling for its own sake.

**And then the superblock was told the wrong thing.**  The group's count was
right -- it is recomputed from the trees -- but the superblock's total was moved
by however many blocks were *asked* to be freed rather than by however many the
group's free space actually grew by.  For blocks that were genuinely new those
are the same, which is why the first test passed and this one did not.  Cutting
a record in two around blocks that were already free adds nothing at all, and
the superblock said `sb_fdblocks 90626, counted 90624`.  The count now follows
the difference the trees actually show, which is the thing repair compares and
also the thing that is true.

The round trip these two tests sit on -- take two blocks from a real image, hand
exactly those blocks back -- passes repair with nothing to say.

### A field that has to follow a sum, not a count

Growing a file at its end left the inode's own block count stale, and
`xfs_repair` said so:

```
bad nblocks 128 for inode 37, would reset to 152
```

The guard that was supposed to prevent it was checking the *number* of extents
rather than the sum of their lengths, which is what the field means.  A file
grown at its end lands beside its own last block, so the new blocks are *joined*
to an extent that was already there: the extent count does not move, twenty-four
more blocks are covered, and the count is silently wrong.  That is the ordinary
case for growing a file, and the guard was exactly inverted for it -- it fired
when an extent was added alongside, and stayed quiet when an existing one grew.

It went unnoticed because every other image in the suite has the file being
grown sitting inside a preallocated extent, so the growth wrote into blocks that
were already counted.  The freshly made image is the only one where growing a
file has to *allocate*, which is what happens on a real file system.  So the
class of defect is: an inode field derived from a sum, guarded by a test on a
count.

### What mixing taking and giving back found

The model test that caught the free path losing records a run at a time was
applied to both directions -- take blocks and give them back, in a fixed
sequence, checking after *every* operation that both trees agree with a model of
which blocks are free.  Comparing needs the two sides canonicalised, because a
tree is allowed to hold two records that touch, and the only thing two answers
can be compared on is which blocks are free.

It found a real defect:

```text
round 2: allocated (1615, 5)   -- out of the record (1615, 6), leaving (1620, 1)
round 5: freed (1615, 5)
later:   allocate(1): no free run in this node starts at block 1620
```

Chasing it turned up two more, both now fixed:

* **A parent could be left with a separator naming a record that is no longer
  there.**  The take path had a fast path that skipped refreshing a parent's key
  when a child merely *lost* a record, and a child that loses its **first**
  record is the ordinary case -- taking the head of a run.  This does not look
  like damage: the tree still answers, and answers wrongly.
* **The descent in `take_in_tree` was written in terms of a start block**, which
  is enough for the tree keyed by start and not enough for the tree keyed by
  length.  There, `child_for` *searches* for any run long enough, so it could
  arrive at a different leaf from the one holding the run the caller had already
  chosen; the take then reported nothing found where something was.  The
  descent now looks for the leaf that holds the run.

With both fixed a different failure took its place: at round 60 the tree keyed by
start held `(1615, 5)` and `(1621, 5)` where the tree keyed by length held only
`(1615, 5)`.  Five blocks free in one tree and allocated in the other -- `multiply
claimed`.  And the cause was the one the images had already answered: coalescing
within a leaf is not enough.

### Keeping the free list stocked: tried, and the accounting confirmed

The refill cannot work by reaching into the trees, because mid-operation the
group header still names the roots the trees had *before*.  The way out is the
one the documentation describes: keep the list stocked, so a split never has to
reach at all.  That was implemented -- free into the trees, then move some of it
onto the list -- and it works as far as the split itself, with the accounting
coming out exactly as `agf_freeblks` is documented:

```text
group 0: freeblks 30144 -> 30144 (freed 80)
```

Eighty blocks freed, eighty moved onto the list, and the count of free extents in
the trees unchanged -- which is the whole claim that the list is neither a free
extent nor a live node.

It was then reverted, for two reasons worth keeping.  It broke three existing
free tests, which is a regression against a working baseline and not something to
leave half-done.  And `xfs_repair` rejected the list it produced:

```text
bad agbno 4294967295 in agfl, agno 0     (once per slot)
```

`4294967295` is the null block, so the window named entries that were all null.
A list block that has never been written has *no* live entries whatever the
header's window says -- the header names a window over a block full of nothing --
and taking that window at its word is exactly this.  So the window handling for a
first-written list is the bug, and it is in `Agfl::window` and `give_back` rather
than in the stocking that calls them.

### Deciding a free once and obeying it twice

Freeing now works like this.  The tree keyed by start **decides** what the free
means: it finds the records the run touches -- the one that ends where it starts
and the one that begins where it ends -- and it can look in the leaves *beside*
it, because there the leaves are in the same order as the blocks they hold, so a
run's neighbours along the group's blocks are its neighbours in the tree.  It
reports what it joined.

The tree keyed by length is then **brought to that answer**: the records the first
tree joined stop being records there, and the run that is left goes in.  It never
forms an opinion of its own, because it cannot -- its neighbours along the
blocks are nowhere near its neighbours in its own order.

Letting each decide for itself is what produced the divergence, and the difference
is invisible unless you look for it: a model that joins touching runs before
comparing cannot see grouping differences at all, which is why this took so long
to believe.  Real images are what settled it -- identical record sets in every
group of `resources/xfsv4.img`, including one of nearly eight thousand records.

Three things came out of writing it:

* **The upward walk was in four places.**  Every path that changes a leaf has to
  do the same things afterwards -- drop a child that emptied and close the gap in
  the sibling chain, join a child that fell below half, read every parent's
  separators back off its children -- and having four copies is how they come to
  differ.  It is one function now.
* **Sharing records out belongs there too.**  Two full leaves cannot be merged,
  and that is the ordinary case rather than the corner one, so the shared walk
  moves records across the boundary when the sibling cannot absorb them.
* **A record that is *named* has to be found by name.**  `remove_run_in_tree`
  locates it and rebuilds the path by pointer, because descending by order answers
  "which leaf should hold this run" -- the right question for a take, and the
  wrong one for a removal.

The model test now passes, so the trees agree on the records and not merely on
the blocks.

A third fix was written for the first failure and taken back, because it was the
wrong shape: it made `take_from_run` find the record that *covers* the block
rather than the one starting at it, and that breaks a test which is asserting
intent -- **a take is from the head of a run**, since taking out of the middle
leaves a fragment nobody asked for.

From that it was tempting to conclude that the two trees are *meant* to group
free space differently, and that the second therefore needs a remove-this-range
operation rather than a take.  **That is wrong, and the images say so.**  In every
group of `resources/xfsv4.img` the two trees hold *identical record sets*:

```text
ag0  bno 19     cnt 19       identical
ag1  bno 1713   cnt 1713     identical
ag2  bno 9      cnt 9        identical
ag3  bno 7947   cnt 7947     identical
```

Not merely the same free blocks -- the same runs, grouped the same way, in a group
of nearly eight thousand records.  A filesystem that let the trees group free
space differently would be free to show it there, and one with that many records
almost certainly has runs freed next to each other.  The question was settled by
reading the images, not the documentation, and it had been sitting there
unexamined.

So coalescing is not tidying, and coalescing within a leaf only is the defect.
Freeing has to join a run to its neighbour **across the leaf boundary** so that
both trees arrive at the same records.  That is the next piece of work.

**This is why the cross-leaf search is the next thing to build and not a polish
item.**  The test is kept and ignored rather than deleted, with the operations
that reproduce it, because it reproduces exactly.

The same test also found a bug in its own model, which is worth recording: the
helper that put a freed run into the model dropped the records *touching* it as
though they were being replaced, losing exactly the blocks the join was supposed
to recover.  The tree was right and the model was wrong.  A false alarm is the
cheapest kind of model-test failure, because it is found by reading the
difference.

### The free list has no header

The list that the refill kept failing on turned out not to be the problem.  In
**every file system in this repository the free list is a bare array of block
numbers with no header at all**: the tool types the block as an array of one
hundred and twenty-eight slots, and reading group 0 of the hand-built image gives

```text
bno[0-127] = 0:null 1:7 2:8 3:9 4:10 5:null ...
```

Slots one to four hold blocks seven to ten, which is exactly the window the group
header names -- so the list was never empty, it had four usable blocks on it.

Two things followed from that, and both are now fixed:

* **Reading the list assumed a header that is not there.**  The array was taken
  to start thirty-two bytes in, so a block written through this code puts its own
  header where entry zero belongs.  Repair read the magic back as an entry --
  `bad agbno 1480672844`, which is `0x5841464c`, the list's own magic spelled out
  as a block number -- and the sequence number as the next one.  A list now says
  whether it carries a header rather than assuming one, and initialising a list
  with no header does not put one there.
* **The block-zero hazard came back through a different door.**  With no header,
  a list block that was never written reads as zeroes, and a zero entry is
  **block 0** -- the first block of the group, which holds its headers and is
  never free space.  Asking whether the list has been *written* says no to a list
  that is full of usable blocks; what has to be asked is whether a slot holds a
  block that could be used, which is neither the null block nor zero.

With both fixed the refill hands back a real free block and the list survives
being written, which it did not before.

### The first row of the AGFL transition table, measured

Freeing blocks and putting them on the free list was measured rather than
assumed, on a real image, with no allocation mixed in:

```text
                    before        after
superblock fdblocks  90624         90624     unchanged
sum(AGF freeblks)    90277         90277     unchanged
AGFL window          (1, 4, 4)     (1, 6, 6)
AGFL entries         [7,8,9,10]    [7,8,9,10,64,65]
```

So the blocks go **straight onto the list and never enter the trees at all**.
Neither count moves, the window grows by the number stocked, and the blocks are
the group's to use but spoken for.  That is what a reservation is, and it is not
what this code was doing -- it freed into the trees and then took them back out,
which arrives at the same numbers by two steps instead of one and is why the
superblock and the trees kept disagreeing by the amount stocked.

The remaining rows are not measured, and the same test cannot measure them: every
allocated block belongs to an inode's data fork, so freeing one without the inode
giving it up is the operation that follows a truncate, which is not built.  The
only free a real image can take honestly is one of the blocks just taken.

### The superblock's total is device-wide, not the groups' sum

Checked on both images, with the group *file* field rather than the group inode
header:

```text
                     sb_fdblocks    sum(AGF freeblks)    difference
xfsv4.img                  90624                90277            347
xfs_writable.img          483138               483122             16
```

**Both differ, including the freshly made one** -- which nothing here has
written to.  So the disagreement is there immediately after creation and is not
something this work introduced.  The likely reading is that the superblock's
count is free blocks on the whole device while each group's is free space in that
group's trees, so the extra blocks are free space belonging to no group -- but
that is not established, only suggested by the two numbers.

What follows for the tests: the stocking test's residual, where the superblock
ends two below the counted total, should be read against this gap rather than as
a bug on its own.  A test that asserts the superblock total equals the sum of the
groups' counts is asserting something false about both fixtures.

### The inode area: less is established than it looked

The measurement offered for the layout question -- forty-two runs of thirty-two
blocks, spread across blocks 16 to 3231, with gaps of thirty-two or forty-eight
-- does not survive checking.  Blocks inside those gaps are **directory and file
data**:

```text
block 48: 58 44 32 44 0f 10 00 f0 ...   ("XD2D": a directory)
block 49: 5f 5f 5f 5f ...                (file data)
block 95: 5f 5f 5f 5f ...                (file data)
```

So the scan that produced the runs was picking up the dinode magic inside data
blocks, and neither the run structure nor the "exactly ilength/2 blocks" count can
be relied on.  What is still established is the small part: two inodes to a block
at offsets 0 and 256, the magic `0x494e`, and inode 32 at block 16 offset 0,
which covers the first chunk and nothing beyond it.

### Where a split's new block comes from

A node that fills up has to become two, and the second needs a block nothing else
is using.  The block comes from the **group's free list**, and the question of
where to ask for it is asked of the same thing that hands out the tree's blocks
rather than being passed in separately: the group holds the answer.  So
`GroupBlocks` grew a third method, `take_btree_block`, and the `N` generic and
the closure the tree functions used to take are gone.

Taking a block moves the free list's window **in the group header, in the same
transaction**, because a list that has lost a block while its header still offers
it hands the same block out a second time.  Both the list and the header move
together or neither does.

**Two claims here were wrong, one of them mine correcting itself.**  First: the
images do *not* show a replenished list.  Second, correcting that: **no image in
the repository has a written free list at all.**  None of the hand-built image,
the freshly made one or the version 5 one contains the list's magic anywhere.
Their group headers name a window -- four entries in most groups, six in group 1
of `xfsv4.img`, eight in group 3 -- over a block that was never written, which is
what a filesystem looks like before it has ever grown a btree node.

So an earlier reading of `flcount = 4`, "the initial four reserved blocks", was
wrong twice over: the window is named but the list behind it was never written,
and the larger counts are not evidence of replenishment.  A list that is created
lazily, on the first split that needs one, is a perfectly ordinary design.

What *is* measurable, and it constrains the design, is this: **`agf_freeblks`
equals the sum of the bno tree's runs exactly** -- difference zero in every group
of both a hand-built and a freshly made image, eight groups measured.  There is no
separate term for the free list in the group's count.  So either a block on the
list is also free in the trees, or it is not counted at all; and if reserving one
removed it from the trees, the two would stop agreeing, which nothing here does.

What is not here is the other half.  The free list is documented as reserved
space for growing the free-space btrees, with more blocks reserved from the group
as it is consumed, so **an empty list is not intrinsically ENOSPC**: the allocator
refills it.  Those blocks are reserved and may not be handed out for ordinary file
data, so metadata-block allocation is a distinct thing from ordinary allocation
rather than a variant of it.

The refill is not written, and the reason is worth recording rather than leaving
as a bare "not done".  Taking a block from the group's free space means mutating
the very trees that are mid-mutation when a split asks for the block.  Taking
only shrinks a record, so it cannot split a leaf and cannot ask for another block
-- but it can merge or share records out, which *writes* parent blocks, and a
parent the outer operation is holding in memory would be written twice.  So the
refill wants its block obtained before the tree work starts, which means knowing
in advance how many a free might need.  That is a design decision rather than a
patch, and it deserves its own look.

The refill is written, and one half of it is tested and passes: a group whose
free list is empty -- which is what **every image in this repository** is in --
hands back a real free block from the group's own space, and stops offering it.
Getting there turned up a hazard worth naming: the list block is never written on
a file system that has not grown a tree, so its array reads as zeroes, and a zero
entry is **block 0**, not the null block.  A list is therefore asked whether it has
been written at all before its window is believed, because the earlier check --
"are any entries not null?" -- said yes, and the refill handed out the
superblock.

The other half is still blocked, and the reason is the one above rather than
anything in the refill itself: mid-split the group header still names the roots
the trees had before, so the length-keyed tree is searched from a root that no
longer reaches the block, and the refill reports `no leaf of the tree covers block
13`.  That test stays ignored with that written on it.

Which leaves stocking as the answer, on two counts.  It is what the free list is
for.  And it was tried once and reverted, because it broke three existing free
tests -- the window handling for a list that has never been written was naming
entries that are all null.  That is the next piece of work here, and it is a
matter of `Agfl`'s window rather than of the allocator.

The AGFL half is tested against a fixture with a real initialised free list,
because that is the only way to reach it here: it checks that the blocks taken
are the ones the list held and in order, that the header's window followed them,
and that the list no longer offers what was taken.

### Finding a free inode: what is known, and what is not

A chunk is sixty-four consecutive inode numbers with a sixty-four bit mask saying
which are free, and its count is the number of set bits in that mask.  Measured on
a freshly made image:

```text
startino = 32   freecount = 57   free = 0xffffffffffffff80
popcount(free) = 57               -- so the two agree
```

57 free inodes are 39 to 95; 32 to 38 are allocated.  The chunk's `freecount` and
the header's `freecount` are one fact, so summing the chunks' counts gives the
header's count exactly, and the check that ties the eight byte mask to the four
byte count beside it is the strongest thing available for catching a wrong
offset: a reader with the offset wrong still produces a plausible count and fails
there instead.

Two things that look contradictory are not:

* `agi_length` is the group's size **in blocks** (153600), not its number of
  inodes.  `agi_count` is the inodes (64).  Reading the length as a count of
  inodes is what makes a chunk of inodes appear to run past the group, when in
  fact two inodes fit in a block.
* The candidates for allocation are **the set bits of the mask, and nothing
  else**.  Not the gaps between chunks, and not any interval worked out from a
  group's inode numbers -- those begin below the group's first chunk and are
  reserved, and synthesising candidates from that interval is how inode zero gets
  handed to somebody.  `agi_newino` is a hint about where to look, not the first
  allocatable number.

Where an inode number lives in the image is now known for the first group of both
images, by finding the inode magic rather than guessing at it:

* the magic is `0x494e`, not the `0x4944` first assumed, which is why looking for
  it found nothing;
* inodes are two to a block, at offsets 0 and 256;
* inode 32 -- the root directory, whose mode the tool agrees on -- is at block 16,
  offset 0, and 33 and 34 follow at 256, 0 and so on.

**What is not established is the layout of a group's inode area for a group with
more than one chunk of inodes.**  In the hand-built image the blocks holding
inodes are not contiguous: the first thirty-two, then a gap, then thirty-two
again, and so on.  Something is interleaved with them and the rule is not
evident from what is here.  So the arithmetic that turns a free inode number into
a block is known for one chunk and not for a group, and guessing at the rest of
the pattern would be exactly the kind of plausible-looking wrong answer this
section exists to avoid.

### Two things that are allocator policy, not format rules

**A take does not have to come from the head of a run.**  The format describes
free space as extents and does not say which part of one an allocation may take;
taking a contiguous subextent and leaving the rest described exactly is equally
valid.  xfuse does take from the head, because that leaves no fragment nobody
asked for, but that is a *policy* and the test pinning it says so.

The structural assertion is separate, and it is what survives any policy: after a
take, the blocks the trees call free are exactly the blocks that were free and
were not taken, and the leaf is still in order.  `a_take_leaves_the_rest_described`
checks that through `remove_range` rather than through the policy-restricted call,
so the check would still hold if the allocator stopped taking from the head -- and
`remove_range` is also the primitive a policy-free take would need.

**Freeing blocks that are already free changes nothing.**  An earlier version cut
the existing record in two around them and added a record of its own, which made
the same blocks free twice: `multiply claimed`.  Cutting is not the answer either,
because it describes the same free space in more records than it needs to -- which
is the two trees disagreeing.  A record that already covers the range says so, so
the range is left alone.

### Sibling links are structure, not navigation

A B+tree block names the block on its left and the block on its right, and those
two names are live structural metadata rather than a hint for getting around
faster.  XFS's consistency checks want sibling pointers to name valid blocks at
the same level, and describe cross-linked sibling lists and loops as a form of
metadata corruption in their own right, checked separately from the parent and
child pointers.  So when a child leaves the tree in the middle of a level, the
chain has to be closed around the gap: if the level is `L <-> N <-> R` and `N`
goes, the live level is `L <-> R`, which means both `L.rightsib = R` and
`R.leftsib = L`.  A root has no siblings of its own.

Three things about that are worth being precise about, because each was a way of
getting it wrong first:

* The chain is doubly linked, so a merge or a drop can touch *three* blocks: the
  surviving one, the far neighbour, and the parent if its key or pointer array
  changes.  Fixing only the neighbour you happen to be holding is the obvious
  mistake.
* Only the *live* blocks are relinked.  The block being dropped is left as it
  is: once it is no longer a member of the tree its contents are no longer part
  of the tree's graph, and whatever writes those bytes next fills them in.  An
  invariant over every block that ever held tree data would fail on XFS's own
  images, and a block that has been freed and reused is not evidence of anything.
* The one that is still an open question, and is recorded rather than guessed:
  which sibling survives a merge.  The implementation must not assume the left
  one does, because if the right-hand block absorbs its left neighbour then the
  survivor's first record changes and the parent's separator key has to change
  with it, even though the pointer stays the same.

A merge also has to cope with the case that turns out to be the ordinary one: the
leaf that has just given up a record sits next to a *full* leaf, and two full
leaves do not fit in one node.  Merging alone turned every such allocation into
`ENOSPC`, because the absorb ran out of room.  So when the two sets of records
will not fit together they are shared across the boundary instead, until the
short node is no longer short, and both nodes stay in the tree -- which also
means no sibling has to be relinked.  Moving the boundary left moves the
*sibling's* first record too, so this is the case where a parent's separator
changes even though no pointer did, and it is why the parent is rebuilt from the
children it actually has.

### The superblock keeps its own count of what is free

The last thing standing between a grown file and an image `xfs_repair` accepts
is one line of repair output:

```
sb_fdblocks 90624, counted 90600
```

The 24 is exactly the number of blocks the test wrote, and the numbers were
traced rather than guessed.  The group's header was updated correctly (its
`freeblks` went 10729 to 10705, and the two trees lost the same 24 blocks), and
`xfs_db` reads the trees back with the same total.  So the trees and the header
agree, and what disagrees is the *superblock's* own count of the free blocks on
the data device, which nothing here was updating.

That was confirmed rather than inferred.  The image is a version 4 file system
with no checksums, so its superblock can be edited: taking `sb_fdblocks` at
offset 144 from 90624 down to 90600 makes `xfs_repair -n` exit clean, with
nothing else changed.  The requirement is therefore that the count moves with
every allocation, and will have to move back the other way when blocks are
freed.

Fixing it did mean writing the superblock, but not by building one.  The count
is *patched* into the first sector's bytes: the bytes are read out of the
transaction, one field is changed in them, and those bytes go back.  Nothing is
rebuilt from the parsed struct, so the parts of the superblock the parser
deliberately threw away cannot be lost on the way out -- and a write of one
sector into a block the first group's headers share cannot take them with it,
because a write that is not block aligned is a read-modify-write.

Two things about that were not obvious and are now pinned by a test rather than
by a comment:

* The count sits at offset 144, which is what the field-by-field parse adds up
  to, and the test checks the constant against the parsed field rather than
  against this file -- so a field moving in the struct cannot quietly leave the
  offset behind.
* A version 5 file system's checksum lives in the second feature word at offset
  200, *not* in the version number, and the checksum itself is at offset 224,
  stored little-endian, covering the bytes before it, then those four bytes
  read as zeroes, then the rest of the sector.  The image the write tests use
  has checksums switched off, so without a test of its own the whole checksum
  branch would be dead code on the path that actually runs.

That test also checks that patching the count changes *nothing else*: the
superblock shares its block with the first group's headers, so a patch that
moved any other byte would be a patch that could take them with it.

`sibling_chains_stay_well_formed` and `closing_a_gap_needs_both_sides` check all
of this over a tree after *every* allocation rather than once at the end, and
the second test exists because of a gap the first one had: draining a group in
block order only ever empties the *first* child, so a check written that way
never sees a child with live children on both sides, and passes just as happily
with that child's right-hand link left dangling.  Both tests were confirmed to
fail when each of the three cases is broken on purpose, which is the only way to
know a check of this kind is doing anything.

One thing worth recording about the free lists: in the v4 test image none of the
four groups has an initialised free list at all, while every group header still
names a window in one.  A file system whose free lists are empty is normal, so
the trees are what an allocator has to be able to read.

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
| a group header and free list can be read and written | done |
| a free space btree node can be read | done |
| a whole free space btree can be walked | done |
| a free space btree can be searched for a run | done |
| a leaf's records can be added and removed | done |
| blocks can be allocated, through a transaction | done |
| an extent can be added to a file | done |
| files can be extended | not started |
| blocks can be allocated | not started |
| blocks can be allocated | not started |
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
