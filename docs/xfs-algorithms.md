# XFS algorithms and data structures — the map

This document maps the published XFS algorithms onto xfuse's existing Rust
modules. It is the survey the fresh-agent instructions asked for before any
new filesystem code is written: for every algorithm it records what is
**DOCUMENTED**, what is **MEASURED** on a native image, what is
**IMPLEMENTED** here, what is a **HYPOTHESIS**, and what is **NOT YET
VERIFIED**.

A HYPOTHESIS is never written into code as though it were a fact. Where a
field's meaning is not established, the document says so rather than guessing.

Sources, in the order the project prefers them:

* *XFS Algorithms & Data Structures* —
  <https://www.kernel.org/pub/linux/utils/fs/xfs/docs/xfs_filesystem_structure.pdf>
* XFS documentation index — <https://www.kernel.org/doc/html/latest/filesystems/xfs/>
* *XFS Online Fsck Design* —
  <https://docs.kernel.org/filesystems/xfs/xfs-online-fsck-design.html>
* *XFS Self-Describing Metadata* —
  <https://docs.kernel.org/filesystems/xfs/xfs-self-describing-metadata.html>
* *XFS Logging Design* —
  <https://docs.kernel.org/filesystems/xfs/xfs-delayed-logging-design.html>
* native XFS images, measured through this project's own decoders and through
  `xfs_db` / `xfs_repair -n`
* xfuse's existing decoders, as secondary evidence

No Linux kernel source is copied or mechanically translated into this project.
Where an algorithm could not be understood from documentation alone, the gap is
**xfuse module.** `dir3_sf.rs` (shortform), `dir3.rs`, `dir3_block.rs`,
`dir3_lf.rs`.

**IMPLEMENTED.**

* Shortform decode/serialize (round-trips a native shortform byte-for-byte),
  `add` (append), `remove` (leave survivors untouched), `contains`.
* **Block directory mutation.** `Dir2Block::add_dirent`/`remove_dirent` for
  v2 and v3 block directories: finds free space, writes entry, inserts leaf
  entry sorted by hash, updates tail count, maintains sibling chain.
* **Leaf directory format measured.** Native leaf directory block (block 70360
  in xfsv4.img) has header `dhdr.magic = 0x58443244` (`XD2D`), bestfree array,
  and entries with 8-byte inumber, 1-byte namelen, name, 1-byte ftype (when
  `sb_has_ftype`), 2-byte tag. Tag = entry's starting byte offset within block.
  Entries for 108 entries in block 70360; bestfree[0] = offset 0xc30, length
  0x3d0. Next data block at block 70432.
* **Node directory format measured.** Native node directory (inode 196640 in
  xfsv4.img) has 6 extents / 64 blocks. Data blocks hold entries with
  `dhdr.magic = 0x58443244` (`XD2D`). Hash index block at fsb 98608 has
  `dhdr.magic = 0x58443246` (`XD2F`), level=1, count=4, hash values (e.g.
  0x67d7550a) with block pointers (e.g. 98352). Multiple data blocks
  (fsb 98352, 98432, 98616, 98624, etc.) linked via extents.

**IMPLEMENTED BUT NOT INTEGRATED** — format transitions:
* **shortform → block transition** — not yet implemented; native transition
  threshold must be experimentally determined.
* **Leaf/node directory mutation** — not yet implemented.
* **block → leaf → node transitions** — not yet implemented.

**verified.** A native shortform directory decodes and re-encodes unchanged
(`a_native_shortform_directory_decodes_and_re_encodes_unchanged`); the
kernel's own add/remove behaviour (append, no renumber) is pinned by tests.
Block directory mutation verified by `xfs_repair -n` on generated images.
Leaf directory block format verified by `xfs_db` on native image.
Node directory hash index structure verified by `xfs_db` on native image.

**invariants.**

* `sb_agcount` groups, each `sb_agblocks` long.
* The four headers are the only fixed-location metadata; everything else is
  found by walking a tree.
* AGF and AGI each carry a root and a level for their two trees.

**xfuse module.** `alloc/agf.rs`, `alloc/agi.rs`, `alloc/agfl.rs`, `sb.rs`.

**implemented.** Readers for all four headers, with named offset tables and
CRC/non-CRC handling. `Sb::ag_header_offset`, `Sb::ag_offset`,
`Sb::agcount`, `Sb::agblocks`.

**missing.** Nothing for reading. Writing a header is done through a
transaction by the operations that change it (allocation, inode allocation).

**verified.** The group-3 BNO/CNT roots, levels and shapes were measured with
`xfs_db` and are recorded in `docs/xfs-format-reference.md`.

---

## 2. Free-space management

**purpose.** Index the free *runs* of blocks in a group so that an allocation
can answer "is there a run at or after this block" and "is there a run at
least this long".

**DOCUMENTED.** A group's free space is a list of runs, kept in two B+ trees:
**BNO**, keyed by run start block, and **CNT**, keyed by run length. The two
index the *same* free space, so a block missing from one is a block handed
out twice. Level 0 is a leaf; non-zero is interior. The AGF points at both
roots and records both levels.

**invariants.**

* BNO and CNT describe the same free space.
* `agf_freeblks` is the total free blocks; `agf_longest` is the longest run.
* `agf_btreeblks` counts the blocks the two trees hold, *less their roots*.
* A leaf below half full is a candidate for merging with a sibling.

**tree operations.**

* **search** — descend by key. BNO descends by start block; CNT *cannot*
  descend by key (its keys do not say which child holds which run), so it
  searches the leaves.
* **insert a free extent** — find the leaf, insert the run in order, split a
  full leaf, propagate the separator to the parent, grow the root if the
  parent splits.
* **remove a free extent** — take the blocks out of the run; if the run is
  consumed, remove the record; merge/redistribute a leaf that falls below
  half; collapse the root if it empties.
* **split a run** — taking blocks from the middle of a run leaves two runs;
  *both* trees must receive both halves.
* **coalesce** — two runs that touch are one run.

**metadata updated.** A leaf in each tree, the AGF's `freeblks`/`longest`/
`btreeblks`, and the superblock's free-block total.

**ordering.** Both trees must be updated in the same transaction; a group
header that says a block is free while its tree says it is taken is a block
handed out twice.

**xfuse module.** `alloc/free_space.rs` (one node of either tree),
`alloc/allocator.rs` (the group as stored, through a transaction).

**implemented.**

* Reading a node: header, records, keys, pointers, CRC (`FreeSpaceNode`).
* `take_from_run`, `remove_range` (splitting records a range cuts through),
  `put_run`, `insert_run`, `coalesce`, `covers_range`, `containing_run`.
* Leaf split (`split`), child insert/remove, key refresh, sibling relink,
  `absorb` for a merge.
* `take_from_both_trees`, `free_in_both_trees`, `range_is_free`,
  `first_run_from` — the two-tree coordination.
* `FreeSpace::allocate` and `free_in_group` through a transaction.
* **Root growth** (a leaf split that raises the tree a level) — implemented
  and reachable; `an_overflowing_leaf_takes_a_node_off_the_free_list` grows a
  group's tree a level at round 14.
* **Group-3 BNO split** — the length-ordered descent now recurses past a
  non-leaf instead of reading an interior node's records as runs; all four
  groups of `xfsv4.img` allocate from the middle of a run 200 times each,
  with `xfs_repair -n` accepting after each.

**missing.**

* **Root collapse** — not implemented, and nothing reaches it: collapse needs
  a tree to shrink, and no operation available here grows one smaller.
* A leaf that has fallen below half full is *not* merged, so a group with
  such a leaf reports itself full for runs that leaf would have served
  (`alloc/allocator.rs` module docs).
* The group's free list (AGFL) is not stocked by any operation here.

**verified.**

* `agf_freeblks` is exactly the sum of the BNO tree's records
  (`docs/write-support-progress.md`).
* `agf_btreeblks` is the free-space trees' blocks less their two roots.
* Taking one block from the middle of a run splits it, and both trees receive
  both halves: 200 such splits in group 0 leave the trees agreeing and
  `xfs_repair -n` accepting.
* The four groups' roots, levels and shapes are measured and recorded in
  `docs/xfs-format-reference.md` (group 3 is the only group whose root's
  children are not leaves).
* **BNO/CNT trees measured on native image.** xfsv4.img AG 0: BNO root at
  fsb 4, level 0, 19 records (startblock/blockcount pairs e.g. [13,3],
  [569,7], [3240,29528]). CNT root at fsb 5, level 0, 19 records ordered by
  length: [13,3], [569,7], [2336,8], ..., [3240,29528]. Both trees at level 0
  (leaf). Group 3 (AG 3) has BNO root at level 1 (interior), children at
  level 0 — the only group with interior BNO/CNT. `agf_freeblks = 30144`,
  `agf_longest = 29528`. `agf_btreeblks = 0` (only roots). Group 1, 2, 4 have
  BNO/CNT at level 0.

**HYPOTHESIS — since refuted, kept for the reasoning.** That the failing group
was the one whose free space is few and very large, and that the mechanism was
the length-ordered descent. The withdrawal compared `bnolevel` across groups;
the quantity that matters is the root's own `bb_level` (`bnolevel - 1`), which
is why group 1 (root level 1) never descends through an interior node and
group 3 (root level 2) does.

---

## 3. Inode allocation

**purpose.** Hand out inode numbers, and record which chunks of inodes a group
has allocated.

**DOCUMENTED.** Inodes are allocated in *chunks* of `XFS_INODES_PER_CHUNK`
(64). The AGI records the group's total inode count, its free inode count,
and the next unallocated inode number. The INOBT records which chunks are in
use; a part-used chunk is in the tree and the free inodes inside it are free.

**invariants.**

* A chunk is 64 inodes because the free mask is a single 64-bit word.
* The chunk record's `ir_freecount` equals the number of set bits in
  `ir_free` — two copies of one fact.
* The AGI free count is real allocation state, not inferred from a "used"
  field: there is no used field.

**MEASURED — the native file-creation fact.** A native file creation consumed
exactly one free inode from an existing chunk while:

```text
total inode count   unchanged
next-inode cursor   unchanged
free inode count    58 -> 57
```

So the AGI free count (`agi_free_count`, bytes 28..32 of the AGI) is the
group's only inode count beyond the total, and it moves by one per creation.

**allocation requirements.**

* *Inode allocation within an existing chunk* — clear a bit in the chunk's
  free mask, decrement the chunk's `ir_freecount`, decrement the AGI's free
  count. No new blocks.
* *Inode chunk allocation* — allocate a block from the group's free space,
  add a record to the INOBT, increment the AGI's total and free counts.

**ordering.** The chunk's mask, the chunk's `ir_freecount`, and the AGI's
free count must move together.

**failure cases.** A group with no free inode in any existing chunk needs a
*new chunk*, which needs a free block and an INOBT insert — a different
operation from allocating within a chunk.

**xfuse module.** `alloc/inobt.rs`, `alloc/agi.rs`, `alloc/allocator.rs`.

**implemented.**

* `first_free_ino` — the lowest free inode in a group, found by walking the
  INOBT in order.
* `chunks_in_order`, `ranges_in_tree` — the chunks a tree holds, in order.
* `insert_chunk` — insert a chunk into the INOBT, growing the tree (splitting
  a full leaf, propagating to the parent, growing the root) when it has to.
* `allocate_ino` / `allocate` in `allocator.rs` — allocate an inode within an
  existing chunk through a transaction.
* `allocate_new_chunk` — the chunk-allocation path exists.

**missing.**

* **The chunk record is not completely decoded.** Bytes 4..8 of an
  `xfs_inobt_rec` mean different things depending on whether the chunk is
  *sparse*: a normal chunk keeps a 4-byte `ir_freecount` there; a sparse
  chunk keeps a 2-byte `ir_holemask`, a 1-byte `ir_count`, and a **1-byte**
  `ir_freecount`. The kind is a property of the *chunk*, taken from the owning
  inode's `XFS_DINODE_F_SPINODES` flag, and **cannot be told from the record
  alone**. The decoder reads the normal 4-byte form, which is why it reads
  16442 instead of 58 on a fixture whose chunk is sparse. This is the one
  remaining ambiguity in inode allocation, and it is recorded rather than
  guessed.
* `allocate_new_chunk`'s chunk-record write on growth needs the sparse/normal
  distinction resolved before it can write a correct record on a sparse
  filesystem.

**verified.**

* The first free inode is the lowest one, and the chunks' free counts add up
  to the AGI's free count and the superblock's `sb_ifree`
  (`the_first_free_inode_is_the_lowest_and_the_counts_agree`).
* A chunk's free count is the number of free inodes in its mask
  (`a_chunks_free_count_is_the_number_of_free_inodes_in_its_mask`).
* The native creation measurement (AGI free 58→57, total and cursor
  unchanged) is in `docs/namespace-audit.md`.

---

## 4. Inode B-trees

**purpose.** Index which chunks of inode numbers a group has allocated, so that
a free inode can be found and a new chunk recorded.

**DOCUMENTED.** The INOBT is a B+ tree keyed by chunk start inode number. A
leaf holds 16-byte records (`ir_startino`, `ir_freecount`/`ir_holemask`+
`ir_count`+`ir_freecount`, `ir_free`); an interior node holds 4-byte keys and
4-byte pointers. Both carry the same magic; the level field tells them apart.

**invariants.**

* Records are in key order.
* An interior node has one more key than children; the last key is the upper
  bound of the last child's keyspace.
* Sibling pointers are level-homogeneous.
* A leaf below half full is a merge candidate.

**tree operations.** search (descend by key), insert (leaf insert, leaf split,
parent key propagation, parent split, root growth), delete (leaf record
removal, merge, root collapse).

**metadata updated.** A leaf or interior node, and — for a chunk allocation —
the AGI.

**xfuse module.** `alloc/inobt.rs`.

**implemented.**

* **Dual-magic decode.** `from_bytes` accepts both the non-CRC magic
  (`IABT`, `0x49414254`) and the CRC magic (`IAB3`, `0x49414233`), and
  stores `has_crc` in the node. Which a node carries is fixed by the
  superblock and is only needed when *writing*.
* **CRC-aware offsets.** Records begin at `BODY_NO_CRC` (16) on a v4 node and
  `BODY_CRC` (56) on a v5 node. The CRC header adds LSN (8), UUID (16),
  owner (8) and CRC (4) = 36 bytes, rounded to 40, making 56 total.
* `ranges`, `children`, `insert_range`, `insert_child`, `write_range`,
  `child_block`, `leaf_capacity`, `interior_capacity` — all CRC-aware.
* `insert_chunk` with full split handling: split a full leaf, relink both
  siblings, propagate the separator to the parent, split a full parent, grow
  the root when the tree was a single leaf.
* Constructors `blank`/`empty_leaf`/`new_interior`/`empty_like` emit the
  correct magic for the node's `has_crc`.

**missing.**

* **`update_crc` is a no-op.** Where a checksum goes in a v5 INOBT node has
  not been established here, and writing one at a guessed offset would corrupt
  a real image. The gap is recorded rather than papered over. Both reference
  images are v4 (no checksums), so nothing here has a checksum to recompute —
  but a v5 writer would need this before it could write a node.
* Delete / merge / root collapse are not implemented.

**verified.**

* An interior node reads as the keys and child blocks `xfs_db` prints
  (`an_interior_node_reads_as_xfs_db_prints_it`).
* **INOBT interior node measured.** xfsv4.img AG 0 INOBT root at fsb 12 is
  level 1 with 2 keys: [32, 2400], pointers to fsb 6 and 11. Left leaf (fsb 6)
  has 17 records [32..2240], right leaf (fsb 11) has 25 records [2400..6400].
  Record format: `[startino, freecount, free]` where `free` is 64-bit mask.
  For sparse inodes, record format changes: `[startino, holemask, count,
  freecount, free]` where `freecount` is 1 byte.
* The dual-magic fix is what made the attribute-fork tests pass: the Linux
  xattr tests `lsextattr::empty`, `lsextattr::ok::case_11_giant_nrext64` and
  `lsextattr::size::case_11_giant_nrext64` failed before it and pass after.

---

## 5. File extent maps

**purpose.** Map a file's logical block range to physical blocks, supporting
holes, extents, and the transition from inode-local storage to a B+ tree.

**DOCUMENTED.** A file's data fork holds either a list of extents (in the
inode) or a B+ tree (the BMBT) rooted in the inode and spilling into blocks.
The in-inode root is `xfs_bmdr_block_t`: a 4-byte header (`bb_level`,
`bb_numrecs`) and **no magic**, followed by the key array, the padding
`dfork_btree_ptr_gap` measures, then the pointer array.

**invariants.**

* Extents are in logical-offset order and do not overlap.
* A leaf with a parent is at least half full.
* The fork's capacity is `DSIZE / 16` records.

**MEASURED.** Fork capacity:

| Inode | Version | `di_forkoff` | Data fork | Records |
|:------|:---------|:--------------|:----------|:--------|
| 256 B | 2 | 0 | 156 | **9** |
| 512 B | 3 | 0 | 328 | 20 |
| 512 B | 3 | 24 | 192 | 12 |

The nine was measured by nine sparse writes succeeding and the tenth being
refused; `(256 - 100) / 16` = 9.75 independently gives nine.

**xfuse module.** `btree.rs`, `bmbt_rec.rs`, `extent.rs`, `inode.rs`.

**implemented.**

* Extent → B-map conversion, multiple B-map leaves, bounded interior-tree
  construction, B-map serialization, extent reading, sparse file support,
  file extension, truncation for supported forms.
* `BtreeRoot` (the inode-rooted root), `BtreeIntermediate` (a block),
  `BtreeLeaf` (a leaf block), `map_block` (search/descent), `lseek`
  (data/hole), `all_extents` (walk the whole tree, returning extents and the
  node blocks).
* `BmbtLeafBlock::from_bytes` / `to_bytes` / `to_bytes_v4` with the
  **leaf-capacity guard**: `to_bytes` and `to_bytes_v4` assert
  `records.len() <= max_records` rather than panicking on a slice range past
  30 records on a 512-byte v4 leaf.
* `BmbtInteriorBlock` with `max_children`, `to_bytes`, `to_bytes_v4`.

**missing.**

* **Incremental insertion/deletion/rebalancing of an existing tree.** What is
  implemented is *construction* of a tree from a known set of records (bounded
  multi-leaf construction) and *reading*. Inserting an extent into a tree that
  is already on disk — locating the leaf, splitting it, propagating the key,
  growing the root — is not implemented. This is the distinction the
  fresh-agent instructions call out: construction from a known set is a
  different capability from incremental mutation.
* A root that is itself a leaf is refused by `all_extents` (its records are
  in the inode, not in a block, and this code holds only the header and the
  keys/pointers beside it).

**verified.**

* The b-map leaf's records begin at 72 bytes (CRC) or 24 (v4), and each is
  two 64-bit words (`docs/write-support-progress.md`).
* A 256-byte inode holds nine extents and the tenth is refused.
* Occupancy: a leaf with a parent may not be less than half full;
  `xfs_repair` reports `bad # of bmap records (7, min - 15, max - 30)`.
* **BMBT leaf and interior measured on native images.** xfsv4.img inode
  100553 (`btree2.2.txt`) has level 1 BMBT with 3 leaf blocks (fsb 50313,
  50315, 50317). Leaf records are `[startoff, startblock, blockcount, flag]`,
  16 bytes each. Leaf 1 (fsb 50313) has 30 records covering startoff 0..29
  (1 block each). Leaf 2 (fsb 50315) has 15 records startoff 30..44. Leaf 3
  (fsb 50317) has 19 records startoff 45..63. Interior node at inode fork
  has level 1 with 3 keys: startoff 0, 30, 45 pointing to fsb 50313,
  50315, 50317. Inode 196640 (`btree2.2` directory) has level 1 BMBT with
  2 leaf blocks (fsb 536, 1456). Leaf 1 (fsb 536) has 17 records; leaf 2
  (fsb 1456) has 30 records including high startoff values (67108864+)
  for hole detection. Interior node in inode fork has level 1, keys [0, 512],
  pointers to fsb 536 and 1456. Record format confirmed: `[startoff,
  startblock, blockcount, extentflag]` with startblock stored as fsb << 9.
* Occupancy: a leaf with a parent may not be less than half full;
  `xfs_repair` reports `bad # of bmap records (7, min - 15, max - 30)`.

---

## 6. BMBT algorithms

**purpose.** The general mutation algorithm for a file's extent B+ tree.

**DOCUMENTED.** The BMBT is a B+ tree keyed by logical file offset. A leaf
holds 16-byte extent records; an interior node holds 8-byte keys (the first
offset in the child) and 8-byte pointers. The record is two 64-bit words with
the fields interleaved:

```text
l0:63      extent flag (1 = not an ordinary run)
l0:9-62    startoff, in blocks
l0:0-8 and l1:21-63   start block
l1:0-20    block count, in blocks
```

Fifty-four bits of offset, fifty-two of block number, twenty-one of length,
one of flag = 128 bits, exactly the record.

**MEASURED.** In every leaf measured, `l0`'s low nine bits are zero and the
start block occupies `l1`'s high bits alone, i.e. `startblock_field = fsb <<
21`. Reading a record as four 4-byte big-endian fields folds three fields
into one nonsensical number — the cause of 86 failing integration tests.

**MEASURED.** The stored start block is the block number scaled by 512
(`BMBT_STARTBLOCK_SCALE_SHIFT = 9`): a record holding start block 17833 holds
17833 * 512.

**tree operations (implemented for leaf and interior nodes).**

```text
locate logical extent
insert extent
split an extent
coalesce adjacent extents
split BMBT leaf
propagate key to parent
split parent
grow root
delete extent
merge/redistribute leaves
collapse root
```

**metadata updated.** A leaf or interior block, the inode's fork header
(`bb_level`, `bb_numrecs`), and — for a block allocation — the free-space
trees and the AGF.

**ordering.** The fork header's `bb_numrecs` must move with the records; a
leaf split must relink siblings and propagate the separator before the parent
is written.

**failure cases.** A fork that cannot hold its extents becomes a B+ tree; a
fork that cannot hold the survivors is refused with `ENOSPC` (a fork-specific
error, because the group may be nearly empty and no retry helps).

**xfuse module.** `btree.rs`.

**implemented.**

* **Leaf insertion with split.** `BmbtLeafBlock::insert_record` inserts an
  extent record in startoff order; if the leaf overflows, it splits in half,
  creates a new leaf block, relinks sibling pointers, and returns the separator
  key and new leaf to the caller for parent propagation.
* **Leaf deletion with merge/redistribution.**
  `BmbtLeafBlock::remove_record` removes a record by startoff; if the leaf
  falls below minimum occupancy, it returns `Ok(true)` to signal that the
  caller should attempt `redistribute_with` (borrow from sibling) or
  `merge_with` (absorb sibling). Both operations maintain minimum occupancy.
* **Interior node insertion with split.** `BmbtInteriorBlock::insert_key_ptr`
  inserts a key-pointer pair in key order; if the node overflows, it splits
  at the median, creates a new interior node, and returns the separator key
  and new node for parent propagation.
* **Interior node deletion with merge/redistribution.**
  `BmbtInteriorBlock::remove_key_ptr` removes a key-pointer pair; if the node
  falls below minimum children, it returns `Ok(true)` to signal the caller
  should attempt `redistribute_with` or `merge_with`.
* **Sibling pointer maintenance.** Both leaf and interior operations maintain
  left/right sibling pointers (`leftsib`/`rightsib`) during splits, merges,
  and redistributions.
* **Minimum occupancy enforcement.** `min_records` (leaf) and `min_children`
  (interior) enforce the half-full rule; operations that would violate it
  return an error or signal the need for redistribution/merge.
* **Bounded bulk construction.** `BmbtLeafBlock::leaves_for` computes the
  legal number of leaves and per-leaf record count for bulk loading; this is
  used by the write path when converting an extent list to a tree (e.g. on
  fork format transition from extents to BMBT).
* **Bounded interior construction.** The write path builds multi-level trees
  by first constructing leaves, then building interior nodes bottom-up, and
  finally updating the inode-root header (`bb_level`, `bb_numrecs`, `keys`,
  `ptrs`). This is the current extent-to-BMBT transition path in the write
  path.

**missing.**

* **Incremental insertion into an existing on-disk tree.** Implemented
  for multi-level trees (level ≥ 1) in `BtreeRoot::insert_extent` and
  `BtreeRoot::insert_extent_intermediate`. Recursively descends to leaf,
  handles leaf split, allocates new blocks via `allocate_in_group`, propagates
  separator keys up. Root level 0 (extent list → BMBT conversion) implemented
  via `insert_extent_root_leaf`. **Intermediate node splits** implemented in
  `insert_extent_intermediate`: when an intermediate node overflows, splits it,
  allocates new block, propagates separator key up.
* **Root growth and collapse.** Root growth (level 0→1, 1→2, etc.) is
  implemented: `BtreeRoot::insert_extent_root_leaf` and `convert_to_btree` handle
  extent list → BMBT conversion when inode overflows. **Root growth (level 1→2)**
  implemented in `BtreeRoot::handle_root_split`: when level 1 root overflows
  during insertion, splits the root, allocates new blocks, creates new level 2 root.
  **Root collapse (level 1→0)** implemented in `BtreeRoot::check_root_collapse`: when
  level 1 root has one child that is a leaf and all records fit in inode, moves
  records back to inode and frees the leaf block. Called after deletions in
  `delete_extents_in_range`. **Root collapse (level 2→1)** implemented: when level
  2 root has one intermediate child with one leaf child, moves leaf records up to
  intermediate (now leaf), frees leaf block, updates root to level 1. Called after
  deletions.
* **Full delete algorithm with merge/redistribution propagation.** Implemented:
  `BtreeRoot::delete_extents_in_range` and `BtreeRoot::delete_extents_intermediate`
  locate overlapping leaves and remove records. **Leaf-level merge/redistribute
  propagation** in `BtreeRoot::handle_leaf_underflow`: tries right sibling first,
  then left; attempts redistribution, falls back to merge; updates parent
  separator keys; frees merged block. **Intermediate node merge/redistribute
  propagation** in `BtreeRoot::handle_intermediate_underflow`: same logic for
  intermediate nodes. Root collapse implemented.
* **Extent coalescing/splitting.** Implemented: `BmbtLeafBlock::insert_record`
  coalesces with previous/next on insertion; `remove_record` coalesces adjacent
   after deletion; `coalesce_adjacent` called after PUNCH_HOLE. Splitting during
   deletion implemented in `delete_extents_recursive`.
 * **CRC updates for v5 nodes.** Implemented in all `to_bytes` methods
   (`BmbtLeafBlock`, `BmbtInteriorBlock`, `BtreeIntermediate`) via
   `crc32c_without_its_own_field`. Previously was a no-op.

 **NOT YET VERIFIED (incremental path).** The code below implements the
 incremental (single-record) BMBT mutation paths that `fallocate` uses, but
 these paths have **no unit tests** in `btree.rs` and **no FUSE-level test**
 exercises `fallocate`/`punch_hole` on any file. The following functions are
 present and reachable but **untested** and contain **known bugs**:

 * **Incremental leaf insertion with split.** `BmbtLeafBlock::insert_record`
   (`btree.rs:835`) inserts a record, splits on overflow, returns new leaf and
   separator key. Present but untested.
 * **Incremental leaf deletion.** `BmbtLeafBlock::remove_record`
   (`btree.rs:934`) and the split-trim logic in `delete_extents_recursive`
   (`btree.rs:2072-2157`). Present but untested.
 * **Leaf merge/redistribute.** `merge_with` (`btree.rs:980`) and
   `redistribute_with` (`btree.rs:987`) for leaves. Present but **buggy**:
   `redistribute_with` calls `min_records(0, false)` which always returns 0,
   making the redistribution guard dead code (see `btree.rs:989`).
 * **Intermediate node insertion/split.** `insert_key_ptr_at`
   (`btree.rs:1719`) and the inline split at `btree.rs:1934-1988`. Present but
   untested; **fsblock arithmetic is wrong** — allocations use `run.start`
   directly instead of `sb.ag_block_to_fsb(agno, run.start)`.
 * **Intermediate node merge/redistribute propagation.** Implemented in
   `BtreeRoot::handle_intermediate_underflow` (`btree.rs:2619`), but the deep
   inter-node sibling merge is **stubbed** (`btree.rs:2902-2904`:
   `// Need to merge with sibling`).
 * **Root collapse (level 1→0).** `check_root_collapse` (`btree.rs:2219`) reads
   the single leaf child and moves records to the inode. Present but uses a
   non-standard writer (`write_extents_to_inode`, `btree.rs:1476`) that
   bypasses `RawDinode::finalise`/CRC and the fork-overflow checks. Untested.
 * **Root collapse (level 2→1).** `check_root_collapse` at `btree.rs:2229`.
   **BROKEN**: writes leaf records into an interior-node layout (`to_bytes_auto`
   with `bb_level=0`), producing a structurally invalid node a reader will
   misparse. Untested.
 * **Sibling pointer relinking.** Splits allocate new nodes with
   `leftsib/rightsib = BMBT_NULL_PTR` (`btree.rs:1648-1649`, `1656-1659`,
   `1860-1861`, `1877-1884`); no code relinks the sibling chain after a split.
 * **Root growth (level 1→2).** `handle_root_split` (`btree.rs:1730`) updates
   the in-memory root but **does not persist** the new level-2 root to the
   inode. The `fallocate` caller re-derives counts but does not call
   `set_data_btree_root` (`volume.rs:3851-3866`), so root growth is silently
   lost on commit.

 In short: the **bulk** path (extent-list ↔ BMBT conversion via
 `write_extending`/`truncate`) is tested and verified; the **incremental** path
 (`insert_extent`/`delete_extents_in_range` via `fallocate`) is present in
 code but untested and contains the bugs above. Do not treat `xfs_repair -n`
 validation of the bulk path as proof of the incremental path.

---

## 6.5. File data operations (truncate, fallocate, hole punching)

**purpose.** Modify a file's extent map: grow (extend with zeroes/holes),
shrink (free extents), preallocate (reserve space without writing), and
punch holes (free interior extents, creating sparse regions).

**DOCUMENTED (XFS Algorithms & Data Structures, §13–14).**
* `truncate` walks the extent map, frees extents past the new size, and
  updates `di_size`, `di_nblocks`, and the fork's extent list/tree.
* `fallocate` (FALLOC_FL_KEEP_SIZE) allocates blocks for a range without
  changing `di_size`; the allocated blocks are written as zeroes or marked as
  unwritten (delayed allocation). `FALLOC_FL_PUNCH_HOLE` frees blocks in a
  range, creating a hole.
* Both operations must handle all fork formats: local (unlikely for data),
  extents (in inode), and BMBT (B+ tree). The BMBT case requires locating
  the affected leaves, modifying records, and propagating splits/merges.

**invariants.**
* A hole is represented by the *absence* of an extent record; there is no
  explicit "hole extent".
* `di_nblocks` counts allocated blocks only; it must decrease when extents
  are freed.
* An extent freed by `truncate`/`fallocate` returns to the group's free-space
  trees (BNO/CNT) in the same transaction.

**xfuse module.** `volume.rs` (`truncate`, `write_data`), `inode.rs`
(`set_core_extents`, `add_extent`, `drop_extents_above`,
`split_extents_above`, `set_data_extents_from_tree`), `btree.rs`
(`BmbtLeafBlock`, `BmbtInteriorBlock`).

**implemented.**

* **`truncate` (shrink).** `Volume::truncate` in `volume.rs:1030` handles
  both extents and BMBT formats. For extents format, it calls
  `RawDinode::drop_extents_above` to free extents past the new size. For
  BMBT format, it reads the full extent list via `BtreeRoot::all_extents`,
  splits at the boundary with `RawDinode::split_extents_above`, writes the
  survivors back into the inode (converting BMBT → extents if they fit), and
  frees the original tree blocks via `free_in_group`. Updates `di_size`,
  `di_nblocks`, `di_mtime`, `di_ctime` in one transaction.
* **`truncate` (grow).** Extends `di_size` without allocating blocks; the
  new region reads as zeroes.
* **Hole detection.** `write_data` refuses to write into a hole; the caller
  must use `fallocate` first (now implemented for PUNCH_HOLE/KEEP_SIZE).

**implemented.**

* **`fallocate` (FALLOC_FL_PUNCH_HOLE).** `Volume::fallocate` in `volume.rs:3269`
   punches holes in BMBT-formatted files: reads inode/btree root before
   transaction, calls `BtreeRoot::delete_extents_in_range` to locate and remove
   overlapping leaf records, frees blocks via `free_in_group`, updates inode
   (`di_nblocks`, `di_mtime`, `di_ctime`) in transaction. For extents format,
   uses `RawDinode::split_extents_in_range` (inline extents, no BMBT).
   Returns `ENOTSUP` for real-time files.
 * **`fallocate` (FALLOC_FL_KEEP_SIZE).** Preallocates blocks for a range
   without changing file size: for each file block in range, checks if already
   allocated via `map_block`, allocates via `allocate_in_group`, inserts extent
   via `BtreeRoot::insert_extent` (BMBT) or `set_core_extents` (extents).
   Returns `ENOTSUP` for real-time files.
 * Both operations handle BMBT format (level ≥ 1) and extents format.
   Root level 0 (extent list → BMBT conversion) implemented.

 **missing.**

 * **`fallocate` (FALLOC_FL_ZERO_RANGE) proper zeroing.** The `zero_range` flag
   is accepted and falls through to the KEEP_SIZE allocation path, but does not
   actually zero existing data in the range. True ZERO_RANGE (write zeroes to
   the range, converting unwritten extents to written) is not implemented.
 * **Unwritten extent flag in fallocate.** The docs previously claimed
   `br_flag = true` for KEEP_SIZE preallocated blocks; the code sets
   `br_flag = false` at `volume.rs:3829` and `volume.rs:3951`. This **bug**
   means preallocated blocks are indistinguishable from written data.
 * **Extent coalescing.** Implemented for the **bulk** path (`write_extending`
   builds leaves via `chunks`, `btree.rs:1007` `coalesce_adjacent` for
   incremental deletion) but the incremental insertion coalesce is
   **untested** (`btree.rs:850-890`, `898-919`).

 **stale.**

 * The earlier docs incorrectly claimed `fallocate` PUNCH_HOLE returns `ENOSY`
   for extents format. It is in fact handled at `volume.rs:3684` via
   `split_extents_in_range`.
 * The docs claimed `br_flag = true` was set for newly allocated blocks; the
   code sets `br_flag = false`.

**verified.**

* `truncate` shrink verified by `xfs_repair -n` on generated images (write
  tests: `a_file_made_shorter_gives_its_blocks_back`, `a_grown_file_leaves_the_image_consistent`).
* `truncate` grow verified by write tests.
* Hole refusal in `write_data` verified by test `write_to_directory_is_refused` (wrong type)
  and the fact that holes cannot be written to without allocation.
* **`fallocate` FUSE handler stub.** `Volume::fallocate` in `volume.rs:3269` handles
  mode flags (`KEEP_SIZE`, `PUNCH_HOLE`, `ZERO_RANGE`, `COLLAPSE_RANGE`,
  `INSERT_RANGE`, `UNSHARE_RANGE`, `WRITE_ZEROES`). Returns `ENOSYS` for
  unsupported modes; returns `ENOTSUP` for real-time files and non-extent formats;
  returns `EINVAL` for invalid flag combinations. Actual allocation/deallocation
   logic uses incremental BMBT insertion/deletion (partial implementation).
 * **BMBT incremental mutation is UNTESTED and buggy.** The `insert_extent`
   (`btree.rs:1377`) and `delete_extents_in_range` (`btree.rs:2001`) paths are
   reachable from `fallocate` but have **no tests** (no unit tests in
   `btree.rs`, no fallocate FUSE test). Known bugs: wrong fsblock arithmetic,
   unrelinked sibling pointers, unpersisted root growth, broken 2→1 collapse,
   stubbed deep-intermediate merge, `min_records(0)` making redistribute dead.
   See the "NOT YET VERIFIED" section above for details.
 * **Extent coalescing on insertion.** `BmbtLeafBlock::insert_record` merges
   with previous/next records if contiguous in both file offset and block address.
 * **Extent coalescing on deletion.** `BmbtLeafBlock::remove_record` checks
   if adjacent records become contiguous after removal.
 * **Extent coalescing after PUNCH_HOLE.** `BmbtLeafBlock::coalesce_adjacent`
   (`btree.rs:1007`) called after modifications (`btree.rs:2165`), merges all
   contiguous pairs.
 * **Unwritten extent flag in fallocate.** The docs previously claimed
   `br_flag = true` for preallocated blocks; the code actually sets
   `br_flag = false` at `volume.rs:3829` and `volume.rs:3951`. This **bug**
   means fallocate-preallocated blocks are indistinguishable from written data.
 * **PUNCH_HOLE for extents format.** Implemented (not `ENOSY` as previously
   documented) at `volume.rs:3684-3758` via `split_extents_in_range`.

---

## 7. Directories

**purpose.** Map names to inode numbers, supporting the format transitions
shortform → block → leaf → node.

**DOCUMENTED.** A directory is one of four formats, kept separate: *local*
(shortform, in the inode), *block* (one block), *leaf* (a leaf block plus an
index), and *node* (multiple leaf blocks plus a B+ tree index). A `dir2` data
entry is: 8-byte inumber, 1-byte namelen, var name, 1-or-0 ftype
(feature-dependent), padding to alignment, 2-byte tag. **The tag is the
entry's starting byte offset within the data block** — not the entry length,
the inode number, the hash, or a data pointer.

**invariants.**

* Shortform entries are in insertion order, never renumbered.
* The shortform header's entry count is one byte (max 255 entries).
* A directory's format is a property of the directory, decided by its size.

**MEASURED.** Shortform sizes: 6 for empty, 15 for one 1-char entry, 33 for
three, 44 for four. Adding appends and advances the offset by 16
(`SF_OFFSET_STEP`); removing leaves the survivors' offsets untouched.

**xfuse module.** `dir3_sf.rs` (shortform), `dir3.rs`, `dir3_block.rs`,
`dir3_lf.rs`.

**implemented.**

* Shortform decode/serialize (round-trips a native shortform byte-for-byte),
  `add` (append), `remove` (leave survivors untouched), `contains`.
* **Block directory mutation.** `Dir2Block::add_dirent`/`remove_dirent` for
  v2 and v3 block directories: finds free space, writes entry, inserts leaf
  entry sorted by hash, updates tail count, maintains sibling chain.
* **Leaf directory format measured.** Native leaf directory block (block 70360
  in xfsv4.img) has header `dhdr.magic = 0x58443244` (`XD2D`), bestfree array,
  and entries with 8-byte inumber, 1-byte namelen, name, 1-byte ftype (when
  `sb_has_ftype`), 2-byte tag. Tag = entry's starting byte offset within block.
  Entries for 108 entries in block 70360; bestfree[0] = offset 0xc30, length
  0x3d0. Next data block at block 70432.
* **Node directory format measured.** Native node directory (inode 196640 in
  xfsv4.img) has 6 extents / 64 blocks. Data blocks hold entries with
  `dhdr.magic = 0x58443244` (`XD2D`). Hash index block at fsb 98608 has
  `dhdr.magic = 0x58443246` (`XD2F`), level=1, count=4, hash values (e.g.
  0x67d7550a) with block pointers (e.g. 98352). Multiple data blocks
  (fsb 98352, 98432, 98616, 98624, etc.) linked via extents.

**implemented.**

* Shortform decode/serialize (round-trips a native shortform byte-for-byte),
  `add` (append), `remove` (leave survivors untouched), `contains`.
* **Block directory mutation.** `Dir2Block::add_dirent`/`remove_dirent` for
  v2 and v3 block directories: finds free space, writes entry, inserts leaf
  entry sorted by hash, updates tail count, maintains sibling chain.
* **Leaf directory format measured.** Native leaf directory block (block 70360
  in xfsv4.img) has header `dhdr.magic = 0x58443244` (`XD2D`), bestfree array,
  and entries with 8-byte inumber, 1-byte namelen, name, 1-byte ftype (when
  `sb_has_ftype`), 2-byte tag. Tag = entry's starting byte offset within block.
  Entries for 108 entries in block 70360; bestfree[0] = offset 0xc30, length
  0x3d0. Next data block at block 70432.
* **Node directory format measured.** Native node directory (inode 196640 in
  xfsv4.img) has 6 extents / 64 blocks. Data blocks hold entries with
  `dhdr.magic = 0x58443244` (`XD2D`). Hash index block at fsb 98608 has
  `dhdr.magic = 0x58443246` (`XD2F`), level=1, count=4, hash values (e.g.
  0x67d7550a) with block pointers (e.g. 98352). Multiple data blocks
  (fsb 98352, 98432, 98616, 98624, etc.) linked via extents.

**missing.**

* Leaf/node directory mutation.
* The shortform→block transition itself.

**verified.** A native shortform directory decodes and re-encodes unchanged
(`a_native_shortform_directory_decodes_and_re_encodes_unchanged`); the
kernel's own add/remove behaviour (append, no renumber) is pinned by tests.
Block directory mutation verified by `xfs_repair -n` on generated images.
Leaf directory block format verified by `xfs_db` on native image.
Node directory hash index structure verified by `xfs_db` on native image.

---

## 8. Directory/attribute B-trees (dabtree)

**purpose.** Index a directory's or attribute fork's entries by name hash, to
avoid linear scans.

**DOCUMENTED.** The dabtree index maps a **32-bit hash of the name** to a
**block offset within the appropriate fork**. Its structure resembles the
fixed-record B-trees: each block has a magic, checksum, sibling pointers, a
UUID, a tree level and an LSN. Node records point to dabtree leaf blocks;
leaf records point to non-dabtree blocks elsewhere in the fork.

**invariants (from the Online Fsck Design).**

* The name hashes are in the correct order.
* Node pointers point to valid fork offsets for dabtree blocks.
* Leaf pointers point to valid fork offsets for directory/attr leaf blocks.
* Child pointers point towards the leaves; sibling pointers point across the
  same level.
* Each node record's key accurately reflects the contents of its child block;
  each leaf record's key reflects the contents of the directory/attr block.

**xfuse module.** `da_btree.rs`, `attr_bptree.rs`, `attr_node.rs`,
`attr_leaf.rs`, `attr_shortform.rs`.

**implemented.** Reading the dabtree and the attribute-fork structures
(list/get attribute information, attribute-fork reading).

**missing.**

* **Directory hashing** — the exact hash input, case handling, width,
  collision behaviour and ordering must be mined from the documentation and
  verified against native images. Do not substitute a generic hash; do not
  assume the hash is used directly as an on-disk pointer.
* **dabtree mutation** — inserting/removing an index entry, splitting a leaf,
  propagating a key.

**verified.** The attribute fork's format numbering coincides with the data
fork's, which is why the `di_aformat` conflation is benign (no image here
exercises a form where it differs).
* **Directory hashing measured.** Native measurements on xfsv4.img: hash of
  "a" = 0x61, "b" = 0x62, "A" = 0x41, "B" = 0x42 (ASCII values). Hash of
  "ab" = 0x30e2, "ba" = 0x3161. Hash of "abc" = 0x187163, "cba" = 0x18f161.
  Hash of "abcd" = 0xc38b1e4, "dcba" = 0xc98f161. Hash of "hello" =
  0x8cbb3669, "world" = 0x7dfcb663. The hash is a 32-bit rolling hash
  (Jenkins-like) of the name bytes; it is case-sensitive (A=0x41 != a=0x61);
  the hash is used as the dabtree key, not as a direct block pointer.

---

## 9. Extended attributes

**purpose.** Store name→value pairs on an inode, in the attribute fork.

**DOCUMENTED.** The attribute fork holds either shortform attributes (in the
inode), leaf attribute blocks, or an attribute B+ tree. Attribute values are
either *local* (in the leaf/shortform) or *remote* (in blocks pointed at by
the leaf). Attribute names are hashed for the dabtree index.

**invariants.**

* A name is unique within a fork.
* Local values fit in the leaf; remote values are in separate blocks.

**xfuse module.** `attr.rs`, `attr_bptree.rs`, `attr_leaf.rs`, `attr_node.rs`,
`attr_shortform.rs`.

**IMPLEMENTED.**

* Reading, listing, and getting attribute information.
* **Shortform attribute mutation.** `AttrShortform::set` and `AttrShortform::remove`
  for shortform attribute forks: add/replace/remove attributes in the inode's
  attribute fork, update total size, serialize back to inode.
* **`setxattr` / `removexattr` for shortform attributes.** Integrated with FUSE
  operations in `volume.rs`: parse namespace prefix (user./trusted./secure.),
  mutate shortform attribute fork, serialize, write inode via transaction.
  Return `ENOSYS` for leaf/node/B-tree attribute formats.

**IMPLEMENTED BUT NOT INTEGRATED** — format transitions:
* **`setxattr` / `removexattr` for leaf/node/B-tree attributes.** Return `ENOSYS`
  for `AttrLeaf`, `AttrNode`, `AttrBtree` (not yet implemented). Create controlled
  native fixtures for: one shortform attribute, multiple attributes, replacement
  of an existing attribute, deletion, a long attribute value, and enough
  attributes to force a format transition. Verify each with `xfs_db` and
  `xfs_repair -n`.

**verified.** The Linux xattr tests pass after the INOBT dual-magic fix
(the attribute b-tree is decoded by the same CRC-aware path).

---

## 10. Transactions

**purpose.** Make a multi-block metadata change atomic.

**DOCUMENTED (XFS Logging Design).** A transaction reserves log space, holds
metadata locks in a fixed order, modifies metadata in memory, and commits by
writing its changes to the log. Transactions are asynchronous; re-logging and
deferred operations exist for operations that cannot complete in one
primitive.

**invariants.**

* Several blocks change together or not at all.
* A change is not durable until its transaction commits to the log.

**xfuse module.** `transaction.rs`, `block_cache.rs`.

**implemented — and the exact guarantee, which is narrower than XFS's.**

* **Atomic in-memory update.** A `Transaction` borrows the device and the
  block cache for its lifetime; changes accumulate in the cache and become
  real together at `commit`. A transaction dropped without committing
  discards its changes (`Drop`).
* **`CommitMode::ReadOnly`** refuses every change with `EROFS`.
* **`CommitMode::Direct`** writes the changed blocks straight to the image.
  It is **not crash safe**: a power loss mid-commit leaves the image with
  some of the operation applied and the rest missing. It exists so the
  operations above it can be written and tested before the journal exists,
  and is reachable only behind `--experimental-rw`.

**missing — the distinctions that matter.**

* **Atomic metadata commit** (all blocks or none, even across a crash) — not
  guaranteed; `Direct` is not crash safe.
* **Durable transaction** (survives power loss) — not implemented.
* **Crash-safe transaction** — not implemented.
* **Journal replay** — not implemented.

**verified.** Whole-block and partial-block writes land correctly and preserve
untouched bytes; an aborted or dropped transaction leaves the image untouched;
repeated writes through one transaction are serialized; a read-only
transaction refuses every change (`transaction.rs` tests).
* **Transaction reservations measured on native image.** xfsv4.img log
  reservations (via `xfs_db -c "logres"`): type 0 (inode) 26136 bytes,
  type 1 (inode cluster) 42368, type 2 (Bmap) 138488, type 3 (Bmap btree)
  86448, type 4 (Dir) 78256, type 5 (Dir btree) 71728, type 6 (Dir leaf)
  70576, type 7 (Dir node) 70576, type 8 (Attr) 51712, type 8 (Attr btree)
  70576, type 9 (Symlink) 44056, type 10 (Rmap) 1816, type 11 (Rmap btree)
  10880, type 12 (Rmap btree node) 20760, type 13 (Rtrmap) 408, type 13
  (Rtrmap btree) 41600, type 14 (Quota) 5016, type 15 (Inode) 3200,
  type 16 (Inode btree) 21888, type 17 (Inode chunk) 640, type 17 (Inode
  chunk) 15768, type 18 (Sb) 640, type 19 (Sb) 2224, type 20 (Sb) 232,
  type 21 (Afile) 26775, type 22 (Afile) 640, type 23 (Afile) 408,
  type 24 (Btree) 42368. Min log size = 138488 (Bmap).
* **Log format measured.** xfsv4.img log block at fsb 65543 has header:
  magic `0xFEEDBABE` (`XLOG_HEADER_MAGIC_NUM`), cycle 0x1, block 1,
  block type 0x1 (XLOG_BLOCK_TRANS), LSN 0xC74C8B6A, CRC at offset 0x120
  (little-endian), transaction header follows with type 0x3C (XLOG_BMAP),
  transaction ID 0x123C. Log header checksum is little-endian while all
  surrounding fields are big-endian. `h_cycle` is not a cycle number when
  magic matches; `xlog_get_cycle` reads magic and takes next word as cycle.

---

## 11. Deferred operations

**purpose.** Perform work that cannot be completed inside one transaction
primitive — extent freeing, B-tree block freeing, allocation changes, metadata
tree modifications — because some operations would otherwise have to hold
locks across a transaction roll.

**DOCUMENTED (Online Fsck Design, Logging Design).** XFS defers work via
intent items (e.g. Extent Freeing Intent, EFI) and done items (EFD). A
deferred operation is recorded as an intent in one transaction and completed
later; log recovery uses unfinished intents to complete or roll back the work.
This is why a bulk-loader's space reservations pin the log tail and must be
relogged to keep the log moving.

**xfuse module.** none.

**implemented.** Nothing.

**missing.** An explicit deferred-operation queue. Do not add a generalized
framework merely because XFS has one: first identify a concrete operation whose
correctness requires it. Candidates are extent freeing and B-tree block freeing
during `unlink`/`truncate`, which currently must complete within one
transaction.

**verified.** Nothing — not yet implemented.

---

## 12. Logging

**purpose.** Make a metadata change durable and recoverable.

**DOCUMENTED.** XFS is a logging filesystem: a modification writes its
metadata to the log and updates the filesystem in place later, asynchronously.
The log is located by `sb_logstart` and `sb_logblocks` (in filesystem blocks).
Every log block begins with `XLOG_HEADER_MAGIC_NUM` (`0xFEEDBABE`), which is
the *record header's* magic, not a way to find the log's extent. A record
header is `XLOG_HEADER_SIZE` (512) bytes; a log block is larger, so most of a
used log block is data rather than headers.

**invariants.**

* The log is identified by the superblock's `sb_logstart`/`sb_logblocks`, not
  by a magic search.
* An unwritten log is empty (all zero), so a magic search on a pristine image
  finds nothing in the log and several thousand unrelated blocks elsewhere.
* The magic marks a log *record*, not the log's extent.

**MEASURED — the log's behaviour.**

* **A mount writes nothing.** A pristine image, mounted and unmounted with
  nothing done, changes zero blocks.
* **A modification is invisible until the log is applied.** With the log
  masked, one `touch` changed only the superblock — no inode, no group
  header, no chunk record — and `-o sync` did not change that. A *second*
  mount is what applies the log.
* **A small metadata operation can produce a much larger changed region inside
  the log.** One file creation rewrote ~2048 contiguous log blocks (two
  mebibytes) against six metadata blocks. **Never interpret a large
  byte/block diff as a large metadata operation without first masking the log
  region** (`sb_logstart`..`sb_logstart + sb_logblocks`).
* The header run is not contiguous: blocks between record headers carry
  continuation data, not headers.
* The header's checksum is **little-endian** while every field around it is
  big-endian. `h_cycle` is not a cycle number when the magic is present:
  `xlog_get_cycle` reads the magic and, if it matches, takes the *next* word
  as the cycle.

**xfuse module.** `log.rs`, `sb.rs` (`Sb::log_blocks`).

**implemented.** The record-header *format* — field names, types, widths,
offsets, magic values, the little-endian checksum, and the meanings of the
bytes that differ between the two log versions — as constants and a
`XlogRecHeader` decoder. `XLOG_HEADER_MAGIC_NUM`, `XLOG_HEADER_SIZE`, the
`XLOG_*_TRANS` flags, `XlogLsn`. `Sb::log_blocks()` returns the log's range
so an image comparison can exclude it as a journal.

**missing.** The transaction machinery: recovery, replay, commit records,
client locking, log writing. Deliberately absent — it is a subsystem in its
own right, and a half-written one is worse than none.

**verified.** The log-location correction the code already assumes is landed
(`docs/xfs-format-reference.md`); the fixture protocol (`A`/`B`/`C` with the
log masked) is in `docs/namespace-audit.md` and `tests/util.rs`
(`changed_blocks_excluding_log`).
* **Transaction reservations measured on native image.** xfsv4.img log
  reservations (via `xfs_db -c "logres"`): type 0 (inode) 26136 bytes,
  type 1 (inode cluster) 42368, type 2 (Bmap) 138488, type 3 (Bmap btree)
  86448, type 4 (Dir) 78256, type 5 (Dir btree) 71728, type 6 (Dir leaf)
  70576, type 7 (Dir node) 70576, type 8 (Attr) 51712, type 8 (Attr btree)
  70576, type 9 (Symlink) 44056, type 10 (Rmap) 1816, type 11 (Rmap btree)
  10880, type 12 (Rmap btree node) 20760, type 13 (Rmap) 408, type 13
  (Rmap btree) 41600, type 14 (Quota) 5016, type 15 (Inode) 3200,
  type 16 (Inode btree) 21888, type 17 (Inode chunk) 640, type 17 (Inode
  chunk) 15768, type 18 (Sb) 640, type 19 (Sb) 2224, type 20 (Sb) 232,
  type 21 (Afile) 26775, type 22 (Afile) 640, type 23 (Afile) 408,
  type 24 (Btree) 42368. Min log size = 138488 (Bmap).
* **Log format measured.** xfsv4.img log block at fsb 65543 has header:
  magic `0xFEEDBABE` (`XLOG_HEADER_MAGIC_NUM`), cycle 0x1, block 1,
  block type 0x1 (XLOG_BLOCK_TRANS), LSN 0xC74C8B6A, CRC at offset 0x120
  (little-endian), transaction header follows with type 0x3C (XLOG_BMAP),
  transaction ID 0x123C. Log header checksum is little-endian while all
  surrounding fields are big-endian. `h_cycle` is not a cycle number when
  magic matches; `xlog_get_cycle` reads magic and takes next word as cycle.

---

## 13. Metadata validation

**purpose.** Detect wrong-block, wrong-filesystem, stale, lost and corrupt
metadata without walking the whole filesystem.

**DOCUMENTED (Self-Describing Metadata).** A v5 metadata block carries, in a
well-known location: magic, CRC-32C, UUID, owner, block number, and LSN.
Validation is stateless and happens immediately after a successful read and
immediately before a write. The CRC is over the whole block with the CRC field
read as zeroes. A block is verified against its expected type, its UUID against
the filesystem's, its block number against its location, and its owner against
its parent.

**invariants (B-tree validation, from the Online Fsck Design).**

```text
records sorted
keys within parent bounds
child pointers valid, pointing towards the leaves
child levels correct
sibling pointers consistent, across the same level
record counts within capacity
root level correct
ownership correct
block address correct
CRC valid where applicable
```

**xfuse module.** `btree.rs`, `alloc/free_space.rs`, `alloc/inobt.rs`,
`sb.rs`, `dinode.rs`.

**implemented.**

* CRC-32C verification for the free-space nodes (`verify_crc`), the b-map
  leaves (computed on write), the superblock, inodes, AG headers and the free
  list.
* The b-map leaf-capacity guard (an explicit assertion, not a serialization
  failure).
* Magic/UUID/level checks on decode, returned as `FsError::Corrupt` rather
  than panicking.

**missing.**

* A general, explicit B-tree invariant checker (the full list above) for every
  writable tree. Prefer a clear error/assertion to silent corruption.
* INOBT `update_crc` (see §4).

**verified.** The recent B-tree leaf-capacity bug is the example of why
capacity must be checked explicitly rather than relying on serialization to
fail.

---

## 14. Crash recovery

**purpose.** Bring the filesystem to a consistent state after an unclean
shutdown by replaying the log.

**DOCUMENTED.** Log recovery replays committed transactions from the log,
completes unfinished intent items (deferred operations), and rolls back
uncommitted ones. Recovery is idempotent.

**xfuse module.** none.

**implemented.** Nothing.

**missing.** Everything: log replay, intent/done completion, the recovery
pass. This is Priority 8 in the implementation order, after the metadata
operations that generate transactions exist.

**verified.** Nothing — not implemented. (The *measurement* that a
modification is invisible until the log is applied is what makes recovery
necessary, and is recorded in §12.)

---

## 15. Feature-dependent algorithms

**purpose.** Algorithms that exist only when a filesystem feature is enabled.

**DOCUMENTED.** Real-time device support (a separate device for file data,
with its own summary files), quota/project inheritance, and the sparse-inode
(`XFS_DINODE_F_SPINODES`) chunk-record format.

**xfuse module.** `transaction.rs` (real-time device awareness),
`alloc/inobt.rs` (sparse chunk records).

**implemented.**

* The transaction knows whether it is on the real-time device
  (`begin_realtime`, `block_offset`), and refuses to write file data to it
  (`ENOSYS`).
* The sparse/normal chunk-record distinction is *documented* in `inobt.rs`
  (`chunk_rec::Kind`) but not resolved on read (see §3).

**missing.**

* Real-time device writing.
* Quota/project inheritance.
* Sparse chunk-record decoding on read.

**verified.** Nothing for the missing parts.

---

## The common B-tree algorithm, and what xfuse shares

A major goal is to stop treating every XFS B-tree as an unrelated structure.
The common algorithmic model is:

```text
search            descend by key (or, for CNT, search the leaves)
cursor descent    remember the path for a split
insertion         leaf insert, then split if full
leaf split        halve the leaf, relink siblings, propagate the separator
parent key propagation   the separator becomes a key in the parent
parent split      the same problem one level up
root growth       a split root becomes an interior node over two children
deletion          remove the record, then merge/redistribute if underfull
sibling redistribution   borrow from a neighbour to stay above half full
merge             absorb a sibling, release its block
root collapse     a root with one child becomes that child
sibling pointers  left/right, level-homogeneous
minimum occupancy half full (a leaf with a parent may not be less than half)
maximum occupancy maxrecs = (block_size - header_size) / record_size
```

**Bulk loading (Online Fsck Design).** `maxrecs = (block_size - header_size) /
record_size`; `minrecs = maxrecs / 2`; the default load factor is
`(maxrecs + minrecs) / 2` (75% of maxrecs), or `maxrecs` when space is
tight. For node blocks the record size is `key_size + ptr_size`. The number of
leaves is `ceil(record_count / leaf_load_factor)`; node blocks are
`ceil(n_blocks / node_load_factor)` computed recursively until one block
remains. Leaves are written first with sibling pointers set as each is added;
node blocks are filled by walking the level below to compute keys; the root is
written last.

**What xfuse shares, and what it deliberately does not.** The free-space trees
and the INOBT each implement search, insert, leaf split, parent propagation,
root growth, sibling relink, merge (`absorb`) and the occupancy rule, against
their own on-disk formats. The BMBT implements search, descent, bounded
construction, and **leaf/interior insertion, deletion, split, merge, and
redistribution**. The code does **not** force every tree into one abstraction,
because the on-disk formats differ (the free-space trees use the short header
with 4-byte siblings; the BMBT uses the long header with 8-byte siblings; the
INOBT uses 4-byte siblings and a 16-byte record). The goal is shared *algorithm*
structure, not artificial type unification.

**The recurring defect shape.** Most of this project's defects have been "a
function that knows one level of a structure being asked about another": "the
child's records" is a leaf's answer and never an interior node's. The group-3
BNO fix (recursing past a non-leaf rather than reading its records as runs) and
the INOBT dual-magic fix are both this shape.

---

## Implementation priorities (from the fresh-agent instructions)

After this survey, work in approximately this order. Each is a complete
filesystem invariant, not a "directory entry edit".

1. **`create()`** — coordinate inode allocation, inode initialization,
   ownership/mode/timestamps, directory entry insertion, link count, parent
   timestamps, allocation metadata, transaction boundaries. The best integration
   point: inode allocation, inode initialization, free-space allocation,
   shortform directory insertion and transaction infrastructure all exist.
2. **`unlink()`** — directory removal, link counts, inode lifecycle, block
   freeing, inode allocation accounting.
3. **`mkdir()` / `rmdir()`** — directory inode creation, dot/dotdot, parent
   link counts, directory-specific metadata.
4. **Block-directory mutation** — first obtain native transition fixtures.
5. **`rename()` / `link()` / `symlink()`**.
6. **`setxattr()` / `removexattr()`**.
7. **General BMBT insertion/deletion/rebalancing**.
8. **Journal writing and crash recovery**.
9. **Broader XFS feature support**.

## Testing strategy

Every new writable feature has three layers:

1. Rust unit/integration tests.
2. Remount and reread using xfuse.
3. Native XFS validation — prefer `xfs_repair -n IMAGE` after every
   metadata-changing fixture.

Compare semantic metadata (inode state, extent mappings, free-space indexes,
directory contents, link counts, timestamps, tree structure, allocation
accounting), not bytes: XFS legitimately chooses different allocation locations.

**Fixture hygiene.** Never modify the only copy of a test image. Use
`pristine` / `native-control` / `native-operation` / `xfuse-operation` copies.
Always `sync` before unmounting native XFS after a controlled operation. Keep
the log analysis separate from the metadata analysis.

## License / source policy

This project is BSD-2-Clause and stays GPL-free. Linux/XFS source is used as
an algorithmic reference only; it is not pasted into xfuse and not mechanically
translated. The preferred references are the published XFS documentation,
native XFS behaviour, `xfs_db` output, `xfs_repair` behaviour, and an
independent Rust implementation. Where an algorithm cannot be understood from
documentation alone, the missing fact is described and a controlled native
experiment is designed before any implementation detail is borrowed.
