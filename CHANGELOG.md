# Change Log

All notable changes to this project will be documented in this file.
This project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased] - ReleaseDate

### Fixed

- Fix readdir in directories containing holes.
  ([#154](https://github.com/KhaledEmaraDev/xfuse/issues/154))

- Fix readdir in a BTree directory whose Leaf is stored in a single directory
  block.  That can happen if most of the directory's contents have been
  removed.
  ([#150](https://github.com/KhaledEmaraDev/xfuse/issues/150))

## [0.4.1] - 2024-06-19

### Fixed

- Better error messages when attempting to mount a file system with unsupported features.
  ([#147](https://github.com/KhaledEmaraDev/xfuse/issues/147))

## [0.4.0] - 2024-05-30

### Added

- Added support for reading XFS version 4 file systems.
  ([#145](https://github.com/KhaledEmaraDev/xfuse/issues/145))

### Fixed

- Fixed read and lseek with files with preallocated extents.  `posix_fallocate`
  will create such extents in order to reserve disk space for a file, but as
  they aren't yet written, they should be treated as holes.
  ([#143](https://github.com/KhaledEmaraDev/xfuse/issues/143))

- Fixed a crash when trying to read extended attributes from files that once
  had enough extended attributes to require BTrees for their attribute forks,
  but then shrunk enough that the remaining extended attributes could fit
  within a single disk block.
  ([#140](https://github.com/KhaledEmaraDev/xfuse/issues/140))

## [0.3.0] - 2024-04-24

### Added

- Improved performance by caching various metadata.
  ([#107](https://github.com/KhaledEmaraDev/xfuse/issues/107))

- Added support for `FUSE_LSEEK` to efficiently copy sparse files.
  ([#133](https://github.com/KhaledEmaraDev/xfuse/pull/133))

### Changed

- The MSRV is now 1.74.0
  ([#128](https://github.com/KhaledEmaraDev/xfuse/pull/128))

- When run as root, the `default_permissions` and `allow_other` mount options
  are always set.
  ([#131](https://github.com/KhaledEmaraDev/xfuse/pull/131))

### Fixed

- Fixed a crash when reading a hole from a Btree-formatted file that is just
  past a data extent.
  ([#133](https://github.com/KhaledEmaraDev/xfuse/pull/133))

- Fixed a crash when opening and closing multiple files simultaneously but not
  in LIFO order.
  ([#116](https://github.com/KhaledEmaraDev/xfuse/pull/116))

- Eliminated sector-size-unaligned reads when using `O_DIRECT`.
  ([#112](https://github.com/KhaledEmaraDev/xfuse/pull/112))

- Eliminated sector-size-unaligned reads from regular files greater than 8 kB in
  size.
  ([#110](https://github.com/KhaledEmaraDev/xfuse/pull/110))

## [0.2.0] - 2024-03-07

Very many bug fixes.  This is the first beta-quality release.

## [0.1.1] - 2023-09-28

Several bugfixes related to readdir , and allow mounting devices that must be sector-sized aligned.

## [0.1.0] - 2023-08-26

Initial release

