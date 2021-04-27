#define _GNU_SOURCE

#include <dirent.h>
#include <errno.h>
#include <fuse_lowlevel.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "xfuse_def.h"
#include "xfuse_dir.h"
#include "xfuse_ino.h"
#include "xfuse_sb.h"
#include "xfuse_vol.h"

static xfuse_volume *get_xfuse_volume(fuse_req_t req) {
  return (xfuse_volume *)fuse_req_userdata(req);
}

static xfuse_ino *get_xfuse_inode(fuse_req_t req, xfs_ino_t id) {
  if (id == FUSE_ROOT_ID)
    id = get_xfuse_volume(req)->sb.sb_rootino;

  xfuse_ino *ino = malloc(sizeof(xfuse_ino));
  if (ino == NULL) {
    fuse_log(FUSE_LOG_ERR, "get_xfuse_inode -> Out of memory: %d\n", errno);
    return NULL;
  }

  if (xfuse_ino_construct(ino, get_xfuse_volume(req), id) == -1) {
    fuse_log(FUSE_LOG_ERR, "get_xfuse_inode -> Cannot initialize inode: %d\n",
             errno);
    return NULL;
  }

  return ino;
}

static void free_xfuse_inode(xfuse_ino *ino) {
  xfuse_ino_destruct(ino);
  free(ino);
}

static void xfuse_init(void *userdata, struct fuse_conn_info *conn) {
  if (conn->capable & FUSE_CAP_EXPORT_SUPPORT)
    conn->want |= FUSE_CAP_EXPORT_SUPPORT;

  conn->want &= ~FUSE_CAP_ASYNC_READ;
  conn->want &= ~FUSE_CAP_ATOMIC_O_TRUNC;
  conn->want &= ~FUSE_CAP_IOCTL_DIR;
  conn->want &= ~FUSE_CAP_AUTO_INVAL_DATA;
  conn->want &= ~FUSE_CAP_ASYNC_DIO;
  conn->want &= ~FUSE_CAP_PARALLEL_DIROPS;
}

static void xfuse_statfs(fuse_req_t req, fuse_ino_t ino) {
  xfuse_volume *vol = get_xfuse_volume(req);
  struct statvfs stbuf = {
      .f_bsize = vol->sb.sb_blocksize,
      .f_frsize = vol->sb.sb_blocksize,
      .f_blocks = vol->sb.sb_dblocks,
      .f_bavail = vol->sb.sb_fdblocks,
      .f_files = vol->sb.sb_icount,
      .f_ffree = vol->sb.sb_ifree,
      .f_favail = vol->sb.sb_ifree,
      .f_fsid = XFS_SB_MAGIC,
      .f_flag = ST_NOATIME | ST_NODEV | ST_NODIRATIME | ST_NOEXEC | ST_NOSUID |
                ST_RDONLY,
      .f_namemax = 255,
  };

  fuse_reply_statfs(req, &stbuf);
}

static void xfuse_lookup(fuse_req_t req, fuse_ino_t parent, const char *name) {
  xfuse_volume *vol = get_xfuse_volume(req);
  xfuse_ino *parent_dir = get_xfuse_inode(req, parent);
  if (parent_dir == NULL) {
    fuse_log(FUSE_LOG_ERR, "xfuse_lookup -> Cannot initialize inode: %d\n",
             errno);
    fuse_reply_err(req, errno);
    return;
  }

  struct fuse_entry_param entry;
  memset(&entry, 0, sizeof(entry));
  entry.attr_timeout = 86400;
  entry.entry_timeout = 86400;

  if (!S_ISDIR(parent_dir->node->di_mode)) {
    errno = ENOTDIR;
    fuse_log(FUSE_LOG_ERR,
             "xfuse_lookup -> Cannot parse parent as a directory: %d\n", errno);
    fuse_reply_err(req, errno);
    return;
  }

  xfuse_dir_it *iterator = malloc(sizeof(xfuse_dir_it));
  if (iterator == NULL) {
    fuse_log(FUSE_LOG_ERR, "xfuse_lookup -> Out of memory: %d\n", errno);
    fuse_reply_err(req, errno);
    return;
  }

  if (xfuse_dir_it_construct(iterator, parent_dir) != 0) {
    free(iterator);
    fuse_log(FUSE_LOG_ERR,
             "xfuse_lookup -> Cannot initialize directory iterator: %d\n",
             errno);
    fuse_reply_err(req, errno);
    return;
  }

  if (xfuse_dir_it_lookup(iterator, name, (xfs_ino_t *)&entry.ino) != 0) {
    free(iterator);
    fuse_log(FUSE_LOG_ERR, "xfuse_lookup -> Cannot lookup name: %d\n", errno);
    fuse_reply_err(req, errno);
    return;
  }

  xfuse_ino *ino = get_xfuse_inode(req, entry.ino);
  if (ino == NULL) {
    fuse_log(FUSE_LOG_ERR, "xfuse_lookup -> Cannot initialize inode: %d\n",
             errno);
    fuse_reply_err(req, errno);
    return;
  }

  entry.attr.st_ino = entry.ino;
  entry.attr.st_mode = ino->node->di_mode;
  entry.attr.st_nlink = ino->node->di_nlink;
  entry.attr.st_uid = ino->node->di_uid;
  entry.attr.st_gid = ino->node->di_gid;
  entry.attr.st_size = ino->node->di_size;
  entry.attr.st_blksize = vol->sb.sb_blocksize;
  entry.attr.st_blocks = ino->node->di_nblocks;
  xfs_inode_get_access_time(ino->node, &entry.attr.st_atim);
  xfs_inode_get_access_time(ino->node, &entry.attr.st_atim);
  xfs_inode_get_modification_time(ino->node, &entry.attr.st_mtim);
  xfs_inode_get_change_time(ino->node, &entry.attr.st_ctim);

  fuse_reply_entry(req, &entry);

  free_xfuse_inode(parent_dir);
  free_xfuse_inode(ino);
  xfuse_dir_it_destruct(iterator);
  free(iterator);
}

static void xfuse_getattr(fuse_req_t req, fuse_ino_t ino,
                          struct fuse_file_info *fi) {
  (void)fi;

  xfuse_volume *vol = get_xfuse_volume(req);
  struct stat buf;
  xfuse_ino *xfuse_inode = get_xfuse_inode(req, ino);
  if (xfuse_inode == NULL) {
    fuse_log(FUSE_LOG_ERR, "xfuse_getattr -> Cannot initialize inode: %d\n",
             errno);
    fuse_reply_err(req, errno);
    return;
  }

  buf.st_ino = xfuse_inode->id;
  buf.st_mode = xfuse_inode->node->di_mode;
  buf.st_nlink = xfuse_inode->node->di_nlink;
  buf.st_uid = xfuse_inode->node->di_uid;
  buf.st_gid = xfuse_inode->node->di_gid;
  buf.st_size = xfuse_inode->node->di_size;
  buf.st_blksize = vol->sb.sb_blocksize;
  buf.st_blocks = xfuse_inode->node->di_nblocks;
  xfs_inode_get_access_time(xfuse_inode->node, &buf.st_atim);
  xfs_inode_get_access_time(xfuse_inode->node, &buf.st_atim);
  xfs_inode_get_modification_time(xfuse_inode->node, &buf.st_mtim);
  xfs_inode_get_change_time(xfuse_inode->node, &buf.st_ctim);

  free_xfuse_inode(xfuse_inode);

  fuse_reply_attr(req, &buf, 86400);
}

static void xfuse_opendir(fuse_req_t req, fuse_ino_t ino,
                          struct fuse_file_info *fi) {
  xfuse_ino *xfuse_inode = get_xfuse_inode(req, ino);
  if (xfuse_inode == NULL) {
    fuse_log(FUSE_LOG_ERR, "xfuse_opendir -> Cannot initialize inode: %d\n",
             errno);
    fuse_reply_err(req, errno);
    return;
  }

  if (!S_ISDIR(xfuse_inode->node->di_mode)) {
    errno = ENOTDIR;
    fuse_log(FUSE_LOG_ERR,
             "xfuse_opendir -> Cannot parse parent as a directory: %d\n",
             errno);
    fuse_reply_err(req, errno);
    return;
  }

  xfuse_dir_it *iterator = malloc(sizeof(xfuse_dir_it));
  if (iterator == NULL) {
    fuse_log(FUSE_LOG_ERR, "xfuse_opendir -> Out of memory: %d\n", errno);
    fuse_reply_err(req, errno);
    return;
  }

  if (xfuse_dir_it_construct(iterator, xfuse_inode) != 0) {
    free(iterator);
    fuse_log(FUSE_LOG_ERR,
             "xfuse_opendir -> Cannot initialize directory iterator: %d\n",
             errno);
    fuse_reply_err(req, errno);
    return;
  }

  iterator->ent = NULL;
  iterator->off = 0;

  fi->fh = (uint64_t)(uintptr_t)iterator;
  fuse_reply_open(req, fi);
}

static void xfuse_readdir(fuse_req_t req, fuse_ino_t ino, size_t size,
                          off_t offset, struct fuse_file_info *fi) {
  (void)ino;

  xfuse_dir_it *it = (xfuse_dir_it *)fi->fh;

  char *buf;
  char *p;
  size_t rem = size;
  int err;

  buf = malloc(size);
  if (!buf) {
    fuse_log(FUSE_LOG_ERR, "xfuse_do_readdir -> Out of memory: %d\n", errno);
    fuse_reply_err(req, errno);
    return;
  }
  memset(buf, 0, size);
  p = buf;

  if (offset != it->off) {
    xfuse_dir_it_seek(it, offset);
    it->ent = NULL;
    it->off = offset;
  }

  while (true) {
    size_t entsize;
    off_t nextoff;
    const char *name;

    if (!it->ent) {
      if ((it->ent = malloc(sizeof(struct dirent))) == NULL) {
        fuse_log(FUSE_LOG_ERR, "xfuse_do_readdir -> Out of memory: %d\n",
                 errno);
        fuse_reply_err(req, errno);
        return;
      }
      memset(it->ent, 0, sizeof(*it->ent));

      errno = 0;
      err = xfuse_dir_it_get_next(it, &it->ent->d_off, it->ent->d_name,
                                  &it->ent->d_ino, &it->ent->d_type);
      if (err != 0) {
        free(it->ent);
        it->ent = NULL;

        if (errno == ENOENT) { // End of stream
          return;
        } else {
          fuse_log(
              FUSE_LOG_ERR,
              "xfuse_do_readdir -> Cannot get next entry in directory: %d\n",
              errno);
          fuse_reply_err(req, errno);
          return;
        }
      }
    }

    nextoff = it->ent->d_off;
    name = it->ent->d_name;

    struct stat st = {
        .st_ino = it->ent->d_ino,
        .st_mode = it->ent->d_type << 12,
    };
    entsize = fuse_add_direntry(req, p, rem, name, &st, nextoff);

    if (entsize > rem) {
      break;
    }

    p += entsize;
    rem -= entsize;

    free(it->ent);
    it->ent = NULL;
    it->off = nextoff;
  }

  if (rem == size)
    fuse_reply_err(req, errno);
  else
    fuse_reply_buf(req, buf, size - rem);

  free(buf);
}

static void xfuse_releasedir(fuse_req_t req, fuse_ino_t ino,
                             struct fuse_file_info *fi) {
  free_xfuse_inode(((xfuse_dir_it *)(uintptr_t)fi->fh)->ino);
  xfuse_dir_it_destruct((xfuse_dir_it *)(uintptr_t)fi->fh);
  free((xfuse_dir_it *)(uintptr_t)fi->fh);
  fuse_reply_err(req, 0);
}

static const struct fuse_lowlevel_ops xfuse_oper = {
    .init = xfuse_init,
    .statfs = xfuse_statfs,
    .lookup = xfuse_lookup,
    .getattr = xfuse_getattr,
    .opendir = xfuse_opendir,
    .readdir = xfuse_readdir,
    .releasedir = xfuse_releasedir,
};

int main(int argc, char *argv[]) {
  struct fuse_args args = FUSE_ARGS_INIT(argc, argv);
  struct fuse_session *session;
  struct fuse_cmdline_opts opts;
  struct fuse_loop_config config;
  xfuse_volume xfuse;
  int ret = -1;

  if (fuse_parse_cmdline(&args, &opts) != 0)
    return EXIT_FAILURE;

  if (opts.show_help) {
    printf("usage: %s [options] <mountpoint>\n\n", argv[0]);
    fuse_cmdline_help();
    fuse_lowlevel_help();
    ret = 0;
    goto err_out1;
  } else if (opts.show_version) {
    printf("xfuse version 0.1.0\n");
    printf("FUSE library version %s\n", fuse_pkgversion());
    fuse_lowlevel_version();
    ret = 0;
    goto err_out1;
  }

  if (opts.mountpoint == NULL) {
    printf("usage: %s [options] <mountpoint>\n", argv[0]);
    printf("       %s --help\n", argv[0]);
    ret = 1;
    goto err_out1;
  }

  if (xfuse_vol_mount(&xfuse, opts.mountpoint) != 0) {
    fuse_log(FUSE_LOG_ERR, "Cannot mount %s: %d\n", opts.mountpoint, errno);
    exit(1);
  }

  session = fuse_session_new(&args, &xfuse_oper, sizeof(xfuse_oper), &xfuse);
  if (session == NULL)
    goto err_out1;

  if (fuse_set_signal_handlers(session) != 0)
    goto err_out2;

  if (fuse_session_mount(session, opts.mountpoint) != 0)
    goto err_out3;

  fuse_daemonize(opts.foreground);

  /* Block until ctrl+c or fusermount -u */
  if (opts.singlethread)
    ret = fuse_session_loop(session);
  else {
    config.clone_fd = opts.clone_fd;
    config.max_idle_threads = opts.max_idle_threads;
    ret = fuse_session_loop_mt(session, &config);
  }

  fuse_session_unmount(session);
err_out3:
  fuse_remove_signal_handlers(session);
err_out2:
  fuse_session_destroy(session);
err_out1:
  free(opts.mountpoint);
  fuse_opt_free_args(&args);

  xfuse_vol_unmount(&xfuse);

  return ret ? EXIT_FAILURE : EXIT_SUCCESS;
}
