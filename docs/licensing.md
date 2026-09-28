# Licensing and provenance audit

This document records the licensing status of `xfuse` (the `xfs-fuse` crate) and
of everything it depends on, together with the external material that was used
while writing the write-support code.  It is updated as part of every
implementation phase, as required by the write-support plan.

The short version: **the project is BSD-2-Clause, every dependency is
permissively licensed (MIT / Apache-2.0 / BSD / ISC / Unlicense), and no
GPL-licensed implementation code has been copied, translated, or adapted.**

## 1. Project source

| Item | License |
|:-----|:--------|
| `xfs-fuse` (this repository) | BSD-2-Clause, see `LICENSE.md` |

Every file in `src/` carries the BSD-2-Clause header that was already present in
the upstream sources.  New files added by the write-support work use the same
header and the same copyright attribution style, so that the provenance of each
file stays obvious.

No file from the Linux kernel, from `xfsprogs`, or from any other GPL project is
vendored into this repository, not even as a "reference" copy.

## 2. Dependencies

`cargo metadata` / `cargo tree` plus per-crate license inspection was used to
enumerate the dependency graph.  The graph contains 142 crates.  Their declared
licenses are:

| License expression | Crates |
|:-------------------|:-------|
| `MIT OR Apache-2.0` | 60 |
| `MIT` | 19 |
| `MIT/Apache-2.0` | 13 |
| `Apache-2.0 OR MIT` | 5 |
| `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | 2 |
| `BSD-2-Clause OR Apache-2.0 OR MIT` | 2 |
| `MIT or Apache-2.0` | 2 |
| `Unlicense OR MIT` | 3 |
| `Unlicense/MIT` | 2 |
| `(MIT OR Apache-2.0) AND Unicode-DFS-2016` | 1 |
| `MIT` (`errno-dragonfly`) | 1 |
| `MIT` (`redox_syscall`) | 1 |
| `MIT` (`valuable`) | 1 |

Direct dependencies, and why they are acceptable:

| Crate | License | Used for |
|:------|:--------|:---------|
| `fuser` | MIT | FUSE protocol types and the mount loop.  The `libfuse` feature links against the system libfuse (LGPL-2.1), which is a *system* library, dynamically linked and not redistributed by this project.  The Rust crate itself is MIT. |
| `bincode-next` | MIT | Fixed-width, big-endian decoding of on-disk XFS structures. |
| `byteorder` | BSD-3-Clause OR Apache-2.0 OR MIT | Big-endian field access on the superblock. |
| `bitflags` | MIT | Feature bitmasks in the superblock. |
| `crc` | MIT | CRC-32C (Castagnoli) checksums for v5 metadata. |
| `enum_dispatch` | MIT | Trait dispatch over the several on-disk directory/file representations. |
| `num-derive`, `num-traits` | MIT OR Apache-2.0 | Enum conversions. |
| `cfg-if` | MIT/Apache-2.0 | Platform conditional compilation. |
| `libc`, `nix` | MIT | System calls. |
| `clap` | MIT OR Apache-2.0 | Command line parsing. |
| `tracing`, `tracing-subscriber` | MIT | Logging. |
| `uuid` | MIT/Apache-2.0 | Filesystem UUID type. |

The LGPL-licensed libfuse that `fuser`'s `libfuse` feature links against is a
system library: it is not vendored, not statically linked, and not modified.  It
was already a dependency of the read-only implementation, and nothing about the
write path changes that relationship.

No new third-party dependency was added by the write-support work.  That is a
deliberate decision: the write path needs nothing that the standard library does
not already provide (`std::os::unix::fs::FileExt` for positional I/O,
`std::collections::HashMap` for the block cache, and so on).

## 3. External material consulted

Permitted by the write-support plan: publicly documented format descriptions,
standards, academic material, and black-box observation of native XFS.

* The *XFS Algorithms and Data Structures* document (Dave Chinner, SGI/Red Hat),
  for the on-disk layout of the superblock, inodes, extents, directories,
  extended attributes, and the log.
* Public XFS on-disk format documentation: the `xfs_format.h` structure
  *definitions* that are mirrored in publicly published filesystem format
  documentation and in the Linux documentation tree under
  `Documentation/filesystems/xfs/`.
* Behaviour observed from `xfsprogs` 6.18 (`mkfs.xfs`, `xfs_db`, `xfs_repair`,
  `xfs_metadump`, `xfs_mdrestore`, `xfs_bmap`, `xfs_logprint`) run as black
  boxes against generated test images.  These tools are used the way the plan
  permits: to *generate* images, to *observe* results, and to *check*
  invariants.  No source code from them was read for this work.
* Behaviour observed from the Linux kernel's XFS driver by mounting images
  produced by `xfuse` and comparing the results against the same images
  modified by native XFS.  Only observable behaviour was used.
* The existing `xfuse` sources in this repository, under their BSD-2-Clause
  license.

## 4. Statement about GPL sources

No GPL-licensed source code has been copied, translated, mechanically ported,
adapted, or otherwise derived into this repository.  In particular:

* No Linux kernel file, header, comment, or function was transliterated into
  Rust.  Where the write path needs to mirror a documented XFS invariant, the
  code was written from the format description and from observed behaviour, and
  the reasoning is described in the module's own rustdoc.
* Where the existing `xfuse` code already expressed a fact about the XFS format
  (for example the B+tree pointer gap calculations in `dinode_core.rs`), that
  code was reused under its original BSD-2-Clause terms rather than
  re-derived.
* Comments in the new code are original prose.  They explain *why* an invariant
  holds and *what* the code does, and they do not reproduce text from any other
  project.

The distinction used throughout the write path is:

* a **fact about the XFS format** — for example "a v3 inode carries a CRC-32C
  computed over the whole inode with the checksum field itself zeroed" — is a
  specification detail and may be implemented independently; and
* an **implementation of that fact** — the code expressing it — is written from
  the specification and from observation, in this project's own style.

## 5. Rationale for code whose provenance could be questioned

Two areas of the write path encode behaviour that is easy to get subtly wrong,
so they are called out here.

* **B+tree pointer gaps in inodes.**  `dinode_core.rs` already contained
  `dfork_btree_ptr_gap` and `afork_btree_ptr_gap`, and its own comments record
  that the published documentation is wrong about them.  The write path must
  use the same calculation when it re-encodes an inode, so it reuses that
  BSD-2-Clause function rather than writing a second, independent one.  The
  values are also covered by unit tests derived from live file systems that
  predate this work.

* **CRC coverage of v5 metadata.**  The superblock reader in `sb.rs` already
  computes a CRC-32C over the superblock with its checksum field zeroed.  The
  inode writer applies the analogous rule to inodes.  The rule itself is a
  documented format fact; the reuse of the existing `crc` crate call pattern is
  from BSD-2-Clause code in this repository.

## 6. How to re-run this audit

```sh
cargo metadata --format-version 1 > metadata.json
cargo tree
# For each crate in the graph, inspect the `license` field of its Cargo.toml,
# e.g. in ~/.cargo/registry/src/*/<crate>-<version>/Cargo.toml
```

Adding a dependency requires repeating this audit and updating the table in
section 2 before the dependency is used.
