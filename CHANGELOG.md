# Change Log

All notable changes to this project will be documented in this file.
This project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased] - ReleaseDate

### Added

- Improved performance ny caching various metadata.
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

