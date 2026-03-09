# An XFS FUSE server

[![Cirrus Build Status](https://api.cirrus-ci.com/github/KhaledEmaraDev/xfuse.svg)](https://cirrus-ci.com/github/KhaledEmaraDev/xfuse)
[![Crates.io](https://img.shields.io/crates/v/xfs-fuse.svg)](https://crates.io/crates/xfs-fuse)


A read-only XFS implementation using FUSE, written for GSoC 2021.

## Building from source
```sh
$ git clone https://github.com/KhaledEmaraDev/xfuse
$ cd xfuse
$ cargo build
```

## Example Usage
```sh
$ xfs-fuse /dev/sdb1 /mnt
```

## Golden Image

Some canned golden images are checked into the repository, in the `resources`
directory.  They contain a variety of file and directory types.  Run
`scripts/mkimg.sh` to rebuild them.

## License

BSD-2-clause
