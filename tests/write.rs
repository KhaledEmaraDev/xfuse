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
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use util::{writable_copy, GOLDEN4KN, GOLDENV4};

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
                .stderr(Stdio::null())
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
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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

/// A write that would run past the end of the file must be refused, and must
/// leave the file untouched.
#[test]
fn write_past_eof_is_refused() {
    require_fusefs!();
    let image = writable_copy(&GOLDENV4, "past-eof");
    let (size, before) = with_rw_mount_at(&image, "past-eof", |mnt| {
        let file = mnt.join("files/hello.txt");
        let before = read_file(&file);
        let size = before.len();
        let mut f = open_rw(&file);
        f.seek(SeekFrom::End(0)).unwrap();
        let result = f.write_all(b"more data than fits");
        drop(f);

        // How the refusal arrives is the kernel's business as much as ours: a
        // kernel that finds the write unacceptable before it reaches the file
        // system reports a short write rather than the file system's EFBIG, and
        // both are a refusal.  A write reported as a success is not.
        match result {
            Ok(()) => panic!(
                "a write of 21 bytes past the end of a {size} byte file was reported as succeeding"
            ),
            Err(e) => {
                let acceptable =
                    e.raw_os_error() == Some(libc::EFBIG) || e.raw_os_error().is_none();
                assert!(
                    acceptable,
                    "writing past the end of the file failed with an error that is not a refusal: \
                     {e:?}"
                );
            }
        }
        (size, before)
    });

    // What the file looks like *through the mount that tried the write* is not
    // the question.  A kernel may extend its own idea of the file's size on its
    // way to the write -- it has to, to turn a write past the end into a write
    // at the end -- and then hand us a request we refuse, leaving the kernel
    // with a larger size than the file system has and padding reads to match.
    // That disagreement is the kernel's, and it lasts only as long as the mount.
    //
    // The question is whether anything was written, and that is what a second,
    // read-only mount with no cached size of its own can answer.
    with_ro_mount(&image, "past-eof-ro", |mnt| {
        let after = read_file(&mnt.join("files/hello.txt"));
        assert_eq!(
            after.len(),
            size,
            "a refused write changed the file on the image"
        );
        assert_eq!(
            after, before,
            "a refused write changed the file on the image"
        );
    });
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
            f.write_all(b"MODIFIED").unwrap();
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
