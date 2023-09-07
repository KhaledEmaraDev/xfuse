use std::{
    fmt,
    fs::metadata,
    path::PathBuf,
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
        let zmtime = metadata(&zimg).unwrap().modified().unwrap();
        let mtime = metadata(&img);
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

/// Mount and unmount the golden image
#[rstest]
#[named]
fn mount(harness: Harness) {
    require_fusefs!();

    drop(harness);
}

/// Tests relating to sf directories
mod sf {
    use super::*;

    /// Lookup entries in an sf directory
    #[rstest]
    #[named]
    fn lookup(harness: Harness) {
        require_fusefs!();

        let amode = AccessFlags::F_OK;
        for i in 0..3 {
            let p = harness.d.path().join(format!("sf/frame{:06}", i));
            access(p.as_path(), amode)
                .unwrap_or_else(|_| panic!("Lookup failed: {}", p.display()));
        }
    }
}

/// Tests relating to block directories
mod block {
    use super::*;

    /// Lookup entries in a block directory
    #[rstest]
    #[named]
    fn lookup(harness: Harness) {
        require_fusefs!();

        let amode = AccessFlags::F_OK;
        for i in 0..8 {
            let p = harness.d.path().join(format!("block/frame{:06}", i));
            access(p.as_path(), amode)
                .unwrap_or_else(|_| panic!("Lookup failed: {}", p.display()));
        }
    }
}

/// Tests relating to leaf directories
mod leaf {
    use super::*;

    /// Lookup entries in a leaf directory
    #[rstest]
    #[named]
    fn lookup(harness: Harness) {
        require_fusefs!();

        let amode = AccessFlags::F_OK;
        for i in 0..256 {
            let p = harness.d.path().join(format!("leaf/frame{:06}", i));
            access(p.as_path(), amode)
                .unwrap_or_else(|_| panic!("Lookup failed: {}", p.display()));
        }
    }
}

/// Tests relating to btree directories
mod btree {
    use super::*;

    /// Lookup entries in a btree directory
    #[rstest]
    #[named]
    #[ignore = "https://github.com/KhaledEmaraDev/xfuse/issues/22" ]
    fn lookup(harness: Harness) {
        require_fusefs!();

        let amode = AccessFlags::F_OK;
        for i in 0..204800 {
            let p = harness.d.path().join(format!("btree/frame{:06}", i));
            access(p.as_path(), amode)
                .unwrap_or_else(|_| panic!("Lookup failed: {}", p.display()));
        }
    }
}
