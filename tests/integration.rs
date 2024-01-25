use std::{
    ffi::{OsStr, OsString},
    fmt,
    fs,
    io,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
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

/// How many extended attributes are present on each file?
fn attrs_per_file(f: &str) -> usize {
    match f {
        "local" => 4,
        "extents" => 64,
        _ => 0
    }
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
    #[allow(clippy::if_same_then_else)]
    fn drop(&mut self) {
        loop {
            let cmd = Command::new("umount")
                .arg(self.d.path())
                .output();
            match cmd {
                Err(e) => {
                    eprintln!("Executing umount failed: {}", e);
                    if std::thread::panicking() {
                        // Can't double panic
                        return;
                    }
                    panic!("Executing umount failed");
                },
                Ok(output) => {
                    let errmsg = OsString::from_vec(output.stderr)
                        .into_string()
                        .unwrap();
                    if output.status.success() {
                        break;
                    } else if errmsg.contains("not a file system root directory")
                    {
                        // The daemon probably crashed.
                        break;
                    } else if errmsg.contains("Device busy") {
                        println!("{}", errmsg);
                    } else {
                        if std::thread::panicking() {
                            // Can't double panic
                            println!("{}", errmsg);
                            return;
                        }
                        panic!("{}", errmsg);
                    }
                }
            }
            sleep(Duration::from_millis(50));
        }
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

#[template]
#[rstest]
#[case::local("local")]
#[case::extents("extents")]
fn all_xattr_fork_types(d: &str) {}

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

#[named]
#[apply(all_xattr_fork_types)]
fn getextattr(harness: Harness, #[case] d: &str) {
    require_fusefs!();

    let p = harness.d.path().join("xattrs").join(d);

    for i in 0..attrs_per_file(d) {
        let s = format!("user.attr.{:06}", i);
        let attrname = OsStr::new(s.as_str());
        let expected_value = OsString::from(format!("value.{:06}", i));
        let binary_value = xattr::get(&p, attrname).unwrap().unwrap();
        let value = OsStr::from_bytes(&binary_value[..]);
        assert_eq!(expected_value, value);
    }
}

/// Lookup the size of an extended attribute without fetching it.
// This test is freebsd-specific because the relevant syscall is.  It could be
// implemented for Linux too, but I haven't done so.
#[cfg(target_os = "freebsd")]
#[named]
#[apply(all_xattr_fork_types)]
fn getextattr_size(harness: Harness, #[case] d: &str) {
    use std::{convert::TryFrom, ffi::CString, ptr};

    require_fusefs!();

    let ns = libc::EXTATTR_NAMESPACE_USER;
    let p = harness.d.path().join("xattrs").join(d);
    let expected_len = "value.000000".len();
    let cpath = CString::new(p.as_os_str().as_bytes()).unwrap();

    for i in 0..attrs_per_file(d) {
        let s = format!("attr.{:06}", i);
        let attrname = OsStr::new(s.as_str());
        let cattrname = CString::new(attrname.as_bytes()).unwrap();
        let r = unsafe {
            libc::extattr_get_file(
                cpath.as_ptr(),
                ns,
                cattrname.as_ptr(),
                ptr::null_mut(),
                0
            )
        };
        if let Ok(r) = usize::try_from(r) {
            assert_eq!(expected_len, r);
        } else {
            panic!("{}", io::Error::last_os_error());
        }
    }
}

/// Hardlinks work. stat should return the same metadata for each and the link
/// count should be correct. lookup via both paths should return the same ino.
#[named]
#[rstest]
fn hardlink(harness: Harness) {
    require_fusefs!();

    let path1 = harness.d.path().join("files").join("hello.txt");
    let path2 = harness.d.path().join("files").join("hello2.txt");

    let stat1 = nix::sys::stat::stat(&path1).unwrap();
    let stat2 = nix::sys::stat::stat(&path2).unwrap();
    assert_eq!(stat1, stat2);
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
#[rstest]
#[case::sf("sf")]
#[case::block("block")]
#[case::leaf("leaf")]
#[case::node("node")]
#[case::btree("btree")]
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

#[named]
#[apply(all_xattr_fork_types)]
fn lsextattr(harness: Harness, #[case] d: &str) {
    require_fusefs!();

    let p = harness.d.path().join("xattrs").join(d);
    let mut count = 0;

    let mut all_attrnames = xattr::list(p).unwrap().collect::<Vec<_>>();
    all_attrnames.sort_unstable();
    for (i, attrname) in all_attrnames.into_iter().enumerate() {
        let expected_name = OsString::from(format!("user.attr.{:06}", i));
        //eprintln!("{:?}", attrname);
        assert_eq!(expected_name, attrname);
        count += 1;
    }
    assert_eq!(count, attrs_per_file(d));
}

/// Lookup the size of the extended attribute list of a file, without fetching
/// it.
// This test is freebsd-specific because the relevant syscall is.  It could be
// implemented for Linux too, but I haven't done so.
#[cfg(target_os = "freebsd")]
#[named]
#[apply(all_xattr_fork_types)]
fn lsextattr_size(harness: Harness, #[case] d: &str) {
    use std::{convert::TryFrom, ffi::CString, ptr};
    require_fusefs!();

    let ns = libc::EXTATTR_NAMESPACE_USER;
    let p = harness.d.path().join("xattrs").join(d);
    let bytes_per_attr = "attr.000000".len() + 1;
    let expected_len = bytes_per_attr * attrs_per_file(d);
    let cpath = CString::new(p.as_os_str().as_bytes()).unwrap();

    let r = unsafe {
        libc::extattr_list_file(
            cpath.as_ptr(),
            ns,
            ptr::null_mut(),
            0
        )
    };
    if let Ok(r) = usize::try_from(r) {
        assert_eq!(expected_len, r);
    } else {
        panic!("{}", io::Error::last_os_error());
    }
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

/// List a directory's hidden contents with readdir
// Use Nix::dir::Dir instead of std::fs::read_dir, because the latter
// unconditionally hides the hidden entries.
#[named]
#[rstest]
#[case::sf("sf")]
#[case::block("block")]
#[case::leaf("leaf")]
#[case::node("node")]
#[case::btree("btree")]
fn readdir_dots(harness: Harness, #[case] d: &str) {
    use nix::{dir::Dir, fcntl::OFlag, sys::stat::Mode};
    require_fusefs!();

    let root_md = fs::metadata(harness.d.path()).unwrap();
    let dir_md = fs::metadata(harness.d.path().join(d)).unwrap();

    let dpath = harness.d.path().join(d);
    let mut dir = Dir::open(&dpath, OFlag::O_RDONLY, Mode::S_IRUSR).unwrap();
    let mut ents = dir.iter();

    // The first entry should be "."
    let dot = ents.next().unwrap().unwrap();
    assert_eq!(".", dot.file_name().to_str().unwrap());
    assert_eq!(dir_md.ino(), dot.ino());

    // Next should be ".."
    let dotdot = ents.next().unwrap().unwrap();
    assert_eq!("..", dotdot.file_name().to_str().unwrap());
    assert_eq!(root_md.ino(), dotdot.ino());
}

#[named]
#[rstest]
#[case::sf("sf", "dest")]
#[case::extent("max", "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDE")]
fn readlink(harness: Harness, #[case] linkname: &str, #[case] destname: &str)
{
    require_fusefs!();

    let path = harness.d.path().join("links").join(linkname);
    let dest = fs::read_link(path).unwrap();
    assert_eq!(dest.as_os_str(), destname);
}

mod stat {
    use super::*;

    /// Verify all of an inode's metadata
    // This may need to be updated whenever the golden image gets rebuilt.
    #[named]
    #[rstest]
    fn file(harness: Harness) {
        require_fusefs!();

        let path = harness.d.path().join("files").join("hello.txt");

        // Due to the interaction of two bugs, we can't use std::fs::metadata here.
        // Instead, we'll use the lower-level nix::sys::stat::stat
        // https://github.com/rust-lang/rust/issues/108277
        // https://bugs.freebsd.org/bugzilla/show_bug.cgi?id=276602
        let stat = nix::sys::stat::stat(&path).unwrap();

        assert_eq!(stat.st_mtime, 401526123);
        assert_eq!(stat.st_mtime_nsec, 0);  // mkimg.sh can't set nsec
        assert_eq!(stat.st_atime, 1332497106);
        assert_eq!(stat.st_atime_nsec, 0);  // mkimg.sh can't set nsec
        // mkimg.sh doesn't have a way to set ctime.  So just check that it's
        // greater than mtime.
        assert!(stat.st_ctime > stat.st_mtime || 
                stat.st_ctime_nsec > stat.st_mtime_nsec);
        assert_eq!(stat.st_ino, 44966);
        assert_eq!(stat.st_size, 14);
        assert_eq!(stat.st_blksize, 4096);
        assert_eq!(stat.st_blocks, 1);
        assert_eq!(stat.st_uid, 1234);
        assert_eq!(stat.st_gid, 5678);
        assert_eq!(stat.st_mode, libc::S_IFREG | 0o1234);
        assert_eq!(stat.st_nlink, 2);
    }

    /// stat should work on symlinks
    #[named]
    #[rstest]
    #[case::sf("sf", 9353)]
    #[case::extent("max", 9354)]
    fn symlink(harness: Harness, #[case] linkname: &str, #[case] ino: libc::ino_t)
    {
        require_fusefs!();

        let path = harness.d.path().join("links").join(linkname);

        let flags = nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW;
        let stat = nix::sys::stat::fstatat(libc::AT_FDCWD, &path,
                                           flags).unwrap();
        assert_eq!(1, stat.st_nlink, "AT_SYMLINK_NOFOLLOW was ignored");
        assert_eq!(ino, stat.st_ino);
    }
}

#[named]
#[rstest]
fn statvfs(harness: Harness) {
    require_fusefs!();

    let svfs = nix::sys::statvfs::statvfs(harness.d.path()).unwrap();
    // xfuse is always read-only.
    assert!(svfs.flags().contains(nix::sys::statvfs::FsFlags::ST_RDONLY));
    assert_eq!(svfs.fragment_size(), 4096);
    assert_eq!(svfs.blocks(), 6824);
    
    // Linux's calculation for f_files is very confusing and not supported by
    // the XFS documentation.  I think it may be wrong.  So don't assert on it
    // here.
    assert_eq!(svfs.files() - svfs.files_free(), 9650);
    assert_eq!(svfs.files_free(), svfs.files_available());

    // Linux's calculation for blocks available and free is complicated and the
    // docs indicate that it's approximate.  So don't assert on the exact value.
    assert_eq!(svfs.blocks_available(), svfs.blocks_free());

    // There are legitimate questions about what the correct value for f_bsize
    // really is.  Until that's decided, don't assert on it.
    // https://bugs.freebsd.org/bugzilla/show_bug.cgi?id=253424

    // svfs.f_fsid is not meaningful.  Use stat().f_fsid instead

    // svfs.f_namemax is DONTCARE.  This information should be retrieved via
    // pathconf instead.
}
