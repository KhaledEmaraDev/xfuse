use std::{
    convert::TryFrom,
    ffi::{OsStr, OsString},
    fs::File,
    io::{self, Read},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    thread::sleep,
    time::Duration,
};

use assert_cmd::cargo::CommandCargoExt;
use function_name::named;
use tempfile::tempdir;
use xattr::FileExt;

mod util {
    include!("../tests/util.rs");
}
use util::{waitfor, GOLDEN1K, GOLDEN4K};

pub struct Gnop {
    path: PathBuf,
}
impl Gnop {
    pub fn new(dev: &Path) -> io::Result<Self> {
        let r = Command::new("gnop")
            .arg("create")
            .arg(dev)
            .status()
            .expect("Failed to execute command")
            .success();
        if !r {
            panic!("Failed to create gnop device");
        }
        let mut path = PathBuf::from(dev);
        path.set_extension("nop");
        Ok(Self { path })
    }

    pub fn as_path(&self) -> &Path {
        &self.path
    }

    /// How many bytes have been read from this gnop so far?
    fn read_bytes(&self) -> u64 {
        let r = Command::new("gnop")
            .arg("list")
            .arg(self.as_path())
            .output()
            .expect("Failed to execute command");
        assert!(r.status.success());
        for line in OsStr::from_bytes(&r.stdout).to_string_lossy().lines() {
            if line.contains("ReadBytes:") {
                let mut fields = line.split_whitespace();
                let _ = fields.next().unwrap();
                let bytes = u64::from_str(fields.next().unwrap()).unwrap();
                return bytes;
            }
        }
        panic!("\"ReadBytes\" not found in \"gnop list\" output.");
    }
}
impl Drop for Gnop {
    fn drop(&mut self) {
        Command::new("gnop")
            .args(["destroy", "-f"])
            .arg(self.as_path())
            .output()
            .expect("failed to deallocate gnop(4) device");
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Image {
    Golden1K,
    Golden4K,
}

struct Bench {
    /// Name of the benchmark
    name:  &'static str,
    /// The disk image to use
    image: Image,
    /// The benchmark's function.  The argument is the path to the mounted file
    /// sytem.  The return value is the number of "useful" bytes the benchmark
    /// read.  An ideal file system would never read anything else.
    f:     fn(&Path) -> u64,
}

impl Bench {
    const fn new(name: &'static str, image: Image, f: fn(&Path) -> u64) -> Self {
        Self { name, image, f }
    }

    fn image(&self) -> &Path {
        match self.image {
            Image::Golden1K => GOLDEN1K.as_path(),
            Image::Golden4K => GOLDEN4K.as_path(),
        }
    }

    fn run(&self, path: &Path) -> u64 {
        (self.f)(path)
    }
}

const BENCHES: &[Bench] = &[
    Bench::new("metadata-sf", Image::Golden4K, stat_sf),
    Bench::new("metadata-block", Image::Golden4K, stat_block),
    Bench::new("metadata-leaf1k", Image::Golden1K, stat_leaf_1k),
    Bench::new("metadata-leaf4k", Image::Golden4K, stat_leaf_4k),
    Bench::new("metadata-node1", Image::Golden1K, stat_node1),
    Bench::new("metadata-node3", Image::Golden1K, stat_node3),
    Bench::new("metadata-btree2.3", Image::Golden1K, stat_btree2_3),
    Bench::new("metadata-btree3", Image::Golden1K, stat_btree3),
    Bench::new("data-fragmented-1k", Image::Golden1K, read_fragmented_1k),
    Bench::new("data-fragmented-4k", Image::Golden4K, read_fragmented_4k),
    Bench::new("data-sequential-1k", Image::Golden1K, read_sequential),
    Bench::new("data-sequential-4k", Image::Golden4K, read_sequential),
    Bench::new("getxattr-local", Image::Golden4K, get_local_xattrs),
    Bench::new("getxattr-extents", Image::Golden4K, get_extents_xattrs),
    Bench::new("getxattr-btree", Image::Golden1K, get_btree_xattrs),
];

fn stat_files(path: &Path, inode_size: u64) -> u64 {
    let mut nfiles = 0;
    let walker = walkdir::WalkDir::new(path).into_iter();
    for entry in walker {
        let entry = entry.unwrap();
        let _ = entry.metadata().unwrap();
        nfiles += 1;
    }
    nfiles * inode_size
}

/// Read all metadata from all files in the shortform directory
fn stat_sf(mountpoint: &Path) -> u64 {
    stat_files(&mountpoint.join("sf"), 512)
}

/// Read all metadata from all files in the block directory
fn stat_block(mountpoint: &Path) -> u64 {
    stat_files(&mountpoint.join("block"), 512)
}

/// Read all metadata from all files in the leaf directory
fn stat_leaf_1k(mountpoint: &Path) -> u64 {
    stat_files(&mountpoint.join("leaf"), 512)
}

/// Read all metadata from all files in the leaf directory
fn stat_leaf_4k(mountpoint: &Path) -> u64 {
    stat_files(&mountpoint.join("leaf"), 512)
}

/// Read all metadata from all files in the node1 directory
fn stat_node1(mountpoint: &Path) -> u64 {
    stat_files(&mountpoint.join("node1"), 512)
}

/// Read all metadata from all files in the node3 directory
fn stat_node3(mountpoint: &Path) -> u64 {
    stat_files(&mountpoint.join("node3"), 512)
}

/// Read all metadata from all files in the btree2.3 directory
fn stat_btree2_3(mountpoint: &Path) -> u64 {
    stat_files(&mountpoint.join("btree2.3"), 512)
}

/// Read all metadata from all files in the btree3 directory
fn stat_btree3(mountpoint: &Path) -> u64 {
    stat_files(&mountpoint.join("btree3"), 512)
}

fn read_files(mountpoint: &Path, files: &[&'static str]) -> u64 {
    let mut user_data = 0;
    let mut buf = Vec::new();
    for file in files {
        buf.truncate(0);
        let mut f = File::open(mountpoint.join("files").join(file)).unwrap();
        f.read_to_end(&mut buf).unwrap();
        user_data += u64::try_from(buf.len()).unwrap();
    }
    user_data
}

/// Read all fragmented dense files in the 4k golden image, sequentially
fn read_fragmented_1k(mountpoint: &Path) -> u64 {
    read_files(
        mountpoint,
        &[
            "btree2.2.txt",
            "btree3.txt",
            "btree3.3.txt",
            "btree2_with_xattrs.txt",
        ],
    )
}

/// Read all fragmented dense files in the 1k golden image, sequentially
fn read_fragmented_4k(mountpoint: &Path) -> u64 {
    read_files(
        mountpoint,
        &[
            "partial_extent.txt",
            "single_extent.txt",
            "four_extents.txt",
            "btree2.txt",
            "btree2.4.txt",
            "btree3.txt",
        ],
    )
}

/// Read all sequential dense files, sequentially
fn read_sequential(mountpoint: &Path) -> u64 {
    read_files(mountpoint, &["large_extent.txt"])
}

fn get_xattrs(mountpoint: &Path, files: &[&'static str]) -> u64 {
    let mut user_data = 0;

    for file in files {
        let p = mountpoint.join(file);
        let f = File::open(p).unwrap();
        for attrname in f.list_xattr().unwrap() {
            let value = f.get_xattr(&attrname).unwrap().unwrap();
            user_data += u64::try_from(attrname.len() + value.len()).unwrap();
        }
    }
    user_data
}

/// Get all extended attributes that are stored in their inode's attribute fork
fn get_local_xattrs(mountpoint: &Path) -> u64 {
    get_xattrs(mountpoint, &["xattrs/local"])
}

/// Get all extended attributes that are stored in a separate extents list
fn get_extents_xattrs(mountpoint: &Path) -> u64 {
    get_xattrs(mountpoint, &["xattrs/extents"])
}

/// Get all extended attributes that are stored in a btree
fn get_btree_xattrs(mountpoint: &Path) -> u64 {
    get_xattrs(
        mountpoint,
        &["xattrs/btree2", "xattrs/btree2.5", "xattrs/btree3"],
    )
}

#[named]
fn main() {
    require_fusefs!();
    require_root!();

    // Outline:
    // 0) Decompress the image, if needed
    // 1) Create an md device
    // 2) Create a gnop device
    //
    // for each benchmark:
    //   1) Check the gnop's stats
    //   2) start xfs-fuse
    //   3) Run the operation
    //   4) unmount
    //   5) Check the gnop's stats and print the difference

    println!(
        "{:^19} {:^20} {:^20}",
        "Benchmark", "Total bytes read", "Read Amplification"
    );
    println!("{:=^19} {:=^20} {:=^20}", "", "", "");

    for bench in BENCHES {
        let md = mdconfig::Builder::vnode(bench.image()).create().unwrap();
        let gnop = Gnop::new(md.path()).unwrap();
        let d = tempdir().unwrap();

        let mut child = Command::cargo_bin("xfs-fuse")
            .unwrap()
            .arg(gnop.as_path())
            .arg(d.path())
            .spawn()
            .unwrap();

        waitfor(Duration::from_secs(5), || {
            let s = nix::sys::statfs::statfs(d.path()).unwrap();
            s.filesystem_type_name() == "fusefs.xfs"
        })
        .unwrap();

        // start_bytes excludes whatever was necessary to mount the file system.
        let start_bytes = gnop.read_bytes();

        let useful_bytes = bench.run(d.path());

        loop {
            let cmd = Command::new("umount").arg(d.path()).output();
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

        let end_bytes = gnop.read_bytes();
        let total_bytes = end_bytes - start_bytes;
        let ra = total_bytes as f64 / useful_bytes as f64;
        println!("{:19} {:20} {:19.1}x", bench.name, total_bytes, ra);
        child.wait().unwrap();
    }
}
