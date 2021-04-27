#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "xfuse_end.h"
#include "xfuse_ino.h"

int xfuse_ino_get_from_disk(xfuse_ino *ino);

void xfs_inode_get_access_time(xfuse_dinode_core *ino, struct timespec *stamp) {
  stamp->tv_sec = ino->di_atime.t_sec;
  stamp->tv_nsec = ino->di_atime.t_nsec;
}

void xfs_inode_get_change_time(xfuse_dinode_core *ino, struct timespec *stamp) {
  stamp->tv_sec = ino->di_ctime.t_sec;
  stamp->tv_nsec = ino->di_ctime.t_nsec;
}

void xfs_inode_get_modification_time(xfuse_dinode_core *ino,
                                     struct timespec *stamp) {
  stamp->tv_sec = ino->di_mtime.t_sec;
  stamp->tv_nsec = ino->di_mtime.t_nsec;
}

void xfs_inode_swap_ends(xfuse_dinode_core *ino) {
  ino->di_magic = be16_to_host(ino->di_magic);
  ino->di_mode = be16_to_host(ino->di_mode);
  ino->di_onlink = be16_to_host(ino->di_onlink);
  ino->di_uid = be32_to_host(ino->di_uid);
  ino->di_gid = be32_to_host(ino->di_gid);
  ino->di_nlink = be32_to_host(ino->di_nlink);
  ino->di_projid = be16_to_host(ino->di_projid);
  ino->di_flushiter = be16_to_host(ino->di_flushiter);
  ino->di_atime.t_sec = be32_to_host(ino->di_atime.t_sec);
  ino->di_atime.t_nsec = be32_to_host(ino->di_atime.t_nsec);
  ino->di_mtime.t_sec = be32_to_host(ino->di_mtime.t_sec);
  ino->di_mtime.t_nsec = be32_to_host(ino->di_mtime.t_nsec);
  ino->di_ctime.t_sec = be32_to_host(ino->di_ctime.t_sec);
  ino->di_ctime.t_nsec = be32_to_host(ino->di_ctime.t_nsec);
  ino->di_size = be64_to_host(ino->di_size);
  ino->di_nblocks = be64_to_host(ino->di_nblocks);
  ino->di_extsize = be32_to_host(ino->di_extsize);
  ino->di_nextents = be32_to_host(ino->di_nextents);
  ino->di_anextents = be16_to_host(ino->di_anextents);
  ino->di_dmevmask = be32_to_host(ino->di_dmevmask);
  ino->di_dmstate = be16_to_host(ino->di_dmstate);
  ino->di_flags = be16_to_host(ino->di_flags);
  ino->di_gen = be32_to_host(ino->di_gen);
  ino->di_next_unlinked = be32_to_host(ino->di_next_unlinked);
}

int xfuse_ino_get_from_disk(xfuse_ino *ino) {
  xfs_agnumber_t ag_no =
      (xfs_agnumber_t)ino->id >> (xfuse_sb_get_ag_ino_bits(&ino->vol->sb));

  if (ag_no > ino->vol->sb.sb_agcount) {
    errno = ENOENT;
    fprintf(stderr, "xfuse_ino_get_from_disk -> Inode number is invalid: %d",
            errno);
    return -1;
  }

  uint32_t ag_rel_ino_no =
      (uint32_t)ino->id & INO_MASK(xfuse_sb_get_ag_ino_bits(&ino->vol->sb));

  xfs_agblock_t ag_blk = (ag_rel_ino_no >> (ino->vol->sb.sb_inopblog)) &
                         (INO_MASK(ino->vol->sb.sb_agblklog));

  xfs_off_t off = (ino->id & INO_MASK(ino->vol->sb.sb_inopblog));

  uint32_t len = ino->vol->sb.sb_inodesize;

  xfs_agblock_t ag_num_blks = ino->vol->sb.sb_agblocks;

  xfs_fsblock_t num_blks_to_read =
      ((uint64_t)(ag_no * ag_num_blks) + ag_blk)
      << (ino->vol->sb.sb_blocklog - BASICBLOCKLOG);

  xfs_daddr_t pos = num_blks_to_read * BASICBLOCKSIZE + off * len;

  if (pread(ino->vol->dev, ino->buf, len, pos) != len) {
    fprintf(stderr,
            "xfuse_ino_get_from_disk -> Cannot read block from disk: %d",
            errno);
    return -1;
  }

  memcpy(ino->node, ino->buf, sizeof(xfuse_dinode_core));
  xfs_inode_swap_ends(ino->node);

  return 0;
}

int xfuse_ino_construct(xfuse_ino *ino, xfuse_volume *vol, xfs_ino_t id) {
  ino->vol = vol;
  ino->id = id;

  if ((ino->node = malloc(sizeof(xfuse_dinode_core))) == NULL) {
    fprintf(stderr, "xfuse_ino_construct -> Out of memory: %d\n", errno);
    return -1;
  }

  uint16_t inode_size = ino->vol->sb.sb_inodesize;
  if ((ino->buf = calloc(inode_size, sizeof(char))) == NULL) {
    fprintf(stderr, "xfuse_ino_construct -> Out of memory: %d\n", errno);
    return -1;
  }

  if (xfuse_ino_get_from_disk(ino) == -1) {
    fprintf(stderr, "xfuse_ino_construct -> Cannot read inode from disk: %d",
            errno);
    return -1;
  } else {
    if (ino->node->di_magic == XFS_DINODE_MAGIC) {
      return 0;
    } else {
      errno = EINVAL;
      fprintf(stderr, "xfuse_ino_construct -> Inode is not a valid XFS one: %d",
              errno);
      return -1;
    }
  }
}

void xfuse_ino_destruct(xfuse_ino *ino) {
  free(ino->buf);
  free(ino->node);
}
