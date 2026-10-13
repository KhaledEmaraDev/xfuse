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

/// Is this statfs a FUSE mount?
///
/// Every harness polls for this before it touches the file system, because a
/// mount that is not up yet yields a statfs of whatever is underneath the
/// mount point, and every test that ran against that would fail in a way that
/// says nothing about the code under test.
///
/// FreeBSD reports a name, and the name a FUSE mount reports is what these
/// tests have always compared against.  Linux has no name to compare -- `nix`
/// offers the magic number instead -- so the check is the same question asked
/// of the only thing Linux exposes.
#[allow(unused)] // Not used by the write tests
#[cfg(target_os = "freebsd")]
pub fn is_fusefs(sfs: &nix::sys::statfs::Statfs) -> bool {
    sfs.filesystem_type_name() == "fusefs.xfs"
}

#[allow(unused)] // Not used by the write tests
#[cfg(target_os = "linux")]
pub fn is_fusefs(sfs: &nix::sys::statfs::Statfs) -> bool {
    sfs.filesystem_type() == nix::sys::statfs::FUSE_SUPER_MAGIC
}

/// A check this platform cannot make, counted and named rather than passed over.
///
/// A green run that says how many checks it did not make is honest; one that says
/// nothing is not, and the number is the whole of the difference.
#[allow(unused)] // Not used by the write tests
pub fn skipped_check(what: &str) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SKIPPED: AtomicUsize = AtomicUsize::new(0);
    let n = SKIPPED.fetch_add(1, Ordering::Relaxed) + 1;
    eprintln!("SKIPPED CHECK ({n} so far): {what}");
}

/// The largest an extended attribute *name list* can be here, if this platform
/// caps one.
///
/// Linux caps it.  `XATTR_SIZE_MAX`, in `include/linux/limits.h`, is 65536, and
/// `fuse_listxattr_write` compares the size the daemon reports against it and
/// answers `E2BIG` when the listing is longer -- before the daemon's list is
/// looked at at all, and whether or not the caller supplied a buffer big enough.
///
/// That is worth knowing precisely because it is not a limit this daemon imposes
/// and no change here can lift it: `xattrs/giant` holds 65536 names totalling
/// 1572864 bytes, and Linux can name at most 65536 bytes of them.  The listing of
/// that file is not merely wrong on Linux, it does not exist.
#[cfg(target_os = "linux")]
#[allow(unused)] // Not used by the write tests
pub const XATTR_LIST_MAX: Option<usize> = Some(65536);

/// FreeBSD's `extattr_list_file` reports a size and is given a buffer this test
/// sizes itself, so no cap was found to assert and none is invented here.
#[cfg(not(target_os = "linux"))]
#[allow(unused)] // Not used by the write tests
pub const XATTR_LIST_MAX: Option<usize> = None;

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
pub fn writable_copy(golden: &std::path::Path, tag: &str) -> WritableCopy {
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
    WritableCopy { path: copy }
}

/// A writable copy of a golden image, deleted when it goes out of scope.
///
/// This used to be a bare `PathBuf`, and that was a leak worth 129 gigabytes:
/// nothing ever removed the copies, so every run of every test left one behind,
/// and `CARGO_TARGET_TMPDIR` accumulated 1656 of them across images that are up
/// to 300 MiB each.  A path has no way to say when it is finished with, and no
/// caller was going to remember.
///
/// So the copy owns itself.  It derefs to a path, so `&image` and `image.path()`
/// keep working unchanged at every call site, and it removes the file when it is
/// dropped -- including when a test panics, which is when a leaked image is
/// least wanted.
#[derive(Debug)]
pub struct WritableCopy {
    path: PathBuf,
}

impl std::ops::Deref for WritableCopy {
    type Target = std::path::Path;

    fn deref(&self) -> &std::path::Path {
        &self.path
    }
}

impl AsRef<std::path::Path> for WritableCopy {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

impl AsRef<std::ffi::OsStr> for WritableCopy {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.path.as_os_str()
    }
}

impl Drop for WritableCopy {
    fn drop(&mut self) {
        // Best effort: a file that cannot be removed is not worth failing a test
        // that has already passed, and `cargo clean` will still get it.
        let _ = fs::remove_file(&self.path);
    }
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

/// The blocks two images differ in, **excluding the log**.
///
/// Excluding the log is not optional and not a refinement.  XFS is a logging file
/// system, so a modification writes to the log and the in-place metadata follows
/// later; an unmasked comparison of a change is 2048 blocks of journal in which the
/// metadata is somewhere among them.
///
/// The range comes from the **superblock's own numbers** (`Sb::log_blocks()`) and
/// never from searching for the log's magic.  That is deliberate: a magic search
/// finds the log only on an image that has been written, so on a golden image --
/// whose log is entirely zero -- it finds several thousand unrelated blocks and no
/// log at all, which reads exactly like "the magic is not the log" and is wrong.
///
/// Returns `(block_number, first_differing_offset, last_differing_offset)` for each
/// block that differs outside the log, which is a short list on any real operation.
#[allow(unused)] // The bench target shares this module and has no image fixtures.
pub fn changed_blocks_excluding_log(
    a: &[u8],
    b: &[u8],
    bs: usize,
    log: std::ops::Range<u64>,
) -> Vec<(u64, usize, usize)> {
    assert_eq!(
        a.len(),
        b.len(),
        "two images of different sizes cannot be compared"
    );
    let mut out = Vec::new();
    for blk in 0..(a.len() / bs) as u64 {
        if log.contains(&blk) {
            continue;
        }
        let (x, y) = (&a[blk as usize * bs..][..bs], &b[blk as usize * bs..][..bs]);
        if x == y {
            continue;
        }
        let first = x
            .iter()
            .zip(y.iter())
            .position(|(p, q)| p != q)
            .unwrap_or(0);
        let last = x
            .iter()
            .zip(y.iter())
            .rposition(|(p, q)| p != q)
            .unwrap_or(0);
        out.push((blk, first, last));
    }
    out
}

/// Only the integration target runs tests here; the bench includes this module
/// for its helpers and would otherwise compile them with no test harness.
#[cfg(test)]
#[allow(unused)]
mod image_diff_tests {
    use super::changed_blocks_excluding_log;

    /// Two images of the same filesystem, compared with the log masked, differ
    /// nowhere.
    ///
    /// Which sounds trivial and is the point: it is the *control* every fixture
    /// measurement needs, and without it a 2048-block journal cannot be told from a
    /// three-byte counter.
    #[test]
    fn an_image_does_not_differ_from_itself() {
        let a = vec![0u8; 8192];
        assert!(changed_blocks_excluding_log(&a, &a.clone(), 1024, 0..0).is_empty());
    }

    /// The log is excluded **by block number**, and nothing else is.
    ///
    /// Both halves matter: masking too little leaves the journal in the diff, and
    /// masking too much hides the change being looked for.  So this pins one block
    /// inside the masked range and one immediately outside it, and asserts that only
    /// the second is reported.
    #[test]
    fn a_block_inside_the_log_is_masked_and_one_outside_it_is_not() {
        let mut a = vec![0u8; 4096];
        a[1024..1028].copy_from_slice(&[1, 2, 3, 4]); // block 1
        a[2048..2052].copy_from_slice(&[5, 6, 7, 8]); // block 2
        let mut b = a.clone();
        b[2048..2052].copy_from_slice(&[9, 9, 9, 9]); // differs in block 2 only

        // Mask blocks 0 and 1.
        let d = changed_blocks_excluding_log(&a, &b, 1024, 0..2);
        assert_eq!(d.len(), 1, "exactly the unmasked block should be reported");
        assert_eq!(d[0].0, 2, "and it should be block 2");
        assert_eq!(
            (d[0].1, d[0].2),
            (0, 3),
            "with the offsets of the bytes that differ"
        );
    }
}
