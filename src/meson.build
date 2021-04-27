executable('xfuse', ['xfuse.c'],
           include_directories: include_dirs,
           dependencies: [ dependency('fuse3', version: '>= 3.1.0') ],
           c_args: ['-DFUSE_USE_VERSION=35'],
           link_with : libxfuse,
           install: true,
           install_dir: get_option('bindir'))
