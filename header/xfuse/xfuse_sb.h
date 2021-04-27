#ifndef _XFUSE_SB_H
#define _XFUSE_SB_H

#include "xfuse_def.h"

#define BASICBLOCKLOG 9
#define BASICBLOCKSIZE (1 << BASICBLOCKLOG)

#define XFS_SB_VERSION_ATTRBIT 0x0010
#define XFS_SB_VERSION_NLINKBIT 0x0020
#define XFS_SB_VERSION_QUOTABIT 0x0040
#define XFS_SB_VERSION_ALIGNBIT 0x0080
#define XFS_SB_VERSION_DALIGNBIT 0x0100
#define XFS_SB_VERSION_SHAREDBIT 0x0200
#define XFS_SB_VERSION_LOGV2BIT 0x0400
#define XFS_SB_VERSION_SECTORBIT 0x0800
#define XFS_SB_VERSION_EXTFLGBIT 0x1000
#define XFS_SB_VERSION_DIRV2BIT 0x2000
#define XFS_SB_VERSION_MOREBITSBIT 0x4000

#define XFS_UQUOTA_ACCT 0x0001
#define XFS_UQUOTA_ENFD 0x0002
#define XFS_UQUOTA_CHKD 0x0004
#define XFS_PQUOTA_ACCT 0x0008
#define XFS_OQUOTA_ENFD 0x0010
#define XFS_OQUOTA_CHKD 0x0020
#define XFS_GQUOTA_ACCT 0x0040
#define XFS_GQUOTA_ENFD 0x0080
#define XFS_GQUOTA_CHKD 0x0100
#define XFS_PQUOTA_ENFD 0x0200
#define XFS_PQUOTA_CHKD 0x0400

#define XFS_SBF_READONLY 0x01

#define XFS_SB_VERSION2_LAZYSBCOUNTBIT 0x00000001
#define XFS_SB_VERSION2_ATTR2BIT 0x00000002
#define XFS_SB_VERSION2_PARENTBIT 0x00000010
#define XFS_SB_VERSION2_PROJID32BIT 0x00000080
#define XFS_SB_VERSION2_CRCBIT 0x00000100
#define XFS_SB_VERSION2_FTYPE 0x00000200

typedef struct {
  uint32_t sb_magicnum;
  uint32_t sb_blocksize;
  xfs_rfsblock_t sb_dblocks;
  xfs_rfsblock_t sb_rblocks;
  xfs_rtblock_t sb_rextents;
  uuid_t sb_uuid;
  xfs_fsblock_t sb_logstart;
  xfs_ino_t sb_rootino;
  xfs_ino_t sb_rbmino;
  xfs_ino_t sb_rsumino;
  xfs_agblock_t sb_rextsize;
  xfs_agblock_t sb_agblocks;
  xfs_agnumber_t sb_agcount;
  xfs_extlen_t sb_rbmblocks;
  xfs_extlen_t sb_logblocks;
  uint16_t sb_versionnum;
  uint16_t sb_sectsize;
  uint16_t sb_inodesize;
  uint16_t sb_inopblock;
  char sb_fname[12];
  uint8_t sb_blocklog;
  uint8_t sb_sectlog;
  uint8_t sb_inodelog;
  uint8_t sb_inopblog;
  uint8_t sb_agblklog;
  uint8_t sb_rextslog;
  uint8_t sb_inprogress;
  uint8_t sb_imax_pct;
  uint64_t sb_icount;
  uint64_t sb_ifree;
  uint64_t sb_fdblocks;
  uint64_t sb_frextents;
  xfs_ino_t sb_uquotino;
  xfs_ino_t sb_gquotino;
  uint16_t sb_qflags;
  uint8_t sb_flags;
  uint8_t sb_shared_vn;
  xfs_extlen_t sb_inoalignmt;
  uint32_t sb_unit;
  uint32_t sb_width;
  uint8_t sb_dirblklog;
  uint8_t sb_logsectlog;
  uint16_t sb_logsectsize;
  uint32_t sb_logsunit;
  uint32_t sb_features2;
  uint32_t sb_bad_features2;
} __attribute__((packed)) xfuse_sb;

extern int xfuse_sb_is_valid(xfuse_sb *sb);
extern bool xfuse_sb_has_file_type_field(xfuse_sb *sb);
extern uint8_t xfuse_sb_get_ag_ino_bits(xfuse_sb *sb);
extern void xfuse_sb_swap_ends(xfuse_sb *sb);

#endif /* defined _XFUSE_SB_H */