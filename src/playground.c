#include <stdlib.h>

#include "xfuse_sb.h"
#include "xfuse_volume.h"

int main(int argc, char *argv[]) {
  xfuse_volume *vol = malloc(sizeof(xfuse_volume));
  xfuse_volume_mount(vol, "/dev/sdb1");
  xfuse_volume_unmount(vol);
  free(vol);
}