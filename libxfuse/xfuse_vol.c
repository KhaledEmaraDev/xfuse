#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

#include "xfuse_ino.h"
#include "xfuse_vol.h"

int xfuse_vol_mount(xfuse_volume *vol, const char *device_name) {
  int err = 0;

  if ((vol->dev = open(device_name, O_RDONLY)) == -1) {
    fprintf(stderr, "xfuse_volume_mount -> Cannot open %s: %d\n", device_name,
            errno);
    return -1;
  }

  if ((err = pread(vol->dev, &vol->sb, sizeof(xfuse_sb), 0)) ==
      -1) {
    fprintf(stderr, "xfuse_volume_mount -> Cannot read superblock: %d\n",
            errno);
    return -1;
  }

  if (err != sizeof(xfuse_sb)) {
    fprintf(stderr,
            "xfuse_volume_mount -> Cannot read entire superblock; read: %d\n",
            err);
    return -1;
  }

  xfuse_sb_swap_ends(&vol->sb);

  if (xfuse_sb_is_valid(&vol->sb) == -1) {
    fprintf(stderr,
            "xfuse_volume_init -> Superblock is not a valid XFS one: %d",
            errno);
    return -1;
  }

  return 0;
}

int xfuse_vol_unmount(xfuse_volume *vol) {
  if (close(vol->dev) == -1) {
    fprintf(stderr, "Cannot close device: %d\n", errno);
    return -1;
  }

  return 0;
}
