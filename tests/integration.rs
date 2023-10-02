use std::{
    ffi::OsStr,
    fmt,
    fs,
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirEntryExt, MetadataExt}
    },
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant},
    thread::sleep
};

use assert_cmd::cargo::CommandCargoExt;
use function_name::named;
use lazy_static::lazy_static;
use nix::{
    sys::statfs::statfs,
    unistd::{AccessFlags, access}
};
use rstest::{fixture, rstest};
use rstest_reuse::{self, apply, template};
use tempfile::{tempdir, TempDir};

lazy_static! {
    static ref GOLDEN: PathBuf = {
        let mut zimg = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        zimg.push("resources/xfs.img.zst");
        let mut img = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        img.push("xfs.img");

        // If the golden image doesn't exist, or is out of date, rebuild it
        // Note: we can't accurately compare the two timestamps with less than 1
        // second granularity due to a zstd bug.
        // https://github.com/facebook/zstd/issues/3748
        let zmtime = fs::metadata(&zimg).unwrap().modified().unwrap();
        let mtime = fs::metadata(&img);
        if mtime.is_err() || (mtime.unwrap().modified().unwrap() +
                              Duration::from_secs(1)) < zmtime
        {
            Command::new("unzstd")
                .arg("-f")
                .arg("-o")
                .arg(&img)
                .arg(&zimg)
                .output()
                .expect("Uncompressing golden image failed");
        }
        img
    };
}

/// How many directory entries are in each directory?
// This is a function of the golden image creation.
fn ents_per_dir(d: &str) -> usize {
    match d {
        "sf" => 2,
        "block" => 32,
        "leaf" => 384,
        "node" => 1024,
        "btree" => 8192,
        _ => 0
    }
}

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
#[cfg(target_os = "freebsd")]
#[macro_export]
macro_rules! require_fusefs {
    () => {
        use nix::unistd::Uid;
        use sysctl::Sysctl as _;

        if (!Uid::current().is_root() &&
            ::sysctl::CtlValue::Int(0) ==
                ::sysctl::Ctl::new(&"vfs.usermount")
                    .unwrap()
                    .value()
                    .unwrap()) ||
            !::std::path::Path::new("/dev/fuse").exists()
        {
            skip!(
                "{} requires the ability to mount fusefs. Skipping test.",
                concat!(::std::module_path!(), "::", function_name!())
            );
        }
    };
}

macro_rules! require_root {
    () => {
        if ! ::nix::unistd::Uid::current().is_root() {
            use ::std::io::Write;

            let stderr = ::std::io::stderr();
            let mut handle = stderr.lock();
            writeln!(handle, "{} requires root privileges.  Skipping test.",
                concat!(::std::module_path!(), "::", function_name!()))
                .unwrap();
            return;
        }
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
            break (Err(WaitForError));
        }
        sleep(Duration::from_millis(50));
    }
}

/// A file-backed md(4) device.
pub struct Md(pub PathBuf);
impl Md {
    pub fn new(filename: &Path) -> io::Result<Self> {
        let output = Command::new("mdconfig")
            .args(["-a", "-t",  "vnode", "-f"])
            .arg(filename)
            .output()?;
        // Strip the trailing "\n"
        let l = output.stdout.len() - 1;
        let mddev = OsStr::from_bytes(&output.stdout[0..l]);
        let pb = Path::new("/dev").join(mddev);
        Ok(Self(pb))
    }

    pub fn as_path(&self) -> &Path {
        self.0.as_path()
    }
}
impl Drop for Md {
    fn drop(&mut self) {
        Command::new("mdconfig")
            .args(["-d", "-u"])
            .arg(&self.0)
            .output()
            .expect("failed to deallocate md(4) device");
    }
}

struct Harness {
    d: TempDir,
    child: Child
}

#[fixture]
fn harness() -> Harness {
    let d = tempdir().unwrap();
    let child = Command::cargo_bin("xfs-fuse").unwrap()
        .arg(GOLDEN.as_path())
        .arg(d.path())
        .spawn()
        .unwrap();

    waitfor(Duration::from_secs(5), || {
        let s = statfs(d.path()).unwrap();
        s.filesystem_type_name() == "fusefs.xfs"
    }).unwrap();

    Harness {
        d,
        child
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = Command::new("umount")
            .arg(self.d.path())
            .output();
        let _ = self.child.wait();
    }
}

struct MdHarness {
    _md: Md,
    d: TempDir,
    child: Child
}

#[fixture]
fn mdharness() -> MdHarness {
    let md = Md::new(GOLDEN.as_path()).unwrap();
    let d = tempdir().unwrap();
    let child = Command::cargo_bin("xfs-fuse").unwrap()
        .arg(md.0.as_path())
        .arg(d.path())
        .spawn()
        .unwrap();

    waitfor(Duration::from_secs(5), || {
        let s = statfs(d.path()).unwrap();
        s.filesystem_type_name() == "fusefs.xfs"
    }).unwrap();

    MdHarness {
        _md: md,
        d,
        child
    }
}

impl Drop for MdHarness {
    fn drop(&mut self) {
        let _ = Command::new("umount")
            .arg(self.d.path())
            .output();
        let _ = self.child.wait();
    }
}

#[template]
#[rstest]
#[case::sf("sf")]
#[case::block("block")]
#[case::leaf("leaf")]
#[case::node("node")]
#[ignore = "https://github.com/KhaledEmaraDev/xfuse/issues/30" ]
#[case::btree("btree")]
fn all_dir_types(d: &str) {}

/// Mount the image via md(4) and read all its metadata, to verify that we work
/// with devices that require all accesses to be sector size aligned.
// Regression test for https://github.com/KhaledEmaraDev/xfuse/issues/15
#[rstest]
#[named]
fn dev() {
    require_fusefs!();
    require_root!();
    let h = mdharness();

    let walker = walkdir::WalkDir::new(h.d.path())
        .into_iter()
        // Ignore btree dirs, for now.
        // https://github.com/KhaledEmaraDev/xfuse/issues/22
        .filter_entry(|e| !e.file_name().to_str().unwrap().starts_with("btree"));
    for entry in walker {
        let _ = entry.unwrap().metadata().unwrap();
    }
}

/// Mount and unmount the golden image
#[rstest]
#[named]
fn mount(harness: Harness) {
    require_fusefs!();

    drop(harness);
}

/// Lookup all entries in a directory
#[named]
#[apply(all_dir_types)]
fn lookup(harness: Harness, #[case] d: &str) {
    require_fusefs!();

    let amode = AccessFlags::F_OK;
    for i in 0..ents_per_dir(d) {
        let p = harness.d.path().join(format!("{d}/frame{i:06}"));
        access(p.as_path(), amode)
            .unwrap_or_else(|_| panic!("Lookup failed: {}", p.display()));
    }
}

/// Lookup a directory's "." and ".." entries.  Verify their inode numbers
#[named]
#[apply(all_dir_types)]
fn lookup_dots(harness: Harness, #[case] d: &str) {
    require_fusefs!();

    let root_md = fs::metadata(harness.d.path()).unwrap();
    let dir_md = fs::metadata(harness.d.path().join(d)).unwrap();
    let dotpath = harness.d.path().join(format!("{d}/."));
    let dot_md = fs::metadata(dotpath).unwrap();
    assert_eq!(dir_md.ino(), dot_md.ino());

    let dotdotpath = harness.d.path().join(format!("{d}/.."));
    let dotdot_md = fs::metadata(dotdotpath).unwrap();
    assert_eq!(root_md.ino(), dotdot_md.ino());
}

/// List a directory's contents with readdir
#[named]
#[rstest]
#[case::sf("sf")]
#[case::block("block")]
#[case::leaf("leaf")]
#[case::node("node")]
#[ignore = "https://github.com/KhaledEmaraDev/xfuse/issues/30" ]
#[case::btree("btree")]
fn readdir(harness: Harness, #[case] d: &str) {
    require_fusefs!();

    let dpath = harness.d.path().join(d);
    let ents = std::fs::read_dir(dpath)
        .unwrap();
    let mut count = 0;
    for (i, rent) in ents.enumerate() {
        let ent = rent.unwrap();
        let expected_name = format!("frame{:06}", i);
        assert_eq!(ent.file_name(), OsStr::new(&expected_name));
        assert!(ent.file_type().unwrap().is_file());
        let md = ent.metadata().unwrap();
        assert_eq!(ent.ino(), md.ino());
        // The other metadata fields are checked in a separate test case.
        count += 1;
    }
    assert_eq!(count, ents_per_dir(d));
}
