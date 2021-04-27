#ifndef _XFUSE_DIR_SF_H
#define _XFUSE_DIR_SF_H

#include "xfuse_def.h"
#include "xfuse_ino.h"

typedef struct {
  u_int64_t i;
} __attribute__((packed)) xfuse_dir2_ino8_t;
typedef struct {
  u_int32_t i;
} __attribute__((packed)) xfuse_dir2_ino4_t;
typedef union {
  xfuse_dir2_ino8_t i8;
  xfuse_dir2_ino4_t i4;
} __attribute__((packed)) xfuse_dir2_inou_t;

typedef struct xfs_dir2_sf_entry {
  uint8_t namelen;
  xfs_dir2_data_off_t offset;
  uint8_t name[];
} __attribute__((packed)) xfs_dir2_sf_entry_t;

typedef struct xfs_dir2_sf_hdr {
  __uint8_t count;
  __uint8_t i8count;
  xfuse_dir2_inou_t parent;
} __attribute__((packed)) xfs_dir2_sf_hdr_t;

typedef struct {
  xfuse_ino *ino;
  xfs_dir2_sf_hdr_t *hdr;
  uint16_t lst_ent_off;
  uint8_t trk;
} xfuse_dir_sf;

extern void xfuse_dir_sf_construct(xfuse_dir_sf *dir, xfuse_ino *ino);
extern void xfuse_dir_sf_it_seek(xfuse_dir_sf *dir, uint16_t off);
extern int xfuse_dir_sf_get_next(xfuse_dir_sf *dir, off_t *off, char name[256],
                                 ino_t *ino, unsigned char *type);
extern int xfuse_dir_sf_lookup(xfuse_dir_sf *dir, const char *name,
                               xfs_ino_t *id);

#endif /* defined _XFUSE_DIR_SF_H */
