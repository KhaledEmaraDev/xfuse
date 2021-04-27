project('xfuse', ['c'], version: '0.1.0',
        meson_version: '>= 0.40',
        default_options: [ 'buildtype=debugoptimized', 'warning_level=3' ])

add_global_arguments('-D_REENTRANT', '-DHAVE_CONFIG_H', '-Wno-sign-compare',
                     '-Wmissing-declarations', '-Wwrite-strings',
                     language: 'c')

# Some (stupid) GCC versions warn about unused return values even when they are
# casted to void. This makes -Wunused-result pretty useless, since there is no
# way to suppress the warning when we really *want* to ignore the value.
cc = meson.get_compiler('c')
code = '''
__attribute__((warn_unused_result)) int get_4() {
    return 4;
}
int main(void) {
    (void) get_4();
    return 0;
}'''
if not cc.compiles(code, args: [ '-O0', '-Werror=unused-result' ])
    message('Compiler warns about unused result even when casting to void')
    add_global_arguments('-Wno-unused-result', language: 'c')
endif

include_dirs = include_directories('header/xfuse')

subdir('libxfuse')
subdir('src')
