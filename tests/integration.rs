use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{self, ErrorKind, Read},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{DirEntryExt, FileExt, MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command},
    thread::sleep,
    time::Duration,
};

use assert_cmd::cargo::CommandCargoExt;
use function_name::named;
use nix::{
    errno::Errno,
    unistd::{access, AccessFlags, Whence},
};
use rstest::{fixture, rstest};
use rstest_reuse::{self, apply, template};
use tempfile::{tempdir, TempDir};

mod util;
use util::{waitfor, GOLDEN1K, GOLDEN4K, GOLDEN4KN, GOLDENPREALLOCATED, GOLDENV4, GOLDEN_NOFTYPE};

#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
struct ExpectedXattr {
    name:  OsString,
    value: OsString,
}

/// Return an iterator over the extended attributes that ought to be present in each file, sorted
/// in order by name.
fn expected_xattrs_per_file(f: &str) -> impl Iterator<Item = ExpectedXattr> {
    let locals = (0..local_attrs_per_file(f)).map(|i| ExpectedXattr {
        name:  OsString::from(format!("user.attr.{:06}", i)),
        value: OsString::from(format!("value.{:06}", i)),
    });
    let remotes = (0..remote_attrs_per_file(f)).map(|i| {
        ExpectedXattr {
            name: OsString::from(format!("user.remote_attr.{:06}", i)),
            value: OsString::from(format!("_______________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________.{:06}", i))
        }
    });
    locals.chain(remotes)
}

/// How many extended attributes with local storage are present on each file?
fn local_attrs_per_file(f: &str) -> usize {
    match f {
        "xattrs/local" => 4,
        "xattrs/extents" => 64,
        "xattrs/btree2" => 0,
        "xattrs/btree2.5" => 0,
        "xattrs/btree3" => 0,
        "btree2.with-xattrs" => 1,
        "xattrs/extents4" => 0,
        _ => unimplemented!(),
    }
}

/// How many extended attributes with remote storage are present on each file?
fn remote_attrs_per_file(f: &str) -> usize {
    match f {
        "xattrs/local" => 0,
        "xattrs/extents" => 0,
        "xattrs/btree2" => 16,
        "xattrs/btree2.5" => 256,
        "xattrs/btree3" => 512,
        "btree2.with-xattrs" => 0,
        "xattrs/extents4" => 16,
        _ => unimplemented!(),
    }
}

/// How many directory entries are in each directory?
// This is a function of the golden image creation.
fn ents_per_dir_longnames(path: &Path, d: &str) -> usize {
    match (&*path.file_name().unwrap().to_string_lossy(), d) {
        ("xfs1024.img", "leaf") => 256,
        ("xfs1024.img", "node1") => 496,
        ("xfs1024.img", "node3") => 512,
        ("xfs1024.img", "btree2.with-xattrs") => 1024,
        ("xfs1024.img", "btree2.3") => 8192,
        ("xfs1024.img", "btree3") => 131072,
        ("xfsv4.img", "block") => 4,
        ("xfsv4.img", "btree2.2") => 2048,
        ("xfsv4.img", "btree3") => 16384,
        ("xfsv4.img", "btree_with_single_leaf") => 204,
        ("xfsv4.img", "sparse_leaf") => 4,
        ("xfsv4.img", "sparse_btree") => 10,
        ("xfs_noftype.img", "block") => 4,
        ("xfs_4kn.img", "block") => 4,
        ("xfs_4kn.img", "leaf") => 16,
        ("xfs_4kn.img", "node") => 512,
        x => panic!("{:?} not implemented", x),
    }
}

/// How many directory entries are in each directory?
// This is a function of the golden image creation.
fn ents_per_dir_shortnames(path: &Path, d: &str) -> usize {
    match (&*path.file_name().unwrap().to_string_lossy(), d) {
        (_, "sf") => 2,
        ("xfs4096.img", "block") => 32,
        ("xfs4096.img", "leaf") => 384,
        ("xfs4096.img", "node1") => 512,
        ("xfs4096.img", "all_name_lengths") => 255,
        ("xfsv4.img", "leaf") => 128,
        ("xfsv4.img", "node") => 512,
        _ => unimplemented!(),
    }
}

struct Harness {
    d:     TempDir,
    child: Child,
    path:  PathBuf,
}

fn harness(img: &Path) -> Harness {
    let d = tempdir().unwrap();
    let child = Command::cargo_bin("xfs-fuse")
        .unwrap()
        .arg(img)
        .arg(d.path())
        .spawn()
        .unwrap();

    waitfor(Duration::from_secs(5), || {
        let s = nix::sys::statfs::statfs(d.path()).unwrap();
        s.filesystem_type_name() == "fusefs.xfs"
    })
    .unwrap();

    Harness {
        d,
        child,
        path: img.to_owned(),
    }
}

#[fixture]
fn harness1k() -> Harness {
    harness(GOLDEN1K.as_path())
}

#[fixture]
fn harness4k() -> Harness {
    harness(GOLDEN4K.as_path())
}

#[fixture]
fn harness4kn() -> Harness {
    harness(GOLDEN4KN.as_path())
}

#[fixture]
fn harness_preallocated() -> Harness {
    harness(GOLDENPREALLOCATED.as_path())
}

#[fixture]
fn harnessv4() -> Harness {
    harness(GOLDENV4.as_path())
}

#[fixture]
fn harness_noftype() -> Harness {
    harness(GOLDEN_NOFTYPE.as_path())
}

impl Drop for Harness {
    #[allow(clippy::if_same_then_else)]
    fn drop(&mut self) {
        loop {
            let cmd = Command::new("umount").arg(self.d.path()).output();
            match cmd {
                Err(e) => {
                    eprintln!("Executing umount failed: {}", e);
                    if std::thread::panicking() {
                        // Can't double panic
                        return;
                    }
                    panic!("Executing umount failed");
                }
                Ok(output) => {
                    let errmsg = OsString::from_vec(output.stderr).into_string().unwrap();
                    if output.status.success() {
                        break;
                    } else if errmsg.contains("not a file system root directory") {
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

#[template]
#[rstest]
// Leaf directory with > 4 directory blocks
#[case::leaf(harness1k, "leaf")]
// Node directory with a single leaf block
#[case::node1(harness1k, "node1")]
// Node directory with multiple leaf blocks
#[case::node3(harness1k, "node3")]
#[case::btree_2_with_xattrs(harness1k, "btree2.with-xattrs")]
#[case::btree_2_3(harness1k, "btree2.3")]
#[case::btree_3(harness1k, "btree3")]
#[case::v4_block(harnessv4, "block")]
#[case::v4_btree_2_2(harnessv4, "btree2.2")]
#[case::v4_btree_3(harnessv4, "btree3")]
#[case::v4_btree_with_single_leaf(harnessv4, "btree_with_single_leaf")]
#[case::sparse_leaf(harnessv4, "sparse_leaf")]
#[case::sparse_btree(harnessv4, "sparse_btree")]
#[case::noftype_block(harness_noftype, "block")]
#[case::fourkn_block(harness4kn, "block")]
#[case::fourkn_leaf(harness4kn, "leaf")]
#[case::fourkn_node(harness4kn, "node")]
fn all_dir_types_longnames(h: fn() -> Harness, d: &str) {}

// All directory types that have short file names
#[template]
#[rstest]
#[case::sf(harness4k, "sf")]
#[case::block(harness4k, "block")]
#[case::leaf(harness4k, "leaf")]
#[case::v4_sf(harnessv4, "sf")]
#[case::v4_leaf(harnessv4, "leaf")] // TODO check in xfs_db.  Might not be a leaf dir.
#[case::v4_node(harnessv4, "node")]
#[case::noftype_sf(harness_noftype, "sf")]
#[case::fourkn_sf(harness4kn, "sf")]
fn all_dir_types_shortnames(h: fn() -> Harness, d: &str) {}

#[template]
#[rstest]
#[case::local(harness4k, "xattrs/local")]
#[case::extents(harness4k, "xattrs/extents")]
#[case::btree2(harness1k, "xattrs/btree2")]
#[case::btree2_5(harness1k, "xattrs/btree2.5")]
#[case::btree3(harness1k, "xattrs/btree3")]
#[case::btree2_with_xattrs(harness1k, "btree2.with-xattrs")]
#[case::v4_local(harnessv4, "xattrs/local")]
#[case::v4_extents(harnessv4, "xattrs/extents")]
#[case::four4kn_local(harness4kn, "xattrs/local")]
#[case::four4kn_extents(harness4kn, "xattrs/extents4")]
fn all_xattr_fork_types(h: fn() -> Harness, d: &str) {}

#[template]
#[rstest]
#[case::none(harness4k, "files/hello.txt")]
#[case::local(harness4k, "xattrs/local")]
#[case::extents(harness4k, "xattrs/extents")]
#[case::btree2(harness1k, "xattrs/btree2")]
#[case::btree2_5(harness1k, "xattrs/btree2.5")]
#[case::btree3(harness1k, "xattrs/btree3")]
fn all_xattr_fork_types_with_none(h: fn() -> Harness, d: &str) {}

mod close {
    use super::*;

    /// Files can be closed in any order, not just LIFO
    #[named]
    #[rstest]
    fn order(harness4k: Harness) {
        require_fusefs!();

        let path_a = harness4k.d.path().join("files").join("single_extent.txt");
        let path_b = harness4k.d.path().join("files").join("four_extents.txt");

        // First close in LIFO order
        {
            let fa = fs::File::open(&path_a).unwrap();
            let fb = fs::File::open(&path_b).unwrap();
            drop(fb);
            drop(fa);
        }
        // Then close in FIFO order
        {
            let fa = fs::File::open(&path_a).unwrap();
            let fb = fs::File::open(&path_b).unwrap();
            drop(fa);
            drop(fb);
        }

        // Ensure that daemon didn't crash
        access(&path_a, AccessFlags::F_OK).unwrap();
    }
}

/// Mount the image via md(4) and read all its metadata, to verify that we work
/// with devices that require all accesses to be sector size aligned.
mod dev {
    use super::*;

    struct MdHarness {
        _md:   mdconfig::Md,
        d:     TempDir,
        child: Child,
    }

    fn mdharness(image: &Path, sectorsize: u32) -> MdHarness {
        let md = mdconfig::Builder::vnode(image)
            .sectorsize(sectorsize)
            .create()
            .unwrap();
        let d = tempdir().unwrap();
        let child = Command::cargo_bin("xfs-fuse")
            .unwrap()
            .arg(md.path())
            .arg(d.path())
            .spawn()
            .unwrap();

        waitfor(Duration::from_secs(5), || {
            let s = nix::sys::statfs::statfs(d.path()).unwrap();
            s.filesystem_type_name() == "fusefs.xfs"
        })
        .unwrap();

        MdHarness { _md: md, d, child }
    }

    impl Drop for MdHarness {
        fn drop(&mut self) {
            loop {
                let cmd = Command::new("umount").arg(self.d.path()).output();
                match cmd {
                    Err(e) => {
                        panic!("Executing umount failed: {}", e);
                    }
                    Ok(output) => {
                        let errmsg = OsString::from_vec(output.stderr).into_string().unwrap();
                        if output.status.success() {
                            break;
                        } else if errmsg.contains("not a file system root directory") {
                            // The daemon probably crashed.
                            break;
                        } else if errmsg.contains("Device busy") {
                            println!("{}", errmsg);
                        } else {
                            panic!("{}", errmsg);
                        }
                    }
                }
                sleep(Duration::from_millis(50));
            }
            let _ = self.child.wait();
        }
    }

    /// Read all metadata from the file system to verify read alignment.
    // Regression test for https://github.com/KhaledEmaraDev/xfuse/issues/15
    // TODO: Read all data as well as metadata
    #[named]
    #[rstest]
    #[case::fourk(GOLDEN4K.as_path(), 512)]
    #[case::fourkn(GOLDEN4KN.as_path(), 4096)]
    #[case::onek(GOLDEN1K.as_path(), 512)]
    #[case::v4(GOLDENV4.as_path(), 512)]
    #[case::no_ftype(GOLDEN_NOFTYPE.as_path(), 512)]
    #[case::preallocated(GOLDENPREALLOCATED.as_path(), 512)]
    fn metadata(#[case] image: &Path, #[case] sectorsize: u32) {
        require_fusefs!();
        require_root!();
        let h = mdharness(image, sectorsize);

        let walker = walkdir::WalkDir::new(h.d.path()).into_iter();
        for entry in walker {
            let entry = entry.unwrap();
            let _ = entry.metadata().unwrap();
            // xattrs are not supported on fifo devices.  This limitation it
            // outside of fusefs.
            if entry.path().file_name() != Some(OsStr::new("fifo")) {
                for attr in xattr::list(entry.path()).unwrap() {
                    xattr::get(entry.path(), attr).unwrap().unwrap();
                }
            }
        }
    }

    #[named]
    #[rstest]
    #[case::large_extent(GOLDEN4K.as_path(), 512, "large_extent.txt", 8448)]
    #[case::partial_extent(GOLDEN4K.as_path(), 512, "partial_extent.txt", 8448)]
    #[case::single_extent(GOLDEN4K.as_path(), 512, "single_extent.txt", 4096)]
    #[case::four_extents(GOLDEN4K.as_path(), 512, "four_extents.txt", 16384)]
    #[case::two_height_btree(GOLDEN4K.as_path(), 512, "btree2.txt", 65536)]
    fn data(
        #[case] image: &Path,
        #[case] sectorsize: u32,
        #[case] file: &str,
        #[case] size: usize,
    ) {
        require_fusefs!();
        require_root!();
        let h = mdharness(image, sectorsize);

        let path = h.d.path().join("files").join(file);
        let mut buf = vec![0; size];
        let mut f = fs::File::open(path).unwrap();
        f.read_exact(&mut buf[..]).unwrap();
    }

    /// read a whole file 128 bytes at a time, using direct_io to bypass the cache
    #[named]
    #[rstest]
    #[case::single_extent(GOLDEN4K.as_path(), 512, "single_extent.txt", 4096)]
    #[case::four_extents(GOLDEN4K.as_path(), 512, "four_extents.txt", 16384)]
    fn o_direct(
        #[case] image: &Path,
        #[case] sectorsize: u32,
        #[case] filename: &str,
        #[case] size: usize,
    ) {
        require_fusefs!();
        require_root!();
        let h = mdharness(image, sectorsize);

        const BUFSIZE: usize = 16;
        let path = h.d.path().join("files").join(filename);
        let mut f = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECT)
            .open(path)
            .unwrap();

        // Verify contents
        let mut ofs = 0;
        while ofs < size {
            let mut buf = [0; BUFSIZE];
            if let Err(e) = f.read_exact(&mut buf[..]) {
                if e.kind() == ErrorKind::UnexpectedEof {
                    break;
                } else {
                    panic!("read: {:?}", e);
                }
            } else {
                let expected = format!("{:016x}", ofs);
                assert_eq!(&buf[..], expected.as_bytes());
                ofs += BUFSIZE;
            }
        }
        assert_eq!(ofs, size);
    }
}

// TODO: xattr test on V4 file system
mod getextattr {
    use super::*;

    #[named]
    #[apply(all_xattr_fork_types)]
    fn ok(#[case] h: fn() -> Harness, #[case] d: &str) {
        require_fusefs!();

        let harness = h();
        let p = harness.d.path().join(d);

        for attr in expected_xattrs_per_file(d) {
            let binary_value = xattr::get(&p, attr.name.as_os_str()).unwrap().unwrap();
            let value = OsStr::from_bytes(&binary_value[..]);
            assert_eq!(attr.value, value);
        }
    }

    /// Try to get the value of an extended attribute that doesn't exist.
    // This test is freebsd-specific because the relevant syscall is.  It could
    // be implemented for Linux too, but I haven't done so.
    #[cfg(target_os = "freebsd")]
    #[named]
    #[apply(all_xattr_fork_types_with_none)]
    fn enoattr(#[case] h: fn() -> Harness, #[case] d: &str) {
        use std::ffi::CString;

        require_fusefs!();

        let harness = h();
        let ns = libc::EXTATTR_NAMESPACE_USER;
        let p = harness.d.path().join(d);
        let cpath = CString::new(p.as_os_str().as_bytes()).unwrap();
        let attrname = OsStr::new("user.nonexistent");
        let cattrname = CString::new(attrname.as_bytes()).unwrap();
        let mut v = Vec::<u8>::with_capacity(80);
        let r = unsafe {
            libc::extattr_get_file(
                cpath.as_ptr(),
                ns,
                cattrname.as_ptr(),
                v.as_mut_ptr().cast(),
                v.capacity(),
            )
        };
        assert!(r < 0);
        assert_eq!(
            libc::ENOATTR,
            io::Error::last_os_error().raw_os_error().unwrap()
        );
    }

    /// Try to get the size of an extended attribute that doesn't exist.
    // This test is freebsd-specific because the relevant syscall is.  It could
    // be implemented for Linux too, but I haven't done so.
    #[cfg(target_os = "freebsd")]
    #[named]
    #[apply(all_xattr_fork_types_with_none)]
    fn enoattr_size(#[case] h: fn() -> Harness, #[case] d: &str) {
        use std::{ffi::CString, ptr};

        require_fusefs!();

        let harness = h();
        let ns = libc::EXTATTR_NAMESPACE_USER;
        let p = harness.d.path().join(d);
        let cpath = CString::new(p.as_os_str().as_bytes()).unwrap();
        let attrname = OsStr::new("user.nonexistent");
        let cattrname = CString::new(attrname.as_bytes()).unwrap();
        let r = unsafe {
            libc::extattr_get_file(cpath.as_ptr(), ns, cattrname.as_ptr(), ptr::null_mut(), 0)
        };
        assert!(r < 0);
        assert_eq!(
            libc::ENOATTR,
            io::Error::last_os_error().raw_os_error().unwrap()
        );
    }
}

/// Lookup the size of an extended attribute without fetching it.
// This test is freebsd-specific because the relevant syscall is.  It could be
// implemented for Linux too, but I haven't done so.
#[cfg(target_os = "freebsd")]
#[named]
#[apply(all_xattr_fork_types)]
fn getextattr_size(#[case] h: fn() -> Harness, #[case] d: &str) {
    use std::{ffi::CString, ptr};

    require_fusefs!();

    let harness = h();
    let ns = libc::EXTATTR_NAMESPACE_USER;
    let p = harness.d.path().join(d);
    let expected_len = "value.000000".len();
    let cpath = CString::new(p.as_os_str().as_bytes()).unwrap();

    for i in 0..local_attrs_per_file(d) {
        let s = format!("attr.{:06}", i);
        let attrname = OsStr::new(s.as_str());
        let cattrname = CString::new(attrname.as_bytes()).unwrap();
        let r = unsafe {
            libc::extattr_get_file(cpath.as_ptr(), ns, cattrname.as_ptr(), ptr::null_mut(), 0)
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
fn hardlink(harness4k: Harness) {
    require_fusefs!();

    let path1 = harness4k.d.path().join("files").join("hello.txt");
    let path2 = harness4k.d.path().join("files").join("hello2.txt");

    let stat1 = nix::sys::stat::stat(&path1).unwrap();
    let stat2 = nix::sys::stat::stat(&path2).unwrap();
    assert_eq!(stat1, stat2);
}

/// Mount and unmount the golden image
#[named]
#[rstest]
#[case::fourk(harness4k)]
#[case::onek(harness1k)]
#[case::v4(harnessv4)]
#[case::fourkn(harness4kn)]
fn mount(#[case] h: fn() -> Harness) {
    require_fusefs!();

    let harness = h();
    drop(harness);
}

mod lookup {
    use super::*;

    #[named]
    #[rstest]
    fn all_name_lengths(harness4k: Harness) {
        require_fusefs!();

        let amode = AccessFlags::F_OK;
        for i in 1..=255 {
            let p = harness4k.d.path().join("all_name_lengths").join(format!(
                "{:0width$}",
                i,
                width = i
            ));
            access(p.as_path(), amode).unwrap_or_else(|_| panic!("Lookup failed: {}", p.display()));
        }
    }

    /// A block directory with hash collisions
    #[rstest]
    #[named]
    fn hash_collisions_block(harness4k: Harness) {
        require_fusefs!();

        let filenames = [
            ".", "..", "210001", "2a0004", "310009", "81000a", "210004", "2a0001", "3a0009",
            "81000d", "210005", "2a0000", "3a0008", "81000e", "210011", "2a0014", "310019",
            "81001a", "210014", "2a0011", "3a0019", "81001d", "210015", "2a0010", "3a0018",
            "81001e", "210021", "2a0024", "310029", "81002a", "210024", "2a0021", "3a0029",
            "81002d", "210025", "2a0020", "3a0028", "81002e", "210031", "2a0034", "310039",
            "81003a",
        ];
        let amode = AccessFlags::F_OK;
        for filename in &filenames {
            let p = harness4k
                .d
                .path()
                .join("block-with-hash-collisions")
                .join(filename);
            access(p.as_path(), amode).unwrap_or_else(|_| panic!("Lookup failed: {}", p.display()));
        }
    }

    /// Lookup all entries in a directory
    //
    // In the 1k blocksize golden image, they use a different naming convention.
    #[named]
    #[apply(all_dir_types_longnames)]
    fn longnames(#[case] h: fn() -> Harness, #[case] d: &str) {
        require_fusefs!();

        let harness = h();
        let amode = AccessFlags::F_OK;
        for i in 0..ents_per_dir_longnames(harness.path.as_path(), d) {
            let p = harness.d.path().join(format!("{d}/frame__________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________{i:08}"));
            access(p.as_path(), amode).unwrap_or_else(|_| panic!("Lookup failed: {}", p.display()));
        }
    }

    /// Lookup all entries in a directory
    #[named]
    #[apply(all_dir_types_shortnames)]
    fn shortnames(#[case] h: fn() -> Harness, #[case] d: &str) {
        require_fusefs!();

        let harness = h();
        let amode = AccessFlags::F_OK;
        for i in 0..ents_per_dir_shortnames(harness.path.as_path(), d) {
            let p = harness.d.path().join(format!("{d}/frame{i:06}"));
            access(p.as_path(), amode).unwrap_or_else(|_| panic!("Lookup failed: {}", p.display()));
        }
    }

    /// Lookup a directory's "." and ".." entries.  Verify their inode numbers
    #[named]
    #[rstest]
    // Annoyingly rstest_reuse's "apply" attribute cannot be used more than once.  So we must copy
    // the case definitions from all_dir_types_longnames and all_dir_types_shortnames
    #[case::leaf(harness1k, "leaf")]
    #[case::node1(harness1k, "node1")]
    #[case::node3(harness1k, "node3")]
    #[case::btree_2_with_xattrs(harness1k, "btree2.with-xattrs")]
    #[case::btree_2_3(harness1k, "btree2.3")]
    #[case::btree_3(harness1k, "btree3")]
    #[case::v4_block(harnessv4, "block")]
    #[case::v4_btree_2_2(harnessv4, "btree2.2")]
    #[case::v4_btree_3(harnessv4, "btree3")]
    #[case::v4_btree_with_single_leaf(harnessv4, "btree_with_single_leaf")]
    #[case::sparse_leaf(harnessv4, "sparse_leaf")]
    #[case::sparse_btree(harnessv4, "sparse_btree")]
    #[case::sf(harness4k, "sf")]
    #[case::block(harness4k, "block")]
    #[case::leaf(harness4k, "leaf")]
    #[case::v4_sf(harnessv4, "sf")]
    #[case::v4_leaf(harnessv4, "leaf")]
    #[case::v4_node(harnessv4, "node")]
    #[case::noftype_sf(harness_noftype, "sf")]
    #[case::noftype_block(harness_noftype, "block")]
    #[case::fourkn_sf(harness4kn, "sf")]
    #[case::fourkn_block(harness4kn, "block")]
    #[case::fourkn_leaf(harness4kn, "leaf")]
    #[case::fourkn_node(harness4kn, "node")]
    fn dots(#[case] h: fn() -> Harness, #[case] d: &str) {
        require_fusefs!();

        let harness = h();
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
    #[rstest]
    #[case::sf(harness4k, "sf")]
    #[case::block(harness4k, "block")]
    #[case::leaf(harness4k, "leaf")]
    #[case::btree(harness4k, "btree")]
    #[case::btree2_3(harness1k, "btree2.3")]
    #[case::btree3(harness1k, "btree3")]
    #[case::v4_sf(harness1k, "sf")]
    #[case::v4_blocks(harness1k, "blocks")]
    fn enoent(#[case] h: fn() -> Harness, #[case] d: &str) {
        require_fusefs!();

        let harness = h();
        let p = harness.d.path().join(format!("{d}/nonexistent"));
        let e = access(p.as_path(), AccessFlags::F_OK).unwrap_err();
        assert_eq!(e, nix::Error::ENOENT);
    }
}

mod lseek {
    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Offset {
        FromStart(libc::off_t),
        Eof,
        BeyondEof,
    }

    #[template]
    #[rstest]
    /// Already at EOF
    #[case::eof_data(Whence::SeekData, Offset::Eof, Err(Errno::ENXIO))]
    /// The virtual hole at EOF
    #[case::eof_hole(Whence::SeekHole, Offset::Eof, Ok(Offset::Eof))]
    /// Seeking for hole beyond EOF
    #[case::beyond_eof_hole(Whence::SeekHole, Offset::BeyondEof, Err(Errno::ENXIO))]
    /// Seek for data
    #[case::data_at_start(Whence::SeekData, Offset::FromStart(0), Ok(Offset::FromStart(4096)))]
    #[case::data_in_middle(
        Whence::SeekData,
        Offset::FromStart(8192),
        Ok(Offset::FromStart(12288))
    )]
    /// Seek for a hole
    #[case::hole_in_middle(Whence::SeekHole, Offset::FromStart(4096), Ok(Offset::FromStart(8192)))]
    /// Try to seek to a data region, but we're already there.
    #[case::data_in_data_at_start(
        Whence::SeekData,
        Offset::FromStart(4096),
        Ok(Offset::FromStart(4096))
    )]
    #[case::data_in_data_in_middle(
        Whence::SeekData,
        Offset::FromStart(12288),
        Ok(Offset::FromStart(12288))
    )]
    /// Try to seek to a hole, but it's only data until EOF
    #[case::data_until_eof(Whence::SeekHole, Offset::FromStart(12288), Ok(Offset::Eof))]
    /// Try to seek to a hole, but we're already there.
    #[case::hole_in_hole_at_start(Whence::SeekHole, Offset::FromStart(0), Ok(Offset::FromStart(0)))]
    #[case::hole_in_hole_in_middle(
        Whence::SeekHole,
        Offset::FromStart(8192),
        Ok(Offset::FromStart(8192))
    )]
    /// Searching from a negative offset always returns EINVAL
    #[case::negative_offset(Whence::SeekHole, Offset::FromStart(-1), Err(Errno::EINVAL))]
    fn all_scenarios(
        #[case] whence: Whence,
        #[case] ofs: Offset,
        #[case] expected: nix::Result<Offset>,
    ) {
    }

    #[named]
    #[apply(all_scenarios)]
    fn scenarios(
        harness4k: Harness,
        #[values("sparse.extents.txt", "sparse.btree.txt")] path: &str,
        whence: Whence,
        ofs: Offset,
        expected: nix::Result<Offset>,
    ) {
        require_fusefs!();

        let p = harness4k.d.path().join("files").join(path);
        let f = fs::File::open(p).unwrap();
        let expected = match expected {
            Ok(Offset::FromStart(ofs)) => Ok(ofs),
            Ok(Offset::Eof) => Ok(f.metadata().unwrap().size() as libc::off_t),
            Ok(Offset::BeyondEof) => unreachable!(),
            Err(e) => Err(e),
        };
        let ofs = match ofs {
            Offset::FromStart(ofs) => ofs,
            Offset::Eof => f.metadata().unwrap().size() as libc::off_t,
            Offset::BeyondEof => f.metadata().unwrap().size() as libc::off_t + 1,
        };
        assert_eq!(expected, nix::unistd::lseek(f.as_raw_fd(), ofs, whence));
    }

    /// A completely sparse file has no data regions
    #[named]
    #[rstest]
    fn fully_sparse(harness4k: Harness) {
        require_fusefs!();

        let p = harness4k.d.path().join("files/sparse.fully.txt");
        let f = fs::File::open(p).unwrap();
        assert_eq!(
            Err(Errno::ENXIO),
            nix::unistd::lseek(f.as_raw_fd(), 0, Whence::SeekData)
        );
    }

    /// Try to seek to a data region, but it's only hole untiL EOF
    #[named]
    #[rstest]
    #[case::extents("hole_at_end.extents.txt", 16384)]
    #[case::btree("hole_at_end.btree.txt", 65536)]
    fn hole_at_end(harness4k: Harness, #[case] fname: &str, #[case] offset: i64) {
        require_fusefs!();

        let p = harness4k.d.path().join("files").join(fname);
        let f = fs::File::open(p).unwrap();
        assert_eq!(
            Err(Errno::ENXIO),
            nix::unistd::lseek(f.as_raw_fd(), offset, Whence::SeekData)
        );
    }

    #[named]
    #[rstest]
    #[cfg(any(target_os = "freebsd", target_os = "illumos", target_os = "netbsd"))]
    fn pathconf(harness4k: Harness) {
        require_fusefs!();

        let p = harness4k.d.path().join("files/hello.txt");

        let f = fs::File::open(p).unwrap();
        // We have to use FFI directly until Nix 0.29.0 is released.
        // https://github.com/nix-rust/nix/pull/2349
        // pathconf is always safe, as long as the path is valid
        let raw = unsafe {
            nix::errno::Errno::clear();
            libc::fpathconf(f.as_raw_fd(), libc::_PC_MIN_HOLE_SIZE)
        };
        if raw == -1 {
            let e = io::Error::last_os_error();
            if e.raw_os_error().is_some() {
                panic!("{}", e);
            } else {
                panic!("_PC_MIN_HOLE_SIZE not supported");
            }
        } else {
            // The FUSE protocol doesn't give the server any way to tell the kernel its minimum
            // hole size, so all the kernel can report to userland is "1" or "not supported".
            // "1" means "The file system does not specify the minimum".
            assert_eq!(1, raw);
        }
    }

    #[named]
    #[rstest]
    fn preallocated(harness_preallocated: Harness) {
        require_fusefs!();

        let p = harness_preallocated.d.path().join("files/preallocated");
        let f = fs::File::open(p).unwrap();

        // The unwritten extent should not count as a data region
        assert_eq!(
            Err(Errno::ENXIO),
            nix::unistd::lseek(f.as_raw_fd(), 0, Whence::SeekData)
        );

        // The unwritten extent should count as a hole
        assert_eq!(
            Ok(0),
            nix::unistd::lseek(f.as_raw_fd(), 0, Whence::SeekHole)
        );
    }
}

mod lsextattr {
    use super::*;

    #[named]
    #[apply(all_xattr_fork_types)]
    fn ok(#[case] h: fn() -> Harness, #[case] d: &str) {
        require_fusefs!();

        let harness = h();
        let p = harness.d.path().join(d);

        let mut all_attrnames = xattr::list(p).unwrap().collect::<Vec<_>>();
        all_attrnames.sort_unstable();
        assert_eq!(expected_xattrs_per_file(d).count(), all_attrnames.len());
        for (expected, actual) in std::iter::zip(expected_xattrs_per_file(d), all_attrnames) {
            assert_eq!(expected.name, actual);
        }
    }

    #[named]
    #[rstest]
    fn empty(harness4k: Harness) {
        use std::ffi::CString;
        require_fusefs!();

        let ns = libc::EXTATTR_NAMESPACE_USER;
        let p = harness4k.d.path().join("files/hello.txt");
        let cpath = CString::new(p.as_os_str().as_bytes()).unwrap();
        let mut v = Vec::<u8>::with_capacity(1024);

        let r = unsafe {
            libc::extattr_list_file(cpath.as_ptr(), ns, v.as_mut_ptr().cast(), v.capacity())
        };
        if let Ok(r) = usize::try_from(r) {
            assert_eq!(0, r);
        } else {
            panic!("{}", io::Error::last_os_error());
        }
    }

    #[named]
    #[rstest]
    fn empty_size(harness4k: Harness) {
        use std::{ffi::CString, ptr};
        require_fusefs!();

        let ns = libc::EXTATTR_NAMESPACE_USER;
        let p = harness4k.d.path().join("files/hello.txt");
        let cpath = CString::new(p.as_os_str().as_bytes()).unwrap();

        let r = unsafe { libc::extattr_list_file(cpath.as_ptr(), ns, ptr::null_mut(), 0) };
        if let Ok(r) = usize::try_from(r) {
            assert_eq!(0, r);
        } else {
            panic!("{}", io::Error::last_os_error());
        }
    }

    /// Lookup the size of the extended attribute list of a file, without
    /// fetching it.
    // This test is freebsd-specific because the relevant syscall is.  It could
    // be implemented for Linux too, but I haven't done so.
    #[cfg(target_os = "freebsd")]
    #[named]
    #[apply(all_xattr_fork_types)]
    fn size(#[case] h: fn() -> Harness, #[case] d: &str) {
        use std::{ffi::CString, ptr};
        require_fusefs!();

        let harness = h();
        let ns = libc::EXTATTR_NAMESPACE_USER;
        let p = harness.d.path().join(d);
        let expected_len: usize = expected_xattrs_per_file(d)
            .map(|attr| {
                attr.name.len() /* -5 because "user." is not included*/ - 5 /* +1 for NUL */ + 1
            })
            .sum();
        let cpath = CString::new(p.as_os_str().as_bytes()).unwrap();

        let r = unsafe { libc::extattr_list_file(cpath.as_ptr(), ns, ptr::null_mut(), 0) };
        if let Ok(r) = usize::try_from(r) {
            assert_eq!(expected_len, r);
        } else {
            panic!("{}", io::Error::last_os_error());
        }
    }
}

mod open {
    use super::*;

    /// Files can be opened multiple times
    #[named]
    #[rstest]
    fn multiple(harness4k: Harness) {
        require_fusefs!();

        let path = harness4k.d.path().join("files").join("executable");

        let _f1 = fs::File::open(&path).unwrap();
        // Open it again with a different mode.  This forces fusefs(4) to send a
        // separate FUSE_OPEN request.
        let _f2 = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_EXEC)
            .open(&path)
            .unwrap();
    }

    /// Try to open two reflinked files at the same time
    #[named]
    #[rstest]
    fn reflinks(harness4k: Harness) {
        require_fusefs!();

        let path_a = harness4k.d.path().join("files").join("reflink_a.txt");
        let path_b = harness4k.d.path().join("files").join("reflink_b.txt");

        let fa = fs::File::open(&path_a).unwrap();
        let fb = fs::File::open(path_b).unwrap();

        drop(fa);
        drop(fb);

        // Ensure that daemon didn't crash
        access(&path_a, AccessFlags::F_OK).unwrap();
    }
}

mod pathconf {
    use super::*;

    #[named]
    #[rstest]
    fn name_max(harness4k: Harness) {
        require_fusefs!();

        let var = nix::unistd::PathconfVar::NAME_MAX;
        let r = nix::unistd::pathconf(harness4k.d.path(), var).unwrap();
        assert_eq!(Some(255), r);
    }
}

mod read {
    use super::*;

    #[template]
    #[rstest]
    #[case::large_extent_4k(harness4k, "large_extent.txt", 1048576)]
    #[case::large_extent_1k(harness1k, "large_extent.txt", 1048576)]
    #[case::large_extent_v4(harnessv4, "large_extent.txt", 1048576)]
    #[case::partial_extent(harness4k, "partial_extent.txt", 8448)]
    #[case::single_extent(harness4k, "single_extent.txt", 4096)]
    #[case::four_extents(harness4k, "four_extents.txt", 16384)]
    #[case::two_height_btree(harness4k, "btree2.txt", 65536)]
    #[case::wide_two_height_btree(harness4k, "btree2.4.txt", 8388608)]
    #[case::three_height_btree(harness4k, "btree3.txt", 16777216)]
    #[case::wide_two_height_btree2(harness1k, "btree2.2.txt", 65536)]
    #[case::wide_two_height_btree2(harness1k, "btree3.txt", 2097152)]
    #[case::wide_two_height_btree2(harness1k, "btree3.3.txt", 8388608)]
    #[case::wide_two_height_btree2_v4(harnessv4, "btree2.2.txt", 32768)]
    #[case::wide_two_height_btree2_v4(harnessv4, "btree3.txt", 1048576)]
    #[case::wide_two_height_btree2_v4(harnessv4, "btree3.3.txt", 4194304)]
    #[case::btree_with_xattr(harness1k, "btree2_with_xattrs.txt", 65536)]
    #[case::reflink_a(harness4k, "reflink_a.txt", 16384)]
    #[case::reflink_b(harness4k, "reflink_b.txt", 16384)]
    #[case::reflink_partial(harness4k, "reflink_partial.txt", 16384)]
    fn all_files(h: fn() -> Harness, d: &str) {}

    /// Attempting to read across eof should return the correct amount of data
    #[named]
    #[apply(all_files)]
    fn across_eof(#[case] h: fn() -> Harness, #[case] filename: &str, #[case] size: usize) {
        require_fusefs!();

        let harness = h();
        let path = harness.d.path().join("files").join(filename);
        let f = fs::File::open(path).unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(1, f.read_at(&mut buf[..], size as u64 - 1).unwrap());
    }

    /// Read a whole file in a single syscall
    #[named]
    #[apply(all_files)]
    fn all(#[case] h: fn() -> Harness, #[case] filename: &str, #[case] size: usize) {
        require_fusefs!();

        let harness = h();
        let path = harness.d.path().join("files").join(filename);
        let mut buf = vec![0; size];
        let mut f = fs::File::open(path).unwrap();
        f.read_exact(&mut buf[..]).unwrap();

        // Verify contents
        let mut ofs = 0;
        while ofs < size {
            let expected = format!("{:016x}", ofs);
            assert_eq!(&buf[ofs..ofs + 16], expected.as_bytes());
            ofs += 16;
        }
    }

    /// Read a whole file 16 bytes at a time
    // XXX Even though read(2) only reads 16 bytes at a time, in-kernel
    // buffering may result in different read sizes at the fuse daemon.  We
    // should parameterize this test on all different cacheing types.
    #[named]
    #[apply(all_files)]
    fn by16(#[case] h: fn() -> Harness, #[case] filename: &str, #[case] size: usize) {
        require_fusefs!();

        const BUFSIZE: usize = 16;
        let harness = h();
        let path = harness.d.path().join("files").join(filename);
        let mut f = fs::File::open(path).unwrap();

        // Verify contents
        let mut ofs = 0;
        while ofs < size {
            let mut buf = [0; BUFSIZE];
            if let Err(e) = f.read_exact(&mut buf[..]) {
                if e.kind() == ErrorKind::UnexpectedEof {
                    break;
                } else {
                    panic!("read: {:?}", e);
                }
            } else {
                let expected = format!("{:016x}", ofs);
                assert_eq!(&buf[..], expected.as_bytes());
                ofs += BUFSIZE;
            }
        }
        assert_eq!(ofs, size);
    }

    /// Attempt to read past eof should return 0
    #[named]
    #[apply(all_files)]
    fn past_eof(#[case] h: fn() -> Harness, #[case] filename: &str, #[case] size: usize) {
        require_fusefs!();

        let harness = h();
        let path = harness.d.path().join("files").join(filename);
        let f = fs::File::open(path).unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(0, f.read_at(&mut buf[..], size as u64 + 1).unwrap());
    }

    /// Read a sparse file.
    #[named]
    #[rstest]
    #[case("sparse.fully.txt", false)]
    #[case("sparse.extents.txt", true)]
    #[case("sparse.btree.txt", true)]
    fn sparse(harness4k: Harness, #[case] filename: &str, #[case] sector1: bool) {
        require_fusefs!();

        const BUFSIZE: usize = 16;
        let path = harness4k.d.path().join("files").join(filename);
        let mut buf = vec![0; 4096];
        let mut f = fs::File::open(path).unwrap();

        // First read a sparse block
        f.read_exact(&mut buf[..]).unwrap();
        assert_eq!(&vec![0; 4096], &buf);

        // Then read a dense block
        if sector1 {
            f.read_exact(&mut buf[..]).unwrap();
            let mut ofs = 4096;
            while ofs < 8192 {
                let expected = format!("{:016x}", ofs);
                let bofs = ofs - 4096;
                assert_eq!(&buf[bofs..bofs + BUFSIZE], expected.as_bytes());
                ofs += BUFSIZE;
            }
        }
    }

    #[named]
    #[rstest]
    #[case::extents("hole_at_end.extents.txt", 16384)]
    #[case::btree("hole_at_end.btree.txt", 65536)]
    fn hole_at_end(harness4k: Harness, #[case] fname: &str, #[case] offset: usize) {
        require_fusefs!();

        const BUFSIZE: usize = 16;
        let path = harness4k.d.path().join("files").join(fname);
        let mut f = fs::File::open(path).unwrap();

        // First read the dense parts
        let mut buf = vec![0; offset];
        f.read_exact(&mut buf[..]).unwrap();
        let mut ofs = 0;
        while ofs < offset {
            let expected = format!("{:016x}", ofs);
            assert_eq!(&buf[ofs..ofs + BUFSIZE], expected.as_bytes());
            ofs += BUFSIZE;
        }

        // Now read the hole
        let mut buf = vec![0; 4096];
        f.read_exact(&mut buf[..]).unwrap();
        assert_eq!(&vec![0; 4096], &buf);
    }
    // TODO: add a test case for reading with direct I/O where the image is on a
    // device, not a file

    /// Test reading a file that has been preallocated but unwritten, for example with
    /// posix_fallocate
    #[named]
    #[rstest]
    fn unwritten(harness_preallocated: Harness) {
        require_fusefs!();

        const FLEN: u64 = 8388608;
        const BUFLEN: usize = 1 << 12;

        let path = harness_preallocated
            .d
            .path()
            .join("files")
            .join("preallocated");
        let mut f = fs::File::open(path).unwrap();

        // First verify the length
        assert_eq!(f.metadata().unwrap().size(), FLEN);

        // Then verify that we read all zeros
        let zbuf = vec![0; BUFLEN];
        let mut buf = vec![0; BUFLEN];
        for _ in 0..(FLEN / BUFLEN as u64) {
            f.read_exact(&mut buf[..]).unwrap();
            assert_eq!(zbuf, buf, "Read garbage where there should've been zeros");
        }
    }
}

mod readdir {
    use super::*;

    #[named]
    #[rstest]
    fn all_name_lengths(harness4k: Harness) {
        require_fusefs!();

        let d = "all_name_lengths";
        let dpath = harness4k.d.path().join(d);
        let ents = std::fs::read_dir(dpath).unwrap();
        let mut count = 0;
        for (i, rent) in ents.into_iter().enumerate() {
            let ent = rent.unwrap();
            let expected_name = format!("{:0width$}", i + 1, width = i + 1);
            assert_eq!(ent.file_name(), OsStr::new(&expected_name));
            assert!(ent.file_type().unwrap().is_file());
            let md = ent.metadata().unwrap();
            assert_eq!(
                ent.ino(),
                md.ino(),
                "inode mismatch for {}: readdir returned {} but lookup returned {}",
                expected_name,
                ent.ino(),
                md.ino()
            );
            // The other metadata fields are checked in a separate test case.
            count += 1;
        }
        assert_eq!(count, ents_per_dir_shortnames(harness4k.path.as_path(), d));
    }

    /// A block directory with hash collisions
    #[rstest]
    #[named]
    fn hash_collisions_block(harness4k: Harness) {
        require_fusefs!();

        let dpath = harness4k.d.path().join("block-with-hash-collisions");
        let ents = std::fs::read_dir(dpath).unwrap();
        let mut count = 0;
        for rent in ents {
            let ent = rent.unwrap();
            assert!(ent.file_type().unwrap().is_file());
            let md = ent.metadata().unwrap();
            assert_eq!(
                ent.ino(),
                md.ino(),
                "inode mismatch for {}: readdir returned {} but lookup returned {}",
                ent.file_name().to_string_lossy(),
                ent.ino(),
                md.ino()
            );
            count += 1;
        }
        assert_eq!(count, 40);
    }

    /// List a directory's contents with readdir
    //
    // The 1k blocksize formatted golden image uses a different naming convention than the 4k image
    #[named]
    #[apply(all_dir_types_longnames)]
    fn longnames(#[case] h: fn() -> Harness, #[case] d: &str) {
        require_fusefs!();

        let harness = h();
        let dpath = harness.d.path().join(d);
        let ents = std::fs::read_dir(dpath).unwrap();
        let mut count = 0;
        for (i, rent) in ents.enumerate() {
            let ent = rent.unwrap();
            let expected_name = format!("frame__________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________{i:08}");
            assert_eq!(ent.file_name(), OsStr::new(&expected_name));
            assert!(ent.file_type().unwrap().is_file());
            let md = ent.metadata().unwrap();
            assert_eq!(
                ent.ino(),
                md.ino(),
                "inode mismatch for {}: readdir returned {} but lookup returned {}",
                expected_name,
                ent.ino(),
                md.ino()
            );
            // The other metadata fields are checked in a separate test case.
            count += 1;
        }
        assert_eq!(count, ents_per_dir_longnames(harness.path.as_path(), d));
    }

    /// List a directory's contents with readdir
    #[named]
    #[apply(all_dir_types_shortnames)]
    fn shortnames(#[case] h: fn() -> Harness, #[case] d: &str) {
        require_fusefs!();

        let harness = h();
        let dpath = harness.d.path().join(d);
        let ents = std::fs::read_dir(dpath).unwrap();
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
        assert_eq!(count, ents_per_dir_shortnames(harness.path.as_path(), d));
    }

    /// List a directory's hidden contents with readdir
    // Use Nix::dir::Dir instead of std::fs::read_dir, because the latter
    // unconditionally hides the hidden entries.
    #[named]
    #[rstest]
    // Annoyingly rstest_reuse's "apply" attribute cannot be used more than once.  So we must copy
    // the case definitions from all_dir_types_longnames and all_dir_types_shortnames
    #[case::leaf(harness1k, "leaf")]
    #[case::node1(harness1k, "node1")]
    #[case::node3(harness1k, "node3")]
    #[case::btree_2_with_xattrs(harness1k, "btree2.with-xattrs")]
    #[case::btree_2_3(harness1k, "btree2.3")]
    #[case::btree_3(harness1k, "btree3")]
    #[case::v4_block(harnessv4, "block")]
    #[case::v4_btree_2_2(harnessv4, "btree2.2")]
    #[case::v4_btree_3(harnessv4, "btree3")]
    #[case::v4_btree_with_single_leaf(harnessv4, "btree_with_single_leaf")]
    #[case::sparse_leaf(harnessv4, "sparse_leaf")]
    #[case::sparse_btree(harnessv4, "sparse_btree")]
    #[case::sf(harness4k, "sf")]
    #[case::block(harness4k, "block")]
    #[case::leaf(harness4k, "leaf")]
    #[case::v4_sf(harnessv4, "sf")]
    #[case::v4_leaf(harnessv4, "leaf")]
    #[case::v4_node(harnessv4, "node")]
    #[case::noftype_sf(harness_noftype, "sf")]
    #[case::noftype_block(harness_noftype, "block")]
    #[case::fourkn_sf(harness4kn, "sf")]
    #[case::fourkn_block(harness4kn, "block")]
    #[case::fourkn_leaf(harness4kn, "leaf")]
    #[case::fourkn_node(harness4kn, "node")]
    fn dots(#[case] h: fn() -> Harness, #[case] d: &str) {
        use nix::{dir::Dir, fcntl::OFlag, sys::stat::Mode};
        require_fusefs!();

        let harness = h();
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
}

#[named]
#[rstest]
#[case::sf("sf", "dest")]
#[case::extent("max", "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDE")]
fn readlink(
    #[values(harness4k, harnessv4)] h: fn() -> Harness,
    #[case] linkname: &str,
    #[case] destname: &str,
) {
    require_fusefs!();

    let harness = h();
    let path = harness.d.path().join("links").join(linkname);
    let dest = fs::read_link(path).unwrap();
    assert_eq!(dest.as_os_str(), destname);
}

mod stat {
    use super::*;

    /// Verify all of an inode's metadata
    // This may need to be updated whenever the golden images get rebuilt.
    #[named]
    #[rstest]
    #[case::v4(harnessv4, 100551)]
    #[case::v5(harness4k, 142530)]
    fn file(#[case] h: fn() -> Harness, #[case] st_ino: libc::ino_t) {
        require_fusefs!();

        let harness = h();
        let path = harness.d.path().join("files").join("hello.txt");

        // Due to the interaction of two bugs, we can't use std::fs::metadata here.
        // Instead, we'll use the lower-level nix::sys::stat::stat
        // https://github.com/rust-lang/rust/issues/108277
        // https://bugs.freebsd.org/bugzilla/show_bug.cgi?id=276602
        let stat = nix::sys::stat::stat(&path).unwrap();

        assert_eq!(stat.st_mtime, 401526123);
        assert_eq!(stat.st_mtime_nsec, 0); // mkimg.sh can't set nsec
        assert_eq!(stat.st_atime, 1332497106);
        assert_eq!(stat.st_atime_nsec, 0); // mkimg.sh can't set nsec

        // mkimg.sh doesn't have a way to set ctime.  So just check that it's
        // greater than mtime.
        assert!(stat.st_ctime > stat.st_mtime || stat.st_ctime_nsec > stat.st_mtime_nsec);
        assert_eq!(stat.st_ino, st_ino);
        assert_eq!(stat.st_size, 14);
        assert_eq!(stat.st_blksize, 4096);
        assert_eq!(stat.st_blocks, 1);
        assert_eq!(stat.st_uid, 1234);
        assert_eq!(stat.st_gid, 5678);
        assert_eq!(stat.st_mode, libc::S_IFREG | 0o1234);
        assert_eq!(stat.st_nlink, 2);
    }

    /// Timestamps from before the Epoch should work
    #[named]
    #[rstest]
    fn pre_epoch(harness4k: Harness) {
        require_fusefs!();

        let path = harness4k.d.path().join("files").join("old.txt");

        let stat = nix::sys::stat::stat(&path).unwrap();
        assert_eq!(stat.st_mtime, -1613800129);
        assert_eq!(stat.st_atime, -1613800129);
    }

    #[named]
    #[rstest]
    #[case::blockdev("blockdev", libc::S_IFBLK)]
    #[case::chardev("chardev", libc::S_IFCHR)]
    #[case::fifo("fifo", libc::S_IFIFO)]
    #[case::socket("sock", libc::S_IFSOCK)]
    fn devs(harness4k: Harness, #[case] filename: &str, #[case] devtype: u16) {
        require_fusefs!();

        let path = harness4k.d.path().join("files").join(filename);

        let stat = nix::sys::stat::stat(&path).unwrap();
        assert_eq!(stat.st_mode & libc::S_IFMT, devtype);
    }

    /// stat should work on symlinks
    #[named]
    #[rstest]
    #[case::sf("sf", 65698)]
    #[case::extent("max", 65699)]
    fn symlink(harness4k: Harness, #[case] linkname: &str, #[case] ino: libc::ino_t) {
        require_fusefs!();

        let path = harness4k.d.path().join("links").join(linkname);

        let flags = nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW;
        let stat = nix::sys::stat::fstatat(libc::AT_FDCWD, &path, flags).unwrap();
        assert_eq!(1, stat.st_nlink, "AT_SYMLINK_NOFOLLOW was ignored");
        assert_eq!(ino, stat.st_ino);
    }
}

#[named]
#[rstest]
fn statfs(harness4k: Harness) {
    require_fusefs!();

    let sfs = nix::sys::statfs::statfs(harness4k.d.path()).unwrap();

    assert_eq!(sfs.blocks(), 23208);
    assert_eq!(sfs.block_size(), 4096);

    // Linux's calculation for blocks available and free is complicated and the
    // docs indicate that it's approximate.  So don't assert on the exact value.
    assert_eq!(
        sfs.blocks_available(),
        i64::try_from(sfs.blocks_free()).unwrap()
    );

    // Linux's calculation for f_files is very confusing and not supported by
    // the XFS documentation.  I think it may be wrong.  So don't assert on it
    // here.
    assert_eq!(i64::try_from(sfs.files()).unwrap() - sfs.files_free(), 750);

    // There are legitimate questions about what the correct value for
    // optimal_transfer_size
    // really is.  Until that's decided, don't assert on it.
    // https://bugs.freebsd.org/bugzilla/show_bug.cgi?id=253424

    // svfs.f_fsid is not very useful, and can't even be read if we aren't root.
    // So ignore it.
}

#[named]
#[rstest]
fn statvfs(harness4k: Harness) {
    require_fusefs!();

    let svfs = nix::sys::statvfs::statvfs(harness4k.d.path()).unwrap();
    // xfuse is always read-only.
    assert!(svfs.flags().contains(nix::sys::statvfs::FsFlags::ST_RDONLY));
    assert_eq!(svfs.fragment_size(), 4096);
    assert_eq!(svfs.blocks(), 23208);

    // Linux's calculation for f_files is very confusing and not supported by
    // the XFS documentation.  I think it may be wrong.  So don't assert on it
    // here.
    assert_eq!(svfs.files() - svfs.files_free(), 750);
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
