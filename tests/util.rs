use std::{
    fmt,
    fs,
    path::PathBuf,
    process::Command,
    thread::sleep,
    time::{Duration, Instant},
};

use lazy_static::lazy_static;

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

        if (!Uid::current().is_root()
            && ::sysctl::CtlValue::Int(0)
                == ::sysctl::Ctl::new(&"vfs.usermount")
                    .unwrap()
                    .value()
                    .unwrap())
            || !::std::path::Path::new("/dev/fuse").exists()
        {
            skip!(
                "{} requires the ability to mount fusefs. Skipping test.",
                concat!(::std::module_path!(), "::", function_name!())
            );
        }
    };
}

#[macro_export]
macro_rules! require_root {
    () => {
        if !::nix::unistd::Uid::current().is_root() {
            use ::std::io::Write;

            let stderr = ::std::io::stderr();
            let mut handle = stderr.lock();
            writeln!(
                handle,
                "{} requires root privileges.  Skipping test.",
                concat!(::std::module_path!(), "::", function_name!())
            )
            .unwrap();
            return;
        }
    };
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
        Command::new("unzstd")
            .arg("-f")
            .arg("-o")
            .arg(&img)
            .arg(&zimg)
            .output()
            .expect("Uncompressing golden image failed");
    }
    img
}

lazy_static! {
    pub static ref GOLDEN1K: PathBuf = prepare_image("xfs1024.img");
    pub static ref GOLDEN4K: PathBuf = prepare_image("xfs4096.img");
    pub static ref GOLDEN4KN: PathBuf = prepare_image("xfs_4kn.img");
    pub static ref GOLDENPREALLOCATED: PathBuf = prepare_image("xfs_preallocated.img");
    pub static ref GOLDENV4: PathBuf = prepare_image("xfsv4.img");
    pub static ref GOLDEN_NOFTYPE: PathBuf = prepare_image("xfs_noftype.img");
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
