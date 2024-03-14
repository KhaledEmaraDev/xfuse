use std::{
    convert::TryFrom,
    ffi::{OsString, OsStr},
    fs::File,
    io::{self, Read},
    os::unix::ffi::{OsStringExt, OsStrExt},
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    thread::sleep,
    time::Duration
};

use assert_cmd::cargo::CommandCargoExt;
use function_name::named;
use tempfile::tempdir;
use xattr::FileExt;

mod util {
    include!("../tests/util.rs");
}
use util::{GOLDEN1K, Md, waitfor};

pub struct Gnop {
    path: PathBuf
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
        Ok(Self{path})
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

struct Bench {
    /// Name of the benchmark
    name: &'static str,
    /// The benchmark's function.  The argument is the path to the mounted file
    /// sytem.  The return value is the number of "useful" bytes the benchmark
    /// read.  An ideal file system would never read anything else.
    f: fn(&Path) -> u64
}

impl Bench {
    const fn new(name: &'static str, f: fn(&Path) -> u64) -> Self {
        Self{name, f}
    }

    fn run(&self, path: &Path) -> u64 {
        (self.f)(path)
    }
}

const BENCHES: [Bench; 3] = [
    Bench::new("metadata", stat_all),
    Bench::new("data", read_all_data),
    Bench::new("xattr", read_all_xattrs)
];

/// Read all metadata from all files in the file system
fn stat_all(path: &Path) -> u64 {
    // inode size is 512B by default and we don't currently have any golden
    // images that use other values.
    const INODE_SIZE: u64 = 512;

    let mut nfiles = 0;
    let walker = walkdir::WalkDir::new(path)
        .into_iter();
    for entry in walker {
        let entry = entry.unwrap();
        let _ = entry.metadata().unwrap();
        nfiles += 1;
    }
    nfiles * INODE_SIZE
}

/// Read all dense files, sequentially
fn read_all_data(path: &Path) -> u64 {
    let mut user_data = 0;
    let mut buf = Vec::new();
    for file in [
        "btree2.2.txt", "btree3.txt", "btree3.3.txt", "btree2_with_xattrs.txt"]
    {
        buf.truncate(0);
        let mut f = File::open(path.join("files").join(file)).unwrap();
        f.read_to_end(&mut buf).unwrap();
        user_data += u64::try_from(buf.len()).unwrap();
    }
    user_data
}

/// Read all xattrs from files that have them, sequentially.
// Note that all of these xattrs are short.  Longer xattrs should give lower RA.
fn read_all_xattrs(path: &Path) -> u64 {
    let mut user_data = 0;
    for file in [
        "xattrs/btree2",
        "xattrs/btree2.3",
        "xattrs/btree3",
        "btree2.with-xattrs"
    ] {
        let p = path.join(file);
        let f = File::open(p).unwrap();
        for attrname in f.list_xattr().unwrap() {
            let value = f.get_xattr(&attrname).unwrap().unwrap();
            user_data += u64::try_from(value.len()).unwrap();
        }
    }
    user_data
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

    let image = &GOLDEN1K;
    let md = Md::new(image).unwrap();
    let gnop = Gnop::new(md.as_ref()).unwrap();
    let d = tempdir().unwrap();

    println!("{:^19} {:^20} {:^20}", "Benchmark", "Total bytes read",
             "Read Amplification");
    println!("{:=^19} {:=^20} {:=^20}", "", "", "");

    for bench in &BENCHES {
        let start_bytes = gnop.read_bytes();

        let mut child = Command::cargo_bin("xfs-fuse").unwrap()
            .arg(gnop.as_path())
            .arg(d.path())
            .spawn()
            .unwrap();

        waitfor(Duration::from_secs(5), || {
            let s = nix::sys::statfs::statfs(d.path()).unwrap();
            s.filesystem_type_name() == "fusefs.xfs"
        }).unwrap();

        let useful_bytes = bench.run(d.path());

        loop {
            let cmd = Command::new("umount")
                .arg(d.path())
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

        let end_bytes = gnop.read_bytes();
        let total_bytes = end_bytes - start_bytes;
        let ra = total_bytes / useful_bytes;
        println!("{:19} {:20} {:19}x", bench.name, total_bytes, ra);
        child.wait().unwrap();
    }
}
