#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#include "xfuse_volume.h"

int xfuse_volume_mount(xfuse_volume *vol, const char *device_name) {
  if ((vol->device = open(device_name, O_RDONLY)) == -1) {
    fprintf(stderr, "xfuse_volume_mount -> Cannot open %s: %d\n", device_name,
            errno);
    return -1;
  }

  if (xfuse_volume_init(vol) == -1) {
    fprintf(stderr, "xfuse_volume_mount -> Cannot identify fs in %s: %d\n",
            device_name, errno);
    return -1;
  }

  return 0;
}

int xfuse_volume_unmount(xfuse_volume *vol) {
  if (close(vol->device) == -1) {
    fprintf(stderr, "Cannot close: %d\n", errno);
    return -1;
  }

  return 0;
}

int xfuse_volume_init(xfuse_volume *vol) {
  int err = 0;

  if ((err = pread(vol->device, &vol->super_block, sizeof(xfuse_sb), 0)) ==
      -1) {
    fprintf(stderr, "xfuse_volume_init -> Cannot read superblock: %d\n", errno);
    return -1;
  }

  if (err != sizeof(xfuse_sb)) {
    fprintf(stderr,
            "xfuse_volume_init -> Cannot read entire superblock; read: %d\n",
            err);
    return -1;
  }

  xfuse_sb_swap_ends(&vol->super_block);

  if (xfuse_sb_is_valid(&vol->super_block) == -1) {
    fprintf(stderr, "xfuse_volume_init -> Super is not a valid XFS one: %d",
            errno);
    return -1;
  }

  return 0;
}