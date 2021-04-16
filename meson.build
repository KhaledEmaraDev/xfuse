project('xfuse', ['c'], version: '0.1.0',
        meson_version: '>= 0.42',
        default_options: [ 'buildtype=debugoptimized' ])

add_project_arguments('-D_REENTRANT', '-Wall', '-Wextra', '-Wno-sign-compare',
                      '-Wstrict-prototypes', '-Wmissing-declarations', '-Wwrite-strings',
                      '-fno-strict-aliasing', language: 'c')

include_dirs = include_directories('header')

subdir('src')
