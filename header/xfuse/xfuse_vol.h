#ifndef _XFUSE_VOL_H
#define _XFUSE_VOL_H

#include "xfuse_sb.h"

typedef struct {
  int dev;
  xfuse_sb sb;
} xfuse_volume;

extern int xfuse_vol_mount(xfuse_volume *vol, const char *device_name);
extern int xfuse_vol_unmount(xfuse_volume *vol);

#endif /* defined _XFUSE_VOL_H */
