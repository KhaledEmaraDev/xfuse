# CONTRIBUTING

## How to run the project

1. Check for errors
```
cargo check
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
| volume            | Contains the main struct that communicates with the FUSE kernel module |
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
| file              | Contains a trait for common file operations and some common structures |
| file_extent_list  | Contains a structure for Extents-based files |
| file_btree        | Contains a structure for B+Tree-based files |
| symlink_extent    | Contains a structure for Extents-based symlinks |
| attr              | Contains a trait for common trait operations and some common structures |
| attr_shortform    | Contains a structure for Short Form attributes |
| attr_leaf         | Contains a structure for Extents-based Leaf attributes |
| attr_node         | Contains a structure for Extents-based Node attributes |
| attr_bptree       | Contains a structure for B+Tree-based attributes |
| utils             | Contains common helper functions |
