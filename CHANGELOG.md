# Change Log

All notable changes to this project will be documented in this file.
This project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased] - ReleaseDate

### Fixed

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

