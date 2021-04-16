#ifndef _XFUSE_VOLUME_H
#define _XFUSE_VOLUME_H

#include "xfuse_sb.h"

typedef struct {
  int device;
  xfuse_sb super_block;
} xfuse_volume;

extern int xfuse_volume_mount(xfuse_volume *vol, const char *device_name);
extern int xfuse_volume_unmount(xfuse_volume *vol);
extern int xfuse_volume_init(xfuse_volume *vol);

#endif /* defined _XFUSE_VOLUME_H */
