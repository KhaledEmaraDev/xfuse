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

/// Does this look like a golden image that was extracted properly?
///
/// Not all of the golden images are file systems: `xfs_rt2.img` is a real-time
/// device, which holds nothing but the data blocks a real-time file occupies, so
/// it has no superblock and its first bytes are the test's own pattern.  The one
/// thing every image has in common is that its first bytes are not all zeroes,
/// which is exactly what a failed extraction leaves behind.
///
/// The tail is not checked, because it cannot be: the images are sized to a round
/// number of blocks, so the end of every one of them is unused space.
fn looks_extracted(img: &std::path::Path) -> bool {
    const WINDOW: usize = 512;
    let Ok(mut f) = fs::File::open(img) else {
        return false;
    };
    let mut head = [0u8; WINDOW];
    match f.read_exact(&mut head) {
        Ok(()) => head.iter().any(|b| *b != 0),
        // A file shorter than the window is not an image.
        Err(_) => false,
    }
}

/// Decompress a golden image, and complain loudly if it did not work.
fn extract_image(zimg: &std::path::Path, img: &std::path::Path) {
    let output = Command::new("unzstd")
        .arg("-f")
        .arg("-o")
        .arg(img)
        .arg(zimg)
        .output()
        .expect("Uncompressing golden image failed");
    // A decompression that fails leaves a file that looks current to the
    // staleness check, and that every test then reads.  Do not let one pass
    // silently.
    assert!(
        output.status.success(),
        "uncompressing {} failed: {}",
        zimg.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        looks_extracted(img),
        "{} does not look like a decompressed golden image: it is empty or all zeroes.  Delete it \
         and run the tests again.",
        img.display()
    );
}

pub fn prepare_image(filename: &str) -> PathBuf {
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
    let stale =
        mtime.is_err() || (mtime.unwrap().modified().unwrap() + Duration::from_secs(1)) < zmtime;
    // Being newer than the compressed image is not proof that the extraction
    // finished: a run that was interrupted part way through leaves a file that
    // looks current and that every test would then read.  So the contents have a
    // say in it, and a file that does not look extracted is decompressed again.
    if stale || !looks_extracted(&img) {
        extract_image(&zimg, &img);
    }
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
/// An image made by `scripts/mkimg.sh`'s `mkfs_writable`: a freshly formatted
/// file system that no test has been run against yet.
///
/// Every other image here was built to be awkward -- fragmented, preallocated,
/// with features switched off one at a time -- so the easy end of the
/// allocator's range, a file system whose groups hold their free space in a
/// single leaf, is the case that nothing else covers.
#[allow(unused)] // Not used by the read or bench targets
pub static GOLDENWRITABLE: LazyLock<PathBuf> = LazyLock::new(|| prepare_image("xfs_writable.img"));
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
