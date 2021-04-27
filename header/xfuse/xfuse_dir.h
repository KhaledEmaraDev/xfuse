#ifndef _XFUSE_DIR_H
#define _XFUSE_DIR_H

#include "xfuse_ino.h"
#include "xfuse_dir_sf.h"

typedef struct {
  xfuse_ino *ino;
  void *dir_it;
  struct dirent *ent;
  off_t off;
} xfuse_dir_it;

extern int xfuse_dir_it_construct(xfuse_dir_it *dir_it, xfuse_ino *inode);
extern void xfuse_dir_it_destruct(xfuse_dir_it *dir_it);
extern int xfuse_dir_it_seek(xfuse_dir_it *dir_it, uint16_t offset);
extern int xfuse_dir_it_get_next(xfuse_dir_it *dir_it, off_t *offset,
                                 char name[256], ino_t *inode,
                                 unsigned char *type);
extern int xfuse_dir_it_lookup(xfuse_dir_it *dir_it, const char *name,
                               xfs_ino_t *ino);

#endif /* defined _XFUSE_DIR_H */