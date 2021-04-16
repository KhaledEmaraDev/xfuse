#ifndef _XFUSE_TYPES_H
#define _XFUSE_TYPES_H

#include <stdbool.h>
#include <stdint.h>
#include <uuid/uuid.h>

#define XFS_SB_MAGIC 0x58465342

typedef uint64_t xfs_ino_t;      // absolute inode number
typedef int64_t xfs_off_t;       // file offset
typedef int64_t xfs_daddr_t;     // disk address (sectors)
typedef uint32_t xfs_agnumber_t; // AG number
typedef uint32_t xfs_agblock_t;  // AG relative block number
typedef uint32_t xfs_extlen_t;   // extent length in blocks
typedef int32_t xfs_extnum_t;    // number of extends in a data fork
typedef int16_t xfs_aextnum_t;   // number of extents in an attribute fork
typedef uint32_t
    xfs_dablk_t; // block number for directories and extended attributes
typedef uint32_t
    xfs_dahash_t; // hash of a directory file name or extended attribute name
typedef uint64_t xfs_fsblock_t;  // filesystem block number combining AG number
typedef uint64_t xfs_rfsblock_t; // raw filesystem block number
typedef uint64_t xfs_rtblock_t;  // extent number in the real-time sub-volume
typedef uint64_t xfs_fileoff_t;  // block offset into a file
typedef uint64_t xfs_filblks_t;  // block count for a file
typedef int64_t xfs_fsize_t;     // byte size of a file

#endif /* defined _XFUSE_TYPES_H */
