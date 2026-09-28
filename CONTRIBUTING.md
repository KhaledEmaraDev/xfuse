# CONTRIBUTING

## How to run the project

1. Check for errors
```
cargo check
```

   And the lints, on the toolchain CI uses.  CI runs clippy and rustfmt on
   nightly, and nightly has lints that stable does not have, so a change that
   stable accepts can still fail the build:
```
cargo +nightly clippy --all-targets -- -D warnings
cargo +nightly fmt -- --check
```

2. Build the project
```
cargo build
```

3. Run the program
```
cargo run <device> <mountpoint>
```

4. Debug crashes
```
RUST_BACKTRACE=1 cargo run <device> <mountpoint>
```

5. Save large logs to a file
```
RUST_BACKTRACE=1 cargo run <device> <mountpoint> > run.log
```

### Source Code Structure

All files are relative to `src/libxfuse/`.

| File  | Description       |
|:-----:|:------------------|
| definitions       | Contains constants for magic numbers and various type definitions |
| volume            | Contains the main struct that communicates with the FUSE kernel module, and the only place that FUSE operations are implemented |
| sb                | Contains the Super Block structure and some helper methods |
| dinode_core       | Contains the Core Inode structure |
| dinode            | Contains helper methods for the Inode to return a file, dir, attr, or symlink `impl` |
| bmbt_rec          | Contains extent records |
| da_btree          | Contains the variable length B+Tree structure used with directories and attributes |
| btree             | Contains the fixed length B+Tree structure used for block navigation |
| dir3              | Contains a trait for common directory operations and some common structures |
| dir3_sf           | Contains a structure for Short Form directories |
| dir3_block        | Contains a structure for Extents-based Block directories |
| dir3_leaf         | Contains a structure for Extents-based Leaf directories |
| dir3_node         | Contains a structure for Extents-based Node directories |
| dir3_bptree       | Contains a structure for B+Tree-based directories |
| extent            | Contains `ExtentMap`, the one place that answers where a file's logical block lives |
| block_device      | Contains the only handle onto the image; everything that reads or writes it goes through here |
| block_reader      | Contains the read side's seekable window onto the image |
| block_cache       | Contains the cache of file system blocks that modified blocks live in |
| transaction       | Contains the object through which the file system changes the image |
| inode             | Contains `RawDinode`, the serialized form of an inode, and the in-memory state kept alongside it |
| capabilities      | Contains what this implementation supports for a given image, and what it must refuse |
| error             | Contains the error type the write path uses instead of panicking |
| symlink_extent    | Contains a structure for Extents-based symlinks |
| attr              | Contains a trait for common trait operations and some common structures |
| attr_shortform    | Contains a structure for Short Form attributes |
| attr_leaf         | Contains a structure for Extents-based Leaf attributes |
| attr_node         | Contains a structure for Extents-based Node attributes |
| attr_bptree       | Contains a structure for B+Tree-based attributes |
| utils             | Contains common helper functions |

### Writing

`docs/write-support-progress.md` records what the write path does today, and
`docs/licensing.md` records where the code came from.  Two rules matter when
adding to it:

* Nothing above `transaction.rs` writes to the image.  A change goes through a
  `Transaction`, and a metadata change is committed with the data change it
  belongs to.
* The XFS format is implemented from its documentation and from the behaviour of
  native XFS.  No GPL implementation code -- the Linux kernel's, or xfsprogs' --
  is copied, translated, or adapted.  See `docs/licensing.md`.
