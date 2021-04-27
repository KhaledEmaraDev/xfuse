#ifndef _XFUSE_INO_H
#define _XFUSE_INO_H

#include "xfuse_def.h"
#include "xfuse_vol.h"

#define MASK(n) ((1UL << n) - 1)
#define INO_MASK(x) ((1ULL << (x)) - 1)

typedef struct {
  int32_t t_sec;
  int32_t t_nsec;
} xfuse_timestamp_t;

typedef enum {
  XFUSE_DINODE_FMT_DEV,
  XFUSE_DINODE_FMT_LOCAL,
  XFUSE_DINODE_FMT_EXTENTS,
  XFUSE_DINODE_FMT_BTREE,
  XFUSE_DINODE_FMT_UUID,
  XFUSE_DINODE_FMT_RMAP,
} xfuse_dinode_fmt_t;

typedef struct {
  uint16_t di_magic;
  uint16_t di_mode;
  int8_t di_version;
  int8_t di_format;
  uint16_t di_onlink;
  uint32_t di_uid;
  uint32_t di_gid;
  uint32_t di_nlink;
  uint16_t di_projid;
  uint16_t di_projid_hi;
  uint8_t di_pad[6];
  uint16_t di_flushiter;
  xfuse_timestamp_t di_atime;
  xfuse_timestamp_t di_mtime;
  xfuse_timestamp_t di_ctime;
  xfs_fsize_t di_size;
  xfs_rfsblock_t di_nblocks;
  xfs_extlen_t di_extsize;
  xfs_extnum_t di_nextents;
  xfs_aextnum_t di_anextents;
  uint8_t di_forkoff;
  int8_t di_aformat;
  uint32_t di_dmevmask;
  uint16_t di_dmstate;
  uint16_t di_flags;
  uint32_t di_gen;
  uint32_t di_next_unlinked;
} __attribute__((packed)) xfuse_dinode_core;

extern void xfs_inode_get_access_time(xfuse_dinode_core *ino,
                                      struct timespec *stamp);
extern void xfs_inode_get_change_time(xfuse_dinode_core *ino,
                                      struct timespec *stamp);
extern void xfs_inode_get_modification_time(xfuse_dinode_core *ino,
                                            struct timespec *stamp);
extern void xfs_inode_swap_ends(xfuse_dinode_core *ino);

typedef struct {
  xfs_ino_t id;
  xfuse_volume *vol;
  xfuse_dinode_core *node;
  char *buf;
} xfuse_ino;

extern int xfuse_ino_construct(xfuse_ino *ino, xfuse_volume *vol, xfs_ino_t id);
extern void xfuse_ino_destruct(xfuse_ino *ino);

#endif /* defined _XFUSE_INO_H */
