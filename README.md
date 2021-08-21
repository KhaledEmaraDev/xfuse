# An XFS FUSE server

[![Cirrus Build Status](https://api.cirrus-ci.com/github/KhaledEmaraDev/xfuse.svg)](https://cirrus-ci.com/github/KhaledEmaraDev/xfuse)
[![crates.io](https://meritbadge.herokuapp.com/xfuse)](https://crates.io/crates/xfuse)

A read-only XFS implementation using FUSE, written for GSoC 2021.


## Golden Image

I haven't yet standardized a golden image, because I'm making it as I go. Currently, I have five
directories each with a different number of entries and as such each is of a different type.

### Filesystem configuration

The filesystem has the following properties:

* 4KB block size
* 16KB directory size

To create such a filesystem use the following sh command:

```
mkfs.xfs -n size=16384 -f <device>
```

### Directory tree

The directory tree is as follows:

| Name  | Number of entries |
|:-----:|------------------:|
| sf    |                 4 |
| block |                 8 |
| leaf  |               256 |
| node  |              2048 |
| btree |            204800 |

### Entry naming convention

Entries are named as follows:

```
frame<zero padded and right aligned 6-digit number starting from 0>.tst
```

### Entries

All entries are directories, because they are the only supported format as of now.
It's tedious to create such a large number of entries manually, you can use the following command:

```
for i in $(seq -f "%06g" 0 <number of entries - 1>)
do
    mkdir "frame$i"
done
```

## License

bsd-2-clause
