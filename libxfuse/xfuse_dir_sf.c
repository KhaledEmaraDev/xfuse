#include <errno.h>
#include <stdio.h>
#include <string.h>

#include "xfuse_dir_sf.h"
#include "xfuse_end.h"
#include "xfuse_ino.h"

size_t xfuse_dir_sf_get_header_size(xfuse_dir_sf *dir);
uint8_t xfuse_dir_sf_get_file_type(xfuse_dir_sf *dir, xfs_dir2_sf_entry_t *ent);
xfs_dir2_sf_entry_t *xfuse_dir_sf_get_first_entry(xfuse_dir_sf *dir);
xfs_ino_t xfuse_dir_sf_get_ino(xfuse_dir_sf *dir, xfuse_dir2_inou_t *inum);
xfs_ino_t xfuse_dir_sf_get_entry_ino(xfuse_dir_sf *dir,
                                     xfs_dir2_sf_entry_t *ent);
size_t xfuse_dir_sf_get_entry_size(xfuse_dir_sf *dir, int name_len);

void xfuse_dir_sf_construct(xfuse_dir_sf *dir, xfuse_ino *ino) {
  dir->ino = ino;
  dir->trk = 0;
  dir->hdr =
      (xfs_dir2_sf_hdr_t *)((void *)((char *)dir->ino->buf + DATA_FORK_OFFSET));
}

size_t xfuse_dir_sf_get_header_size(xfuse_dir_sf *dir) {
  if (dir->hdr->i8count)
    return sizeof(xfs_dir2_sf_hdr_t);
  else
    return sizeof(xfs_dir2_sf_hdr_t) - sizeof(uint32_t);
}

uint8_t xfuse_dir_sf_get_file_type(xfuse_dir_sf *dir,
                                   xfs_dir2_sf_entry_t *ent) {
  if (xfuse_sb_has_file_type_field(&dir->ino->vol->sb) == false) {
    fprintf(stderr, "xfuse_dir_sf_get_file_type -> Cannot detect type: %d",
            errno);
    return -1;
  }

  return ent->name[ent->namelen];
}

xfs_dir2_sf_entry_t *xfuse_dir_sf_get_first_entry(xfuse_dir_sf *dir) {
  return (xfs_dir2_sf_entry_t *)((char *)dir->hdr +
                                 xfuse_dir_sf_get_header_size(dir));
}

xfs_ino_t xfuse_dir_sf_get_ino(xfuse_dir_sf *dir, xfuse_dir2_inou_t *inum) {
  if (dir->hdr->i8count)
    return be64_to_host(inum->i8.i);
  else
    return be32_to_host(inum->i4.i);
}

xfs_ino_t xfuse_dir_sf_get_entry_ino(xfuse_dir_sf *dir,
                                     xfs_dir2_sf_entry_t *ent) {
  if (xfuse_sb_has_file_type_field(&dir->ino->vol->sb) == false) {
    return xfuse_dir_sf_get_ino(
        dir, (xfuse_dir2_inou_t *)(ent->name + ent->namelen));
  }

  return xfuse_dir_sf_get_ino(
      dir, (xfuse_dir2_inou_t *)(ent->name + ent->namelen + sizeof(uint8_t)));
}

size_t xfuse_dir_sf_get_entry_size(xfuse_dir_sf *dir, int name_len) {
  return sizeof(xfs_dir2_sf_entry_t) + name_len +
         (xfuse_sb_has_file_type_field(&dir->ino->vol->sb) ? sizeof(uint8_t)
                                                           : 0) +
         (dir->hdr->i8count ? sizeof(uint64_t) : sizeof(uint32_t));
}

void xfuse_dir_sf_it_seek(xfuse_dir_sf *dir, uint16_t off) {
  dir->lst_ent_off = off;
}

int xfuse_dir_sf_get_next(xfuse_dir_sf *dir, off_t *off, char name[256],
                          ino_t *ino, unsigned char *type) {
  if (dir->trk == 0) {
    *off = 0;
    strncpy(name, ".", 2);
    name[1] = '\0';
    *ino = dir->ino->id;
    *type = dir->ino->node->di_mode;

    dir->trk = 1;
    return 0;
  }

  if (dir->trk == 1) {
    *off = 0;
    strncpy(name, "..", 3);
    name[2] = '\0';
    *ino = xfuse_dir_sf_get_ino(dir, &dir->hdr->parent);
    *type = dir->ino->node->di_mode;

    dir->trk = 2;
    return 0;
  }

  xfs_dir2_sf_entry_t *entry = xfuse_dir_sf_get_first_entry(dir);

  for (int i = 0; i < dir->hdr->count; i++) {
    uint16_t cur_offset = be16_to_host(entry->offset);
    if (cur_offset > dir->lst_ent_off) {
      *off = dir->lst_ent_off = cur_offset;
      memcpy(name, entry->name, entry->namelen);
      name[entry->namelen] = '\0';
      *ino = xfuse_dir_sf_get_entry_ino(dir, entry);
      *type = dir->ino->node->di_mode;

      return 0;
    }
    entry = (xfs_dir2_sf_entry_t *)((char *)entry + xfuse_dir_sf_get_entry_size(
                                                        dir, entry->namelen));
  }

  errno = ENOENT;
  fprintf(stderr, "xfuse_dir_sf_get_next -> Cannot find directory entry: %d",
          errno);
  return -1;
}

int xfuse_dir_sf_lookup(xfuse_dir_sf *dir, const char *name, xfs_ino_t *ino) {
  if (strcmp(name, ".") == 0 || strcmp(name, "..") == 0) {
    xfs_ino_t root_ino = dir->ino->vol->sb.sb_rootino;

    if (strcmp(name, ".") == 0 || (root_ino == dir->ino->id)) {
      *ino = dir->ino->id;

      return 0;
    }

    *ino = xfuse_dir_sf_get_ino(dir, &dir->hdr->parent);

    return 0;
  }

  xfs_dir2_sf_entry_t *entry = xfuse_dir_sf_get_first_entry(dir);

  for (int i = 0; i < dir->hdr->count; i++) {
    if (strncmp(name, (char *)entry->name, entry->namelen) == 0) {
      *ino = xfuse_dir_sf_get_entry_ino(dir, entry);

      return 0;
    }

    entry = (xfs_dir2_sf_entry_t *)((char *)entry + xfuse_dir_sf_get_entry_size(
                                                        dir, entry->namelen));
  }

  errno = ENOENT;
  fprintf(stderr, "xfuse_dir_sf_get_next -> Cannot find directory entry: %d",
          errno);
  return -1;
}
