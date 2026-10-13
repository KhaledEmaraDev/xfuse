/*
 * BSD 2-Clause License
 *
 * Copyright (c) 2026, Pedro Giffuni
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions are met:
 *
 * 1. Redistributions of source code must retain the above copyright notice, this
 *    list of conditions and the following disclaimer.
 *
 * 2. Redistributions in binary form must reproduce the above copyright notice,
 *    this list of conditions and the following disclaimer in the documentation
 *    and/or other materials provided with the distribution.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
 * AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
 * DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
 * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 * SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
 * CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
 * OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
 * OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
 */
//! End-to-end tests for writing.
//!
//! Every test here does the same round trip: copy a golden image, mount it,
//! change a file through the mount point, unmount, mount it again, and check
//! that the change is there.  The image is a file rather than a block device,
//! which is how the golden images are built, so the tests do not need root.
//!
//! What these tests can and cannot prove is worth being explicit about.  They
//! prove that the file system wrote what it was asked to write, that it reads
//! back what it wrote, and -- where `xfs_repair` is available -- that the
//! result is still a file system that XFS itself considers consistent.  They
//! cannot prove that the kernel's own XFS driver would mount the result, because
//! that needs a block device and root; `xfs_repair` is the next best witness
//! available without them.

mod util;

use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use util::{writable_copy, GOLDEN4KN, GOLDENV4, GOLDENWRITABLE};

/// Copy a golden image, mount it read-write, hand the mountpoint to `body`, and
/// put everything back.
fn with_rw_mount<T>(golden: &Path, tag: &str, body: impl FnOnce(&Path) -> T) -> T {
    let image = writable_copy(golden, tag);
    with_rw_mount_at(&image, tag, body)
}

/// Mount an image read-write, hand the mountpoint to `body`, and put everything
/// back.
fn with_rw_mount_at<T>(image: &Path, tag: &str, body: impl FnOnce(&Path) -> T) -> T {
    let _guard = serialize();
    let fs = mount(image, tag, true);
    body(&fs.mnt)
}

/// Mount an image read-only, hand the mountpoint to `body`, and put everything
/// back.
fn with_ro_mount<T>(image: &Path, tag: &str, body: impl FnOnce(&Path) -> T) -> T {
    let _guard = serialize();
    let fs = mount(image, tag, false);
    body(&fs.mnt)
}

/// Run these tests one at a time.
///
/// Every test here mounts a file system, and mounting is not something a small
/// build machine does well in parallel: the tests would spend their time
/// competing for the same disk and the same FUSE slots, and a failure would be
/// much harder to read.  The lock is taken for the whole test, so the tests queue
/// up.  A test that panics poisons the lock, and poisoning has to be ignored
/// here, or one failure would turn into ten.
fn serialize() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A mounted file system and the process serving it.
///
/// The process is killed and the mount cleaned up when this is dropped.  That
/// matters for more than tidiness: a test that fails part way through unwinds
/// past any cleanup written after the failure, and a FUSE process that is left
/// running keeps its mount *and* the test harness's output pipe open, which is
/// enough to make the whole run look like it has hung.
struct Mounted {
    mnt:     PathBuf,
    process: Child,
}

impl Drop for Mounted {
    fn drop(&mut self) {
        // Every write was committed before the kernel was told it had
        // succeeded, so there is nothing to flush, and the only way to be sure
        // the process is gone is to insist.  The kernel takes the mount down when
        // the process closes its FUSE descriptor.
        let _ = self.process.kill();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.process.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                // Out of patience: fall through and ask the system below.
                Ok(None) if Instant::now() >= deadline => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            }
        }

        // If the mount outlived the process, ask the system to take it down.
        // Both steps are bounded, because a test that hangs says nothing.
        let mut unmounted = false;
        for unmounter in [
            "/bin/fusermount3",
            "/usr/local/bin/fusermount3",
            "/usr/bin/fusermount3",
            "fusermount3",
            "fusermount",
        ] {
            let Ok(mut fusermount) = Command::new(unmounter)
                .arg("-u")
                .arg(&self.mnt)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
            else {
                // Not installed under that name; try the next one.
                continue;
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match fusermount.try_wait() {
                    Ok(Some(status)) => {
                        // The unmounter's exit status is the only reliable
                        // statement available about whether the mount is gone.
                        unmounted |= status.success();
                        break;
                    }
                    Ok(None) if Instant::now() >= deadline => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                    Err(_) => break,
                }
            }
            let _ = fusermount.kill();
            if unmounted {
                break;
            }
        }

        // Only touch the mountpoint if an unmount said it worked.  A FUSE mount
        // whose server has gone can stay registered, and a path inside such a
        // mount blocks rather than failing, so `remove_dir_all` on a
        // mountpoint whose server may be dead is a way for a test to hang
        // forever.  A mount that would not unmount leaves its directory in the
        // system temporary directory, where nothing has to walk it.
        if unmounted {
            let _ = std::fs::remove_dir_all(&self.mnt);
        }
    }
}

/// Mount `image` at a mountpoint of our own, and wait until it answers.
///
/// `tag` keeps two tests, or two runs, from sharing a mountpoint.
fn mount(image: &Path, tag: &str, writable: bool) -> Mounted {
    // A path of its own for every mount, including every round of a test that
    // mounts more than once: a mount left behind by a killed process stays
    // registered, and the next test would block at the first call it makes on
    // the path.
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut mnt = mountpoint_dir();
    mnt.push(format!("mnt-{tag}-{seq}"));
    std::fs::create_dir_all(&mnt)
        .unwrap_or_else(|e| panic!("creating the mountpoint {mnt:?}: {e}"));

    let mut command = Command::new(env!("CARGO_BIN_EXE_xfs-fuse"));
    command.arg("-f");
    if writable {
        command.arg("-r");
    }
    // The child's output would otherwise be the test harness's, and a process
    // that outlives its test would hold that pipe open.
    let mut process = command
        .arg(image)
        .arg(&mnt)
        // When `XFUSE_TEST_LOG` is set the child speaks, which is the only way to
        // see why the file system refused something the test asked it to do.
        .stdout(Stdio::null())
        .stderr(if std::env::var_os("XFUSE_TEST_LOG").is_some() {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .spawn()
        .unwrap_or_else(|e| panic!("running xfs-fuse on {image:?}: {e}"));

    let mounted = util::waitfor(Duration::from_secs(60), || is_live(&mnt));
    if let Err(e) = mounted {
        let _ = process.kill();
        panic!("xfs-fuse did not mount {image:?} at {mnt:?}: {e}");
    }
    Mounted { mnt, process }
}

/// Run a command to completion, with a bound on how long it may take, and return
/// what it said.
///
/// The bound is the point.  A test that waits forever tells whoever reads the
/// output nothing; a test that says "the mount did not refuse within a minute"
/// says exactly what happened.  The output goes to files rather than to pipes
/// because a pipe has to be drained by somebody, and draining it is another way
/// for a test to block.
fn run_bounded(
    command: &mut Command,
    tag: &str,
    limit: Duration,
) -> (Option<std::process::ExitStatus>, String, String) {
    let mut out_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    out_path.push(format!("out-{}-{tag}-{tag}.log", std::process::id()));
    let mut err_path = out_path.clone();
    err_path.set_extension("err");
    let out = File::create(&out_path).expect("creating the child's output file");
    let err = File::create(&err_path).expect("creating the child's error file");
    let mut process = command
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .expect("running the command");

    let deadline = Instant::now() + limit;
    let status = loop {
        match process.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = process.kill();
                let _ = process.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("waiting for {tag}: {e}"),
        }
    };
    let out = std::fs::read_to_string(&out_path).unwrap_or_default();
    let err = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&err_path);
    (status, out, err)
}

/// A directory of this run's own, in the system temporary directory.
///
/// The mountpoints live here rather than in the target directory because a mount
/// left behind by a killed process stays registered, and a build machine that
/// later collects the target directory will trip over it.
fn mountpoint_dir() -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("xfuse-write-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("creating {}: {e}", dir.display()));
    dir
}

/// Is the file system at `mnt` answering requests?
///
/// An empty directory on the file system's own storage answers `read_dir` too,
/// so a mount that has not come up yet looks like one that has.  The one thing
/// a live mount has and a bare directory does not is a name in the root.
fn is_live(mnt: &Path) -> bool {
    std::fs::read_dir(mnt)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// Is the test running as root?
fn root() -> bool {
    nix::unistd::Uid::current().is_root()
}

/// Read a whole file.
fn read_file(path: &Path) -> Vec<u8> {
    let mut buf = Vec::new();
    File::open(path)
        .unwrap_or_else(|e| panic!("opening {path:?}: {e}"))
        .read_to_end(&mut buf)
        .expect("reading the file");
    buf
}

/// Open a file for writing without truncating it.
///
/// `create` and truncation are namespace operations, and this suite is only
/// about overwriting, so the file is opened the only way that stays inside what
/// the file system supports today.
fn open_rw(path: &Path) -> File {
    try_open_rw(path).unwrap_or_else(|e| panic!("opening {path:?} for writing: {e}"))
}

/// The same, but reporting the error instead of panicking, for the cases where
/// the error is the thing under test.
fn try_open_rw(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
}

/// Does `xfs_repair -n` consider the image consistent?
///
/// Returns the tool's combined output so that a failure can be reported with
/// the reason the tool gave.
fn xfs_repair_check(image: &Path) -> Result<String, String> {
    for tool in ["xfs_repair", "/usr/sbin/xfs_repair", "/sbin/xfs_repair"] {
        let output = Command::new(tool).arg("-n").arg(image).output();
        let Ok(output) = output else { continue };
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !output.status.success() {
            return Err(format!("{tool} failed: {text}"));
        }
        // A clean run says nothing about the structure; a damaged one says so
        // loudly, so look for the words that mean "this needs repair".
        for bad in [
            "bad ",
            "would fix",
            "Metadata CRC error",
            "incorrect",
            "corrupt",
            "zeroed",
        ] {
            if text.contains(bad) {
                return Err(format!("{tool} reported a problem: {text}"));
            }
        }
        return Ok(text);
    }
    // No repair tool: not a failure, but say so.
    Ok(String::from(
        "xfs_repair is not installed; the image was not checked",
    ))
}

/// Overwriting a byte in the middle of a file must stick.
#[test]
fn overwrite_byte() {
    require_fusefs!();
    with_rw_mount(&GOLDENV4, "byte", |mnt| {
        let file = mnt.join("files/hello.txt");
        let before = read_file(&file);
        assert_eq!(
            &before[..6],
            b"Hello,",
            "unexpected starting content: {before:?}"
        );

        let mut f = open_rw(&file);
        f.seek(SeekFrom::Start(1)).unwrap();
        f.write_all(b"J").unwrap();
        drop(f);

        let after = read_file(&file);
        assert_eq!(&after[..6], b"HJllo,", "the byte was not overwritten");
        assert_eq!(&after[6..], &before[6..], "the rest of the file changed");
    });
}

/// The change must be in the image, not just in the kernel's page cache, so
/// check it with a second, read-only mount.
#[test]
fn overwrite_survives_remount() {
    require_fusefs!();
    let image = writable_copy(&GOLDENV4, "remount");
    with_rw_mount_at(&image, "remount", |mnt| {
        let mut f = open_rw(&mnt.join("files/hello.txt"));
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(b"HELLO").unwrap();
    });
    with_ro_mount(&image, "remount-ro", |mnt| {
        let after = read_file(&mnt.join("files/hello.txt"));
        assert_eq!(&after[..5], b"HELLO");
        assert_eq!(
            &after[5..],
            &b", World!\n"[..],
            "the rest of the file changed: {after:?}"
        );
    });
}

/// A write that does not start at a block boundary must leave the bytes on
/// either side of it alone.
#[test]
fn partial_block_write() {
    require_fusefs!();
    // The file system in this image has 512-byte blocks, and hello.txt is one
    // block long, so a write of a few bytes in the middle of it is a
    // read-modify-write of a whole block.
    with_rw_mount(&GOLDENV4, "partial", |mnt| {
        let file = mnt.join("files/hello.txt");
        let before = read_file(&file);
        let mut f = open_rw(&file);
        f.seek(SeekFrom::Start(4)).unwrap();
        f.write_all(b"XYZ").unwrap();
        drop(f);
        let after = read_file(&file);
        assert_eq!(&after[..4], &before[..4]);
        assert_eq!(&after[4..7], b"XYZ");
        assert_eq!(&after[7..], &before[7..]);
    });
}

/// A write across a whole block must work, and so must one that starts and ends
/// inside blocks.
///
/// Every check here is made through a *second*, read-only mount, because the
/// kernel's page cache would otherwise hand back the bytes that were written
/// rather than the bytes that were stored: a file system that put a write in
/// the wrong place in the image would still pass a test that reads its own
/// writes back.
#[test]
fn whole_block_and_unaligned_writes() {
    require_fusefs!();
    // large_extent.txt is a megabyte of data in many extents, so this covers
    // writes that cross extent boundaries as well as block boundaries.
    let image = writable_copy(&GOLDENV4, "blocks");
    let mut expected = with_ro_mount(&image, "blocks-ro", |mnt| {
        read_file(&mnt.join("files/large_extent.txt"))
    });
    assert_eq!(expected.len(), 1 << 20, "unexpected file size");

    for (round, offset) in [0usize, 1, 511, 512, 1000, 4096, 4097].iter().enumerate() {
        let offset = *offset;
        with_rw_mount_at(&image, "blocks", |mnt| {
            let mut f = open_rw(&mnt.join("files/large_extent.txt"));
            f.seek(SeekFrom::Start(offset as u64)).unwrap();
            f.write_all(&[b'A'; 512]).unwrap();
        });
        expected[offset..offset + 512].fill(b'A');

        let after = with_ro_mount(&image, "blocks-ro", |mnt| {
            read_file(&mnt.join("files/large_extent.txt"))
        });
        assert_eq!(
            after.len(),
            expected.len(),
            "round {round}: the file changed size"
        );
        assert_eq!(
            after, expected,
            "round {round}: the image does not hold what was written"
        );
    }
}

/// The last byte of a file is as writable as any other, and the byte after it
/// is not.
#[test]
fn write_the_last_byte() {
    require_fusefs!();
    let image = writable_copy(&GOLDENV4, "last-byte");
    with_rw_mount_at(&image, "last-byte", |mnt| {
        let file = mnt.join("files/hello.txt");
        let size = std::fs::metadata(&file).unwrap().len();
        let mut f = open_rw(&file);
        f.seek(SeekFrom::Start(size - 1)).unwrap();
        f.write_all(b"!").unwrap();
    });
    let content = with_ro_mount(&image, "last-byte-ro", |mnt| {
        read_file(&mnt.join("files/hello.txt"))
    });
    // The golden file is "Hello, World!\n", so the last byte is its newline,
    // and that is what the write replaced.
    assert_eq!(content.len(), 14, "writing the last byte changed the size");
    assert_eq!(&content[..13], b"Hello, World!");
    assert_eq!(content[13], b'!', "the last byte did not change");
}

/// A write past the end of a file makes the file bigger: new blocks allocated,
/// an extent recorded, and the size changed, all of it visible on the image and
/// not only through the mount that wrote it.
#[test]
fn a_write_past_the_end_grows_the_file() {
    require_fusefs!();
    let image = writable_copy(&GOLDENV4, "extend");
    let size_before = with_rw_mount_at(&image, "extend", |mnt| {
        let file = mnt.join("files/hello.txt");
        let before = std::fs::metadata(&file).unwrap().len() as usize;
        // Append four blocks' worth, which is past the end.
        let mut f = open_rw(&file);
        f.seek(SeekFrom::End(0)).unwrap();
        f.write_all(&[b'A'; 4096 * 4]).unwrap();
        drop(f);
        let after = std::fs::metadata(&file).unwrap().len() as usize;
        assert_eq!(after, before + 4096 * 4, "the file did not grow");
        before
    });

    // And it grew on the image, not just in the kernel's idea of it.
    with_ro_mount(&image, "extend-ro", |mnt| {
        let file = mnt.join("files/hello.txt");
        let content = read_file(&file);
        assert_eq!(content.len(), size_before + 4096 * 4);
        assert!(
            content[size_before..].iter().all(|b| *b == b'A'),
            "the new bytes"
        );
        assert_eq!(&content[..size_before], b"Hello, World!\n", "the old bytes");
    });
}

/// A file that is made shorter gives its blocks back, and the image says so.
///
/// The oracle here is stronger than for the write tests, and deliberately so.
/// A grown file on this image is still a file the rest of the file system can
/// find, because it already had a directory entry; a *shortened* one is too, and
/// that is what lets `xfs_repair -n` judge the result at all.  It cannot do the
/// same for a newly allocated inode, which no directory points at and which
/// repair therefore calls disconnected.
///
/// So this checks three things a write test cannot:
///
///   * the file is the size it was made, after a fresh read-only mount;
///   * the blocks it gave up are free again -- checked with the image's own
///     accounting, not by looking for particular numbers;
///   * and `xfs_repair -n` accepts the image, which is the only thing here that
///     can tell whether the inode's extents, its block count and the group's free
///     space all describe the same file system.
#[test]
fn a_file_made_shorter_gives_its_blocks_back() {
    require_fusefs!();
    let image = writable_copy(&GOLDENV4, "truncate");
    const GROWN: usize = 4096 * 4;
    let before = with_rw_mount_at(&image, "truncate", |mnt| {
        let file = mnt.join("files/hello.txt");
        let size = std::fs::metadata(&file).unwrap().len() as usize;
        let mut f = open_rw(&file);
        f.seek(SeekFrom::End(0)).unwrap();
        f.write_all(&[b'A'; GROWN]).unwrap();
        drop(f);
        assert_eq!(
            std::fs::metadata(&file).unwrap().len() as usize,
            size + GROWN,
            "the file did not grow"
        );
        size
    });

    // Now cut it back to a point **strictly inside** the run the append added.
    //
    // That is the case the arithmetic in `drop_extents_above` is about, and it is
    // easy to miss: cutting back to the file's original length lands the new end
    // exactly on an extent boundary, where the straddling extent is not straddling
    // anything and the whole thing is correct by accident.  The first version of
    // this test cut back to half the original size -- seven bytes, one block --
    // and passed with a bug in place that gave away the file's own block, because
    // seven bytes is not inside any extent.
    let wanted = before + 1500;
    // Every `xfs_db` reading below is optional: the project's CI has no `xfs_db`,
    // and a check that cannot run is a check to skip, not a test to fail.
    let Some(blocklog) =
        xfs_db_field(&image, &["sb 0"], "blocklog").and_then(|v| v.parse::<u32>().ok())
    else {
        eprintln!("skipping the free-space check: no xfs_db to read the block size with");
        return;
    };
    let blocksize = 1usize << blocklog;
    let held = |size: usize| size.div_ceil(blocksize);
    let Some(fdblocks_before) = xfs_db_field(&image, &["sb 0"], "fdblocks") else {
        eprintln!("skipping the free-space check: no xfs_db to read the free count with");
        return;
    };
    let gave_up = held(before + GROWN) - held(wanted);
    assert!(gave_up > 0, "the test would not shorten a block at all");

    with_rw_mount_at(&image, "truncate-shorter", |mnt| {
        let file = mnt.join("files/hello.txt");
        open_rw(&file).set_len(wanted as u64).unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().len() as usize, wanted);
    });

    // The blocks are *given back*, which is the part of a truncate that is easy to
    // leave out and which nothing about the file's own appearance would show: an
    // extent longer than the file's size is legal, so a file that keeps the blocks
    // it no longer needs reads and even repairs perfectly.  The file system's own
    // free count is the thing that moves, and it is the sum of the free space, the
    // b-tree blocks and the free list -- so freeing four blocks shows up as four
    // more on `sb_fdblocks` whichever of those three took them.
    if let Some(after) = xfs_db_field(&image, &["sb 0"], "fdblocks") {
        assert_eq!(
            after.parse::<u64>().expect("a number"),
            fdblocks_before.parse::<u64>().expect("a number") + gave_up as u64,
            "the {gave_up} blocks the file gave up were not given back"
        );
    }

    with_ro_mount(&image, "truncate-ro", |mnt| {
        let file = mnt.join("files/hello.txt");
        let content = read_file(&file);
        assert_eq!(content.len(), wanted, "the file did not stay short");
        // The bytes that stayed are the original file's, plus the part of the
        // appended run that the new end still covers.
        assert_eq!(&content[..before], b"Hello, World!\n", "the original bytes");
        assert!(
            content[before..].iter().all(|b| *b == b'A'),
            "the part of the appended run that is still inside the file"
        );
        assert_eq!(
            content[wanted - 1..wanted].len(),
            1,
            "the file is exactly as long as it was asked to be"
        );
    });

    repair_accepts(&image, "after a file was made shorter");
}

/// One field of the image, as `xfs_db` reads it.
///
/// The tests here otherwise judge the image through a mount, which is the right
/// thing to do for what a *file* looks like and the wrong thing for what a *file
/// system* holds: a mount says nothing about whether the blocks a file gave up
/// went anywhere.
fn xfs_db_field(image: &std::path::Path, commands: &[&str], field: &str) -> Option<String> {
    let mut cmd = Command::new("xfs_db");
    for c in commands {
        cmd.arg("-c").arg(c);
    }
    // `None` rather than a panic where the tool is not installed: the project's
    // own CI has no `xfs_db`, and a test that fails because it cannot run its
    // oracle has not found anything.  Every caller treats `None` as "skip this
    // check".
    let out = cmd
        .arg("-c")
        .arg(format!("p {field}"))
        .arg(image)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    text.lines()
        .find_map(|l| l.split_once('=').map(|(_, v)| v.trim().to_string()))
}

/// Where a file's extents stop fitting in its inode, which is now measured.
///
/// This used to be a test of a thing that did not exist.  It is now a test of the
/// boundary, which is the part that can be established without a b-tree
/// implementation: a 256-byte inode's local area holds **nine** extent records, so
/// a file written sparsely enough to need a tenth extent cannot be written by
/// this file system, and it is refused rather than written wrongly.
///
/// The refusal matters as much as the limit.  A file whose extents do not fit
/// has to be either moved into a b-tree or refused, because the alternative --
/// writing an eleventh record over the attribute fork, or over the inode's own
/// tail -- would produce a file that reads back as something else.  So there is a
/// boundary there, and **what happens at it is the conversion**: the fork becomes
/// a B+tree and the writes carry on.  This pins where the boundary is and that
/// crossing it leaves a file system `xfs_repair` accepts.
///
/// It used to pin a refusal.  That was correct while the format's answer -- a
/// b-tree rooted in the inode -- had neither a reader nor a writer, and the
/// refusal was `ENOSYS` and said why.  Both exist now, so the boundary is where the
/// conversion happens instead, and this test's claim is that the *number* has not
/// moved: nine, from `(256 - 100) / 16`.
///
/// The number of records is not read back from the image, because `xfs_db` cannot
/// resolve inode 128 in this image at all -- it returns a magic of `0x5844` where
/// the other images return `0x494e` -- so the capacity is measured by the only
/// means available: how many sparse writes it takes before the file stops growing.
#[test]
fn a_file_crossing_its_inode_extent_capacity_becomes_a_btree() {
    require_fusefs!();
    let image = writable_copy(&GOLDENV4, "extents");
    let blocksize = 512u64;

    // Twice the block size apart, so the extents are never adjacent and never
    // joined: a write beside the last one would make one extent rather than two,
    // and the file would never reach the boundary.
    let grown = with_rw_mount_at(&image, "extents", |mnt| {
        let file = mnt.join("files/hello.txt");
        let mut f = open_rw(&file);
        let mut n = 0usize;
        for i in 0..20 {
            f.seek(SeekFrom::Start(i as u64 * 2 * blocksize)).unwrap();
            if f.write_all(&[b'A' + (i % 26) as u8]).is_err() {
                break;
            }
            n += 1;
        }
        n
    });

    // It got past the fork's capacity -- nine records in a 256-byte inode -- which
    // is only possible if the fork became something else.
    assert!(
        grown > 9,
        "{grown} sparse writes, so the file never crossed its inode's extent capacity and nothing \
         here was tested"
    );

    // And every byte of them survived, and the image is one repair accepts.
    with_ro_mount(&image, "extents-ro", |mnt| {
        let file = mnt.join("files/hello.txt");
        let content = std::fs::read(&file).unwrap();
        for i in 0..grown {
            let at = i * 2 * blocksize as usize;
            assert_eq!(
                content.get(at).copied(),
                Some(b'A' + (i % 26) as u8),
                "byte {i} did not survive the conversion"
            );
        }
    });
    repair_accepts(&image, "after a file crossed its inode's extent capacity");
}

/// A file's mode, owner and timestamps can be changed, and the change survives a
/// remount.
///
/// `setattr` previously honoured a file's size and refused everything else with
/// `ENOTSUP`, which made `chmod`, `chown` and `utimes` fail on every file.  These
/// are the attributes a caller changes most often after the size, and the inode
/// has a field for each.
///
/// What is checked, in order of how badly a mistake would hurt:
///
/// * the **mode** survives, through a fresh read-only mount — read from the image,
///   not from the page cache of the process that wrote it;
/// * the **timestamps** survive, including a *named* one rather than "now", which
///   is the case where a mistake would be invisible: writing the current time
///   twice always looks right;
/// * the file **type is not taken from the mode**.  The kernel sends the whole
///   mode with the file type in it, and copying that into the inode would let
///   `chmod` turn a file into a directory, so only the permission bits are stored;
/// * and `xfs_repair -n` accepts the image, which is what says the inode's
///   checksum and fields still agree.
#[test]
fn a_files_permissions_and_times_can_be_changed() {
    require_fusefs!();
    let image = writable_copy(&GOLDENV4, "setattr");
    let blocksize = 4096u64;
    let named = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);

    with_rw_mount_at(&image, "setattr", |mnt| {
        let file = mnt.join("files/hello.txt");
        let before = std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777;
        assert_ne!(
            before, 0,
            "the file starts with no permissions, so a change proves nothing"
        );
        let f = open_rw(&file);
        f.set_permissions(std::fs::Permissions::from_mode(0o640))
            .unwrap();
        drop(f);
    });

    // A named timestamp, set on its own so that "now" cannot mask a mistake.
    with_rw_mount_at(&image, "setattr-times", |mnt| {
        let file = mnt.join("files/hello.txt");
        let f = open_rw(&file);
        f.set_times(
            std::fs::FileTimes::new()
                .set_modified(named)
                .set_accessed(named),
        )
        .unwrap();
        drop(f);
    });

    with_ro_mount(&image, "setattr-ro", |mnt| {
        let file = mnt.join("files/hello.txt");
        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!(
            meta.permissions().mode() & 0o7777,
            0o640,
            "the permissions did not survive the remount"
        );
        let modified = meta.modified().unwrap();
        assert_eq!(
            modified
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            named
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            "the modification time is not the one that was set"
        );
        // And the file is still a file: the mode must not have carried a type
        // through into the inode.
        assert!(meta.is_file(), "the file stopped being a file");
        assert_eq!(
            std::fs::read(&file).unwrap().len(),
            14,
            "its contents did not change"
        );
        let _ = blocksize;
    });

    repair_accepts(&image, "after a file's permissions and times were changed");
}
/// The image is a file system `xfs_repair -n` accepts, or the reason it does not.
///
/// A skip where there is no repair tool to ask, because a test that cannot run its
/// oracle should say so rather than pass quietly.
fn repair_accepts(image: &std::path::Path, what: &str) {
    // Skipped, with a word, where `xfs_repair` is not installed -- which is the
    // case on the project's CI, and which is a fact about the host rather than
    // about the image.
    let Ok(out) = Command::new("xfs_repair").arg("-n").arg(image).output() else {
        // Loudly.  A skipped oracle is not a passing test, and a run that cannot
        // say how many checks it did not make cannot be read as a run that
        // verified them.
        eprintln!("SKIPPED ORACLE CHECK: {what} (no xfs_repair to run)");
        return;
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let complaints: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with('-')
                && !l.starts_with("Phase")
                && !l.starts_with("No modify")
                && !l.contains("sector size mismatch")
                && !l.contains("host filesystem")
                && !l.contains("Finished running")
        })
        .collect();
    assert!(
        out.status.success(),
        "xfs_repair -n rejected the image {what}:\n{text}"
    );
    assert!(
        complaints.is_empty(),
        "xfs_repair -n had things to say {what}:\n{text}"
    );
}

/// A write that leaves a gap reads as zeroes, because the blocks in the gap are
/// not the file's and must not hold what they held before.
#[test]
fn a_write_past_the_end_can_leave_a_gap() {
    require_fusefs!();
    let image = writable_copy(&GOLDENV4, "extend-gap");
    let size_before = with_rw_mount_at(&image, "extend-gap", |mnt| {
        let file = mnt.join("files/hello.txt");
        let before = std::fs::metadata(&file).unwrap().len() as usize;
        let mut f = open_rw(&file);
        f.seek(SeekFrom::End(0)).unwrap();
        // A kilobyte of data, then a kilobyte of nothing, then a kilobyte of data.
        f.write_all(&[b'B'; 1024]).unwrap();
        f.seek(SeekFrom::Current(1024)).unwrap();
        f.write_all(&[b'C'; 1024]).unwrap();
        drop(f);
        before
    });
    with_ro_mount(&image, "extend-gap-ro", |mnt| {
        let content = read_file(&mnt.join("files/hello.txt"));
        assert_eq!(content.len(), size_before + 3072);
        assert!(content[size_before..size_before + 1024]
            .iter()
            .all(|b| *b == b'B'));
        assert!(
            content[size_before + 1024..size_before + 2048]
                .iter()
                .all(|b| *b == 0),
            "the gap must read as zeroes, not as whatever those blocks held"
        );
        assert!(content[size_before + 2048..].iter().all(|b| *b == b'C'));
    });
}

/// The file system has to agree that the image is still a file system.
#[test]
fn a_grown_file_leaves_the_image_consistent() {
    require_fusefs!();
    let image = writable_copy(&GOLDENV4, "extend-repair");
    with_rw_mount_at(&image, "extend-repair", |mnt| {
        let mut f = open_rw(&mnt.join("files/hello.txt"));
        f.seek(SeekFrom::End(0)).unwrap();
        f.write_all(&[b'Z'; 4096 * 3]).unwrap();
    });
    if let Err(e) = xfs_repair_check(&image) {
        panic!("{e}");
    }
}

/// A file system nobody has written to yet is the other end of the allocator's
/// range, and every image the other tests use was built to be awkward.
///
/// A group in a freshly made file system holds its free space in one leaf and is
/// not fragmented at all, which is a shape the hand-built images never have.  So
/// this writes a file that did not exist, grows it past its end, and asks the
/// file system's own repair whether the image is still one.
#[test]
fn a_freshly_made_image_takes_writes_and_stays_a_file_system() {
    require_fusefs!();
    let image = writable_copy(&GOLDENWRITABLE, "fresh");
    with_rw_mount_at(&image, "fresh", |mnt| {
        // Making a file is not something a read-write mount can do yet -- it
        // needs a block map that can be written, which is later work -- so this
        // grows a file the image already has instead.
        let path = mnt.join("dir/big.bin");
        let before = std::fs::metadata(&path).unwrap().len();
        let mut f = open_rw(&path);
        f.seek(SeekFrom::End(0)).unwrap();
        f.write_all(&[b'E'; 4096 * 3]).expect("growing a file");
        drop(f);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            before + 4096 * 3,
            "the file did not grow"
        );
        // And the bytes are there to read back, which is the other half of a
        // write being real.
        let got = std::fs::read(&path).unwrap();
        assert!(got[before as usize..].iter().all(|b| *b == b'E'));
    });
    if let Err(e) = xfs_repair_check(&image) {
        panic!("{e}");
    }
}

// Writing into a hole, or into preallocated-but-unwritten space, needs an
// allocator and so must be refused rather than guessed at.  There is no test for
// it here yet, and the reason is worth writing down: the image that would test
// it, xfs_preallocated.img, is a version 5 image with reflink, rmapbt and
// big-time, and the capability gate refuses all three for writing.  The
// behaviour is covered where it can be covered for now -- `ExtentMap` reports an
// unwritten extent as a hole, and a hole as no block at all, both in the unit
// tests -- and the end-to-end test belongs here once an image with a writable
// feature set has a hole in it.

/// A directory is not a file, and writing to one must be refused.
#[test]
fn write_to_directory_is_refused() {
    require_fusefs!();
    with_rw_mount(&GOLDENV4, "isdir", |mnt| {
        let dir = mnt.join("files");
        assert!(
            try_open_rw(&dir).is_err(),
            "opening a directory for writing worked"
        );
    });
}

/// A read-write mount of an image whose features cannot be maintained must be
/// refused, while a read-only mount of the same image must work.
///
/// The image is `xfs_4kn.img`: a version 5 file system with reflink, the reverse
/// mapping tree, and big timestamps, all of which the write path cannot keep up
/// to date.  A smaller image with the same features would do, and this one is
/// small, because the point of the test is the refusal and not the copy.
#[test]
fn unsupported_features_refuse_rw_mount() {
    require_fusefs!();
    let image = writable_copy(&GOLDEN4KN, "refuse");
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let seq = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut mnt = mountpoint_dir();
    mnt.push(format!("mnt-refuse-{seq}"));
    std::fs::create_dir_all(&mnt).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_xfs-fuse"));
    command.args(["-f", "-r"]).arg(&image).arg(&mnt);
    let (status, stdout, stderr) = run_bounded(&mut command, "refuse", Duration::from_secs(60));
    assert!(
        !status.map(|s| s.success()).unwrap_or(false),
        "a read-write mount of a reflinked file system should have been refused; it said: \
         {stdout}{stderr}"
    );
    let message = format!("{stdout}{stderr}");
    assert!(
        message.contains("reflink") || message.contains("cannot yet write"),
        "the refusal did not say why: {message}"
    );

    // The same image must still mount read-only, which is the whole point of
    // separating the two questions: a feature that stops xfuse writing must not
    // stop it reading.  This image's root has no "files" directory, so the check
    // is on a name that exists in it.
    with_ro_mount(&image, "refuse-ro", |mnt| {
        assert!(
            mnt.join("xattrs").is_dir(),
            "the image did not mount read-only: {}",
            std::fs::read_dir(mnt).map(|d| d.count()).unwrap_or(0)
        );
    });
}

/// A read-only mount must refuse a write, and must not change the image.
#[test]
fn read_only_mount_refuses_writes() {
    require_fusefs!();
    let image = writable_copy(&GOLDENV4, "ro");
    let before = std::fs::read(&image).expect("reading the image");

    with_ro_mount(&image, "ro-write", |mnt| {
        let err = try_open_rw(&mnt.join("files/hello.txt")).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EROFS), "unexpected: {err:?}");
    });

    assert_eq!(before, std::fs::read(&image).expect("reading the image"));
}

/// A file whose extents no longer fit in its inode grows into a B+tree.
///
/// This is the other direction from the truncate below, and it is the one that
/// needs a **writer**: the extents have to go somewhere the inode cannot reach, and
/// the somewhere is a block taken from the group with a root left behind in the
/// fork.
///
/// The fork in `xfsv4.img` is small — a 512 byte inode with a 24 byte attribute fork
/// holds twelve extent records — so a file has to be pushed well past that before
/// the fork fills.  Growing at the end would join the new blocks to the last extent
/// and never need a thirteenth record, so the test writes at *many* offsets with a
/// gap between them, which is what forces an extent each time.
///
/// And then `xfs_repair -n` has the last word, because "the file still reads" is not
/// the question: the question is whether XFS agrees that the inode, the tree and the
/// group's free space describe the same file system.
#[test]
fn a_file_that_outgrows_its_inode_becomes_a_btree() {
    require_fusefs!();
    let image = writable_copy(&GOLDENV4, "grow-tree");
    const ATTEMPTS: usize = 40;
    // Two blocks apart, so each write is one block and the forty of them are forty
    // extents in about eighty kilobytes.
    //
    // The stride used to be a megabyte, which made this test depend on free space
    // rather than on the code: forty mebibyte of file on a sixty-four mebibyte file
    // system exhausted the group, and FreeBSD took one write where Linux took forty
    // -- the same image, the same code, and a test whose outcome was a function of
    // how much of the file system happened to be left.  A short stride forces an
    // extent per write for the reason that matters, which is that an extent is what
    // a fork has to outgrow, and costs almost nothing.
    const STRIDE: u64 = 4096;
    // ...starting well clear of the first byte, so the check that the original
    // contents survived cannot be confused by a write that legitimately covered
    // them.
    const MARGIN: u64 = 4096;

    // How far it got, and whether the leaf's limit stopped it.  Both are the
    // point: a file that outgrows its inode has to become a tree, and a file that
    // outgrows its **leaf** has to be refused, because the step after one leaf is a
    // second leaf and an interior node and that is not built.
    // **No assertion about whether a limit is reached.**  Two versions of this test
    // had one and both were wrong, in opposite directions: first that the writes
    // *must* fail (when a third leaf was not built), then that they must *succeed*
    // (once an interior node was).  Neither is a property of the code.
    //
    // Whether forty writes fit depends on how much free space the group has and on
    // how the running kernel chops the writes up -- a 4082-byte write arrives as
    // two FUSE writes, and each adds an extent.  That is capacity and transport,
    // not correctness, and FreeBSD hits a limit where Linux does not.
    //
    // What this test is about is that the file's fork became a B+tree, the data
    // survived it, and `xfs_repair` accepts the result.  Those three do not vary by
    // platform, and a refusal at the end -- if there is one -- is an honest answer
    // from the code rather than a failure here.
    let (before, wrote_any, first_byte) = with_rw_mount_at(&image, "grow-tree", |mnt| {
        let file = mnt.join("files/hello.txt");
        let before = std::fs::metadata(&file).unwrap().len();
        let first_byte = std::fs::read(&file).unwrap()[0];
        let mut wrote = 0usize;
        for i in 0..ATTEMPTS {
            let mut f = open_rw(&file);
            if f.seek(SeekFrom::Start(before + MARGIN + i as u64 * STRIDE))
                .is_err()
            {
                break;
            }
            if f.write_all(&[b'x'; 4096]).is_err() {
                break;
            }
            wrote += 1;
        }
        (before, wrote, first_byte)
    });

    assert!(
        wrote_any > ATTEMPTS / 2,
        "only {wrote_any} of {ATTEMPTS} writes landed, which is too few to have crossed the \
         inode's extent capacity and so tests nothing about the conversion"
    );
    assert!(
        before < 1 << 20,
        "the sanity check on the image is wrong: the file started at {before}"
    );

    // The file still reads, and it is longer than it was -- which is the observable
    // consequence of the fork becoming a tree at all.
    with_rw_mount_at(&image, "grow-tree-read", |mnt| {
        let file = mnt.join("files/hello.txt");
        let size = std::fs::metadata(&file).unwrap().len();
        assert!(size > before, "the file did not grow at all");
        let read = std::fs::read(&file).expect("reading the grown file");
        assert_eq!(
            read[0], first_byte,
            "the first byte, which no write in this test touched, changed"
        );
    });

    // And XFS agrees the whole thing describes a real file system.
    xfs_repair_check(&image).unwrap_or_else(|e| panic!("a file grown into a B+tree: {e}"));
}

/// Writing must not damage the file system, as far as XFS's own tools can tell.
#[test]
fn xfs_repair_is_happy() {
    require_fusefs!();
    let image = writable_copy(&GOLDENV4, "repair");
    with_rw_mount_at(&image, "repair", |mnt| {
        // Overwrite a byte in each of several files, including one with many
        // extents and one in a directory of a different format.
        // One file per inode: hello.txt and hello2.txt are two names for one
        // inode in this image, and writing to both would only test the same
        // inode twice.
        for (name, offset) in [
            ("files/hello.txt", 3u64),
            ("files/large_extent.txt", 4096),
            ("files/btree2.2.txt", 1000),
            ("files/btree3.txt", 7000),
            ("files/btree3.3.txt", 0),
        ] {
            let file = mnt.join(name);
            let Ok(meta) = std::fs::metadata(&file) else {
                continue;
            };
            assert!(meta.len() > offset + 8, "{name} is too small to write into");
            let mut f = open_rw(&file);
            f.seek(SeekFrom::Start(offset)).unwrap();
            match f.write_all(b"MODIFIED") {
                Ok(()) => {}
                // A file whose extent mapping is a B+tree is refused, because the
                // record's data-block field is not yet established and a write
                // through a value known to be provisional is a write to an
                // arbitrary block.  Skipping it is the same choice this test already
                // makes for a file it cannot stat, and `files/hello.txt` and
                // `files/large_extent.txt` below are the ones whose mappings *are*
                // in their inodes and are written here.
                Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
                    eprintln!("skipping {name}: its mapping is a B+tree");
                }
                Err(e) => panic!("writing into {name} failed: {e}"),
            }
            drop(f);
        }
    });

    if let Err(e) = xfs_repair_check(&image) {
        panic!("{e}");
    }
    // And the data has to still be there.
    with_ro_mount(&image, "repair-ro", |mnt| {
        let after = read_file(&mnt.join("files/hello.txt"));
        assert_eq!(&after[3..11], b"MODIFIED");
    });
}

/// The modification time of a file must move when it is written, the file must
/// keep its size, and the time must be on the image rather than in the kernel's
/// cache.
#[test]
fn timestamps_and_size() {
    require_fusefs!();
    let image = writable_copy(&GOLDENV4, "times");
    let (before_size, before_time) = with_ro_mount(&image, "times-ro", |mnt| {
        let meta = std::fs::metadata(mnt.join("files/hello.txt")).unwrap();
        (meta.len(), meta.modified().unwrap())
    });

    with_rw_mount_at(&image, "times", |mnt| {
        let file = mnt.join("files/hello.txt");
        let mut f = open_rw(&file);
        // A byte that differs from what is already there: a write of the bytes
        // that are already in the file dirties no page, and the file system
        // never hears about it.
        f.write_all(b"J").unwrap();
    });

    let (after_size, after_time) = with_ro_mount(&image, "times-ro", |mnt| {
        let meta = std::fs::metadata(mnt.join("files/hello.txt")).unwrap();
        (meta.len(), meta.modified().unwrap())
    });
    assert_eq!(
        before_size, after_size,
        "writing changed the size of the file"
    );
    assert!(
        after_time > before_time,
        "the modification time did not move: {before_time:?} -> {after_time:?}"
    );
    // The golden image's hello.txt was written with a decade-old modification
    // time, so any time after the file system was built is a change.  Comparing
    // against the golden value is the check that matters; comparing against
    // "now" would only test the clock.
    let golden_epoch = before_time
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    assert!(
        golden_epoch < 1_500_000_000,
        "the golden image's hello.txt is unexpectedly recent: {before_time:?}"
    );
}

/// create() in a shortform directory must produce a valid file with correct
/// metadata, and the image must pass xfs_repair.
#[test]
fn create_in_shortform_directory() {
    require_fusefs!();
    eprintln!("=== Starting create_in_shortform_directory test ===");
    let image = writable_copy(&GOLDENV4, "create");
    eprintln!("=== Created image copy ===");

    // First, inspect the sf directory to understand its structure
    eprintln!("=== About to mount read-only for inspection ===");
    with_ro_mount(&image, "create-inspect", |mnt| {
        eprintln!("=== Inside read-only mount ===");
        let sf_dir = mnt.join("sf");
        let entries: Vec<_> = std::fs::read_dir(&sf_dir).unwrap().collect();
        eprintln!("sf directory has {} entries:", entries.len());
        for entry in entries {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            use std::os::unix::fs::MetadataExt;
            eprintln!(
                "  {}: ino={}, size={}, mode={:o}",
                entry.file_name().to_string_lossy(),
                meta.ino(),
                meta.len(),
                meta.permissions().mode()
            );
        }
        eprintln!("=== Inspection complete ===");
    });
    eprintln!("=== Inspection mount done ===");

    // The sf/ directory in GOLDENV4 is a shortform directory with 2 entries
    // (created by mkimg.sh: mkfiles ${MNTDIR}/sf 2).  Create a new file there.
    eprintln!("=== About to mount read-write for create ===");
    with_rw_mount_at(&image, "create", |mnt| {
        eprintln!("=== Inside read-write mount ===");
        let new_file = mnt.join("sf/newfile.txt");
        File::create(&new_file).unwrap();
        eprintln!("=== File created ===");
    });
    eprintln!("=== Create mount done ===");

    // Verify the file exists with correct metadata after remount.
    eprintln!("=== About to mount read-only for verification ===");
    with_ro_mount(&image, "create-ro", |mnt| {
        eprintln!("=== Inside verification mount ===");
        let meta = std::fs::metadata(mnt.join("sf/newfile.txt")).unwrap();
        assert_eq!(meta.len(), 0, "new file should have size 0");
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o644 & 0o777,
            "default mode should be 0o644"
        );
        // nlink should be 1
        use std::os::unix::fs::MetadataExt;
        assert_eq!(meta.nlink(), 1, "new file should have nlink=1");
        eprintln!("=== Verification complete ===");
    });
    eprintln!("=== Verification mount done ===");

    // The image must be structurally valid.
    eprintln!("=== Running xfs_repair ===");
    xfs_repair_check(&image).unwrap_or_else(|e| panic!("xfs_repair failed: {e}"));
    eprintln!("=== Test passed ===");
}

/// A golden image that was not decompressed properly must not be silently
/// believed.
///
/// A real-time image that is empty or all zeroes is invisible from inside a
/// mount: the file that uses it has exactly the right size, because the size
/// comes from the other image, and its contents are zeroes.  That is a fixture
/// failure wearing the clothes of a file system failure, and the harness cannot
/// let it.  The staleness check trusts a file that is newer than the compressed
/// image, which is what an interrupted run leaves behind, so the contents have to
/// overrule it.
#[test]
fn a_bad_golden_image_is_extracted_again() {
    require_fusefs!();
    // A real-time image, because it is the one that is not a file system and so
    // is the one a superblock check would reject.
    let image = util::prepare_image("xfs_rt2.img");
    let good = std::fs::read(&image).expect("reading the golden image");
    assert!(good.len() > 1024, "the golden image is implausibly small");

    // The two ways an extraction can go wrong that leave a file that looks
    // current: nothing was written at all, and something was written but the
    // file is zeros.
    for damage in [(0usize, 0usize), (0, good.len())] {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&image)
            .expect("opening the golden image");
        file.set_len(damage.1 as u64).unwrap();
        if damage.0 == 0 && damage.1 > 0 {
            std::fs::write(&image, vec![0u8; damage.1]).unwrap();
        }
        // Preparing it again must notice and fix it, rather than handing the
        // damage to whatever test asks for this image next.
        let again = util::prepare_image("xfs_rt2.img");
        assert_eq!(again, image);
        let after = std::fs::read(&image).expect("reading the golden image again");
        assert_eq!(
            after.len(),
            good.len(),
            "the image was not re-extracted: {} bytes instead of {}",
            after.len(),
            good.len()
        );
        assert!(after.iter().any(|b| *b != 0), "the image is all zeroes");
    }
}

/// A FIFO created with `mknod` must survive a remount and leave a
/// consistent image.
#[test]
#[allow(clippy::unnecessary_cast)]
fn mknod_creates_a_fifo() {
    require_fusefs!();
    let image = writable_copy(&GOLDENV4, "mknod-fifo");
    with_rw_mount_at(&image, "mknod-fifo", |mnt| {
        let path = mnt.join("sf/mydev");
        let cpath = CString::new(path.to_str().unwrap()).unwrap();
        let ret = unsafe { libc::mknod(cpath.as_ptr(), libc::S_IFIFO | 0o644, 0) };
        assert_ne!(ret, -1, "mknod failed: {}", std::io::Error::last_os_error());
    });

    with_ro_mount(&image, "mknod-fifo-ro", |mnt| {
        let path = mnt.join("sf/mydev");
        let meta = std::fs::metadata(&path).expect("the FIFO should exist after remount");
        use std::os::unix::fs::MetadataExt;
        assert!(
            (meta.mode() & libc::S_IFMT as u32) == libc::S_IFIFO as u32,
            "expected a FIFO, got mode {:#o}",
            meta.mode() & 0o7777
        );
    });

    repair_accepts(&image, "after a FIFO was created with mknod");
}

/// A character device created with `mknod` must survive a remount and leave a
/// consistent image.
#[test]
#[allow(clippy::unnecessary_cast)]
fn mknod_creates_a_chrdev() {
    require_fusefs!();
    if !root() {
        skip!("creating char devices requires root");
    }
    let image = writable_copy(&GOLDENV4, "mknod-chrdev");
    with_rw_mount_at(&image, "mknod-chrdev", |mnt| {
        let path = mnt.join("sf/mychar");
        let cpath = CString::new(path.to_str().unwrap()).unwrap();
        let rdev = libc::makedev(1, 3);
        let ret = unsafe { libc::mknod(cpath.as_ptr(), libc::S_IFCHR | 0o600, rdev) };
        assert_ne!(ret, -1, "mknod failed: {}", std::io::Error::last_os_error());
    });

    with_ro_mount(&image, "mknod-chrdev-ro", |mnt| {
        let path = mnt.join("sf/mychar");
        let meta = std::fs::metadata(&path).expect("the char device should exist after remount");
        use std::os::unix::fs::MetadataExt;
        assert!(
            (meta.mode() & libc::S_IFMT as u32) == libc::S_IFCHR as u32,
            "expected a character device, got mode {:#o}",
            meta.mode() & 0o7777
        );
        assert_eq!(
            meta.rdev(),
            libc::makedev(1, 3) as u64,
            "the rdev did not survive the remount"
        );
    });

    repair_accepts(&image, "after a char device was created with mknod");
}

/// A block device created with `mknod` must survive a remount and leave a
/// consistent image.
#[test]
#[allow(clippy::unnecessary_cast)]
fn mknod_creates_a_blockdev() {
    require_fusefs!();
    if !root() {
        skip!("creating block devices requires root");
    }
    let image = writable_copy(&GOLDENV4, "mknod-blkdev");
    with_rw_mount_at(&image, "mknod-blkdev", |mnt| {
        let path = mnt.join("sf/mybdev");
        let cpath = CString::new(path.to_str().unwrap()).unwrap();
        let rdev = libc::makedev(8, 0);
        let ret = unsafe { libc::mknod(cpath.as_ptr(), libc::S_IFBLK | 0o660, rdev) };
        assert_ne!(ret, -1, "mknod failed: {}", std::io::Error::last_os_error());
    });

    with_ro_mount(&image, "mknod-blkdev-ro", |mnt| {
        let path = mnt.join("sf/mybdev");
        let meta = std::fs::metadata(&path).expect("the block device should exist after remount");
        use std::os::unix::fs::MetadataExt;
        assert!(
            (meta.mode() & libc::S_IFMT as u32) == libc::S_IFBLK as u32,
            "expected a block device, got mode {:#o}",
            meta.mode() & 0o7777
        );
        assert_eq!(
            meta.rdev(),
            libc::makedev(8, 0) as u64,
            "the rdev did not survive the remount"
        );
    });

    repair_accepts(&image, "after a block device was created with mknod");
}

/// `mknod` with a regular-file mode must be refused, because mknod does not
/// create regular files.
#[test]
fn mknod_refuses_regular_file() {
    require_fusefs!();
    let image = writable_copy(&GOLDENV4, "mknod-reg");
    with_rw_mount_at(&image, "mknod-reg", |mnt| {
        let path = mnt.join("sf/regular");
        let cpath = CString::new(path.to_str().unwrap()).unwrap();
        let ret = unsafe { libc::mknod(cpath.as_ptr(), libc::S_IFREG | 0o644, 0) };
        assert_eq!(ret, -1, "mknod with S_IFREG should have been refused");
        let err = std::io::Error::last_os_error();
        assert_eq!(
            err.raw_os_error(),
            Some(libc::EINVAL),
            "expected EINVAL for a regular file mode, got: {err}"
        );
    });
}
