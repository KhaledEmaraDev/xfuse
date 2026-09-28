use std::{
    fmt,
    fs,
    io::Read,
    path::PathBuf,
    process::Command,
    sync::LazyLock,
    thread::sleep,
    time::{Duration, Instant},
};

/// Skip a test.
// Copied from nix.  Sure would be nice if the test harness knew about "skipped"
// tests as opposed to "passed" or "failed".
#[macro_export]
macro_rules! skip {
    ($($reason: expr),+) => {
        use ::std::io::{self, Write};

        let stderr = io::stderr();
        let mut handle = stderr.lock();
        writeln!(handle, $($reason),+).unwrap();
        return;
    }
}

/// Skip the test if we don't have the ability to mount fuse file systems.
// Copied from nix.
//
// The skip message names the file and line of the `require_fusefs!` call, which
// is what a test reader wants and is available everywhere.  It used to name the
// test's own function, through the `function_name!` macro that the
// `function_name` crate's `#[named]` attribute defines; that only works in a
// test target where *every* test carries the attribute, and in a target without
// it the name resolved to the crate of that name instead, which is not a macro.
#[cfg(target_os = "freebsd")]
#[macro_export]
macro_rules! require_fusefs {
    () => {
        use nix::unistd::Uid;
        use sysctl::Sysctl as _;

        if (!Uid::current().is_root()
            && ::sysctl::CtlValue::Int(0)
                == ::sysctl::Ctl::new(&"vfs.usermount")
                    .unwrap()
                    .value()
                    .unwrap())
            || !::std::path::Path::new("/dev/fuse").exists()
        {
            let here = ::std::panic::Location::caller();
            skip!(
                "{} requires the ability to mount fusefs. Skipping test ({}:{}).",
                ::std::module_path!(),
                here.file(),
                here.line()
            );
        }
    };
}

/// Skip the test if we don't have the ability to mount fuse file systems.
// Copied from nix.
#[cfg(target_os = "linux")]
#[macro_export]
macro_rules! require_fusefs {
    () => {
        if !::std::path::Path::new("/dev/fuse").exists() {
            let here = ::std::panic::Location::caller();
            skip!(
                "{} requires the ability to mount fusefs. Skipping test ({}:{}).",
                ::std::module_path!(),
                here.file(),
                here.line()
            );
        }
    };
}

#[macro_export]
macro_rules! require_root {
    () => {
        if !::nix::unistd::Uid::current().is_root() {
            use ::std::io::Write;

            let here = ::std::panic::Location::caller();
            let stderr = ::std::io::stderr();
            let mut handle = stderr.lock();
            writeln!(
                handle,
                "{} requires root privileges.  Skipping test ({}:{}).",
                ::std::module_path!(),
                here.file(),
                here.line()
            )
            .unwrap();
            return;
        }
    };
}

/// Make sure an image that has been extracted looks like an XFS file system.
///
/// A golden image that is truncated, empty, or was never finished being written
/// is indistinguishable from a file system that reads the wrong thing: the
/// mount succeeds, the sizes are right, and the contents are zeroes.  The one
/// cheap thing that tells the two apart is the superblock, so check it here,
/// where the failure can be named as what it is.
fn check_extracted_image(img: &std::path::Path) {
    const XFS_MAGIC: &[u8; 4] = b"XFSB";
    let mut magic = [0u8; 4];
    match fs::File::open(img).and_then(|mut f| f.read_exact(&mut magic)) {
        Ok(()) if &magic == XFS_MAGIC => {}
        Ok(()) => panic!(
            "{} is not an XFS image: it starts with {magic:02x?} rather than the superblock \
             magic.  The golden image was probably not decompressed properly; delete it and run \
             the tests again.",
            img.display()
        ),
        Err(e) => panic!(
            "{} could not be read ({e}); the golden image was probably not decompressed \
             properly.  Delete it and run the tests again.",
            img.display()
        ),
    }
}

fn prepare_image(filename: &str) -> PathBuf {
    let mut zimg = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    zimg.push("resources");
    zimg.push(filename);
    zimg.set_extension("img.zst");
    let mut img = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    img.push(filename);

    // If the golden image doesn't exist, or is out of date, rebuild it
    // Note: we can't accurately compare the two timestamps with less than 1
    // second granularity due to a zstd bug.
    // https://github.com/facebook/zstd/issues/3748
    let zmtime = fs::metadata(&zimg).unwrap().modified().unwrap();
    let mtime = fs::metadata(&img);
    if mtime.is_err() || (mtime.unwrap().modified().unwrap() + Duration::from_secs(1)) < zmtime {
        let output = Command::new("unzstd")
            .arg("-f")
            .arg("-o")
            .arg(&img)
            .arg(&zimg)
            .output()
            .expect("Uncompressing golden image failed");
        // A decompression that fails leaves a file that looks current to the
        // check above, and that every later test then reads.  Do not let one
        // pass silently.
        assert!(
            output.status.success(),
            "uncompressing {} failed: {}",
            zimg.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    check_extracted_image(&img);
    img
}

#[allow(unused)] // Not used by the write tests
pub static GOLDEN1K: LazyLock<PathBuf> = LazyLock::new(|| prepare_image("xfs1024.img"));
#[allow(unused)] // Not used by the write tests
pub static GOLDEN4K: LazyLock<PathBuf> = LazyLock::new(|| prepare_image("xfs4096.img"));
#[allow(unused)] // Not used by benches
pub static GOLDEN4KN: LazyLock<PathBuf> = LazyLock::new(|| prepare_image("xfs_4kn.img"));
#[allow(unused)] // Not used by benches
pub static GOLDENPREALLOCATED: LazyLock<PathBuf> =
    LazyLock::new(|| prepare_image("xfs_preallocated.img"));
#[allow(unused)] // Not used by benches
pub static GOLDENV4: LazyLock<PathBuf> = LazyLock::new(|| prepare_image("xfsv4.img"));
#[allow(unused)] // Not used by benches
pub static GOLDEN_NOFTYPE: LazyLock<PathBuf> = LazyLock::new(|| prepare_image("xfs_noftype.img"));
#[allow(unused)] // Not used by benches
pub static GOLDEN_NREXT64: LazyLock<PathBuf> = LazyLock::new(|| prepare_image("xfs_nrext64.img"));
#[allow(unused)] // Not used by benches
pub static GOLDEN_RT1: LazyLock<PathBuf> = LazyLock::new(|| prepare_image("xfs_rt1.img"));
#[allow(unused)] // Not used by benches
pub static GOLDEN_RT2: LazyLock<PathBuf> = LazyLock::new(|| prepare_image("xfs_rt2.img"));
#[allow(unused)] // Not used by benches
pub static GOLDEN_ATTRV1: LazyLock<PathBuf> = LazyLock::new(|| prepare_image("xfs_xattr_v1.img"));

#[allow(unused)] // Not used by benches
/// Copy a golden image to a scratch file that a test may modify.
///
/// The golden images are shared by every test in the binary, so a test that
/// writes must never touch the original.  The copy is made in the target
/// directory, and its name says where it came from.
pub fn writable_copy(golden: &std::path::Path, tag: &str) -> PathBuf {
    let name = golden.file_name().expect("golden image has a name");
    let mut copy = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    copy.push(format!(
        "write-{}-{}-{tag}",
        std::process::id(),
        name.to_string_lossy()
    ));
    // Start from the golden image every time, so that a failed test cannot
    // leave a half-written image behind for the next one.
    fs::copy(golden, &copy).expect("copying the golden image");
    copy
}

#[derive(Clone, Copy, Debug)]
pub struct WaitForError;

impl fmt::Display for WaitForError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "timeout waiting for condition")
    }
}

impl std::error::Error for WaitForError {}

/// Wait for a limited amount of time for the given condition to be true.
pub fn waitfor<C>(timeout: Duration, condition: C) -> Result<(), WaitForError>
where
    C: Fn() -> bool,
{
    let start = Instant::now();
    loop {
        if condition() {
            break Ok(());
        }
        if start.elapsed() > timeout {
            break Err(WaitForError);
        }
        sleep(Duration::from_millis(50));
    }
}
