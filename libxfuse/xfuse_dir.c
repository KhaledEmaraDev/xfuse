#include <errno.h>
#include <stdio.h>
#include <stdlib.h>

#include "xfuse_dir.h"
#include "xfuse_dir_sf.h"

int xfuse_dir_it_construct(xfuse_dir_it *dir_it, xfuse_ino *inode) {
  dir_it->ino = inode;

  if (dir_it->ino->node->di_format == XFUSE_DINODE_FMT_LOCAL) {
    if ((dir_it->dir_it = malloc(sizeof(xfuse_dir_sf))) == NULL) {
      fprintf(stderr, "xfuse_dir_it_construct -> Out of memory: %d\n", errno);
      return -1;
    }

    xfuse_dir_sf_construct(dir_it->dir_it, dir_it->ino);

    return 0;
  }

  errno = ENOTSUP;
  fprintf(stderr, "xfuse_dir_it_construct -> Cannot parse directory type: %d",
          errno);
  return -1;
}

void xfuse_dir_it_destruct(xfuse_dir_it *dir_it) { free(dir_it->dir_it); }

int xfuse_dir_it_seek(xfuse_dir_it *dir_it, uint16_t offset) {
  if (dir_it->ino->node->di_format == XFUSE_DINODE_FMT_LOCAL) {
    xfuse_dir_sf_it_seek(dir_it->dir_it, offset);
  }

  errno = ENOTSUP;
  fprintf(stderr, "xfuse_dir_it_seek -> Cannot parse directory type: %d",
          errno);
  return -1;
}

int xfuse_dir_it_get_next(xfuse_dir_it *dir_it, off_t *offset, char name[256],
                          ino_t *inode, unsigned char *type) {
  if (dir_it->ino->node->di_format == XFUSE_DINODE_FMT_LOCAL) {
    return xfuse_dir_sf_get_next(dir_it->dir_it, offset, name, inode, type);
  }

  errno = ENOTSUP;
  fprintf(stderr, "xfuse_dir_it_init -> Cannot parse directory type: %d",
          errno);
  return -1;
}
int xfuse_dir_it_lookup(xfuse_dir_it *dir_it, const char *name,
                        xfs_ino_t *inode) {
  if (dir_it->ino->node->di_format == XFUSE_DINODE_FMT_LOCAL) {
    return xfuse_dir_sf_lookup(dir_it->dir_it, name, inode);
  }

  errno = ENOTSUP;
  fprintf(stderr, "xfuse_dir_it_init -> Cannot parse directory type: %d",
          errno);
  return -1;
}