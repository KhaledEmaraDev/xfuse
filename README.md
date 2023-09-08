# An XFS FUSE server

[![Cirrus Build Status](https://api.cirrus-ci.com/github/KhaledEmaraDev/xfuse.svg)](https://cirrus-ci.com/github/KhaledEmaraDev/xfuse)
[![crates.io](https://meritbadge.herokuapp.com/xfuse)](https://crates.io/crates/xfuse)

A read-only XFS implementation using FUSE, written for GSoC 2021.


## Golden Image

A canned golden image is checked into the repository, at
`resources/xfs.img.zstd`.  It contains five subdirectories each using a
different on-disk implementation, with a different number of empty files inside
of each.  Run `scripts/mkimg.sh` to rebuild it.

## License

BSD-2-clause
