#include <errno.h>
#include <stdio.h>

#include "xfuse_end.h"
#include "xfuse_sb.h"

int xfuse_sb_is_valid(xfuse_sb *sb) {
  if (sb->sb_magicnum != XFS_SB_MAGIC) {
    errno = EINVAL;
    fprintf(stderr,
            "xfuse_sb_is_valid -> Superblock magic number is invalid: %d\n",
            errno);
    return -1;
  }

  return 0;
}

bool xfuse_sb_has_file_type_field(xfuse_sb *sb) {
  return sb->sb_features2 & XFS_SB_VERSION2_FTYPE;
}

uint8_t xfuse_sb_get_ag_ino_bits(xfuse_sb *sb) {
  return sb->sb_agblklog + sb->sb_inopblog;
}

void xfuse_sb_swap_ends(xfuse_sb *sb) {
  sb->sb_magicnum = be32_to_host(sb->sb_magicnum);
  sb->sb_blocksize = be32_to_host(sb->sb_blocksize);
  sb->sb_dblocks = be64_to_host(sb->sb_dblocks);
  sb->sb_rblocks = be64_to_host(sb->sb_rblocks);
  sb->sb_rextents = be64_to_host(sb->sb_rextents);
  sb->sb_logstart = be64_to_host(sb->sb_logstart);
  sb->sb_rootino = be64_to_host(sb->sb_rootino);
  sb->sb_rbmino = be64_to_host(sb->sb_rbmino);
  sb->sb_rsumino = be64_to_host(sb->sb_rsumino);
  sb->sb_rextsize = be32_to_host(sb->sb_rextsize);
  sb->sb_agblocks = be32_to_host(sb->sb_agblocks);
  sb->sb_agcount = be32_to_host(sb->sb_agcount);
  sb->sb_rbmblocks = be32_to_host(sb->sb_rbmblocks);
  sb->sb_logblocks = be32_to_host(sb->sb_logblocks);
  sb->sb_versionnum = be16_to_host(sb->sb_versionnum);
  sb->sb_sectsize = be16_to_host(sb->sb_sectsize);
  sb->sb_inodesize = be16_to_host(sb->sb_inodesize);
  sb->sb_inopblock = be16_to_host(sb->sb_inopblock);
  sb->sb_icount = be64_to_host(sb->sb_icount);
  sb->sb_ifree = be64_to_host(sb->sb_ifree);
  sb->sb_fdblocks = be64_to_host(sb->sb_fdblocks);
  sb->sb_frextents = be64_to_host(sb->sb_frextents);
  sb->sb_uquotino = be64_to_host(sb->sb_uquotino);
  sb->sb_gquotino = be64_to_host(sb->sb_gquotino);
  sb->sb_qflags = be16_to_host(sb->sb_qflags);
  sb->sb_inoalignmt = be32_to_host(sb->sb_inoalignmt);
  sb->sb_unit = be32_to_host(sb->sb_unit);
  sb->sb_width = be32_to_host(sb->sb_width);
  sb->sb_logsectsize = be16_to_host(sb->sb_logsectsize);
  sb->sb_logsunit = be32_to_host(sb->sb_logsunit);
  sb->sb_features2 = be32_to_host(sb->sb_features2);
  sb->sb_bad_features2 = be32_to_host(sb->sb_bad_features2);
}
