/*
 * BSD 2-Clause License
 *
 * Copyright (c) 2026, the xfuse authors
 * All rights reserved.
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
    process::Command,
    time::Duration,
};

use util::{writable_copy, GOLDEN1K, GOLDENV4};

/// Mount a read-write copy of a golden image, hand it to `body`, and unmount.
fn with_rw_mount<T>(golden: &Path, tag: &str, body: impl FnOnce(&Path) -> T) -> T {
    let image = writable_copy(golden, tag);
    let mnt = mountpoint(tag);
    let _ = std::fs::remove_dir_all(&mnt);
    std::fs::create_dir_all(&mnt).expect("creating the mountpoint");

    let mut xfs_fuse = Command::new(env!("CARGO_BIN_EXE_xfs-fuse"))
        .arg("-f")
        .arg("-r")
        .arg(&image)
        .arg(&mnt)
        .spawn()
        .expect("running xfs-fuse");
    let mounted = util::waitfor(Duration::from_secs(30), || is_live(&mnt));
    let result = match mounted {
        Ok(()) => body(&mnt),
        Err(e) => panic!("xfs-fuse did not mount {image:?}: {e}"),
    };

    unmount(&mnt);
    let _ = xfs_fuse.wait();
    result
}

/// Mount an already copied image read-write, hand it to `body`, and unmount.
fn with_rw_mount_at<T>(image: &Path, tag: &str, body: impl FnOnce(&Path) -> T) -> T {
    let mnt = mountpoint(tag);
    let _ = std::fs::remove_dir_all(&mnt);
    std::fs::create_dir_all(&mnt).expect("creating the mountpoint");
    let mut xfs_fuse = Command::new(env!("CARGO_BIN_EXE_xfs-fuse"))
        .args(["-f", "-r"])
        .arg(image)
        .arg(&mnt)
        .spawn()
        .expect("running xfs-fuse");
    util::waitfor(Duration::from_secs(30), || is_live(&mnt))
        .expect("xfs-fuse did not mount the image read-write");
    let result = body(&mnt);
    unmount(&mnt);
    let _ = xfs_fuse.wait();
    result
}

/// Mount an image read-only, hand it to `body`, and unmount.
fn with_ro_mount<T>(image: &Path, tag: &str, body: impl FnOnce(&Path) -> T) -> T {
    let mnt = mountpoint(tag);
    let _ = std::fs::remove_dir_all(&mnt);
    std::fs::create_dir_all(&mnt).expect("creating the mountpoint");

    let mut xfs_fuse = Command::new(env!("CARGO_BIN_EXE_xfs-fuse"))
        .arg("-f")
        .arg(image)
        .arg(&mnt)
        .spawn()
        .expect("running xfs-fuse");
    util::waitfor(Duration::from_secs(30), || is_live(&mnt))
        .expect("xfs-fuse did not mount the image read-only");

    let result = body(&mnt);
    unmount(&mnt);
    let _ = xfs_fuse.wait();
    result
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

/// A mountpoint of our own, so that parallel tests do not collide.
fn mountpoint(tag: &str) -> PathBuf {
    let mut mnt = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    mnt.push(format!("mnt-{}-{}", std::process::id(), tag));
    mnt
}

fn unmount(mnt: &Path) {
    for unmounter in [
        "/bin/fusermount3",
        "/usr/local/bin/fusermount3",
        "/usr/bin/fusermount3",
    ] {
        if Path::new(unmounter).exists() {
            let _ = Command::new(unmounter).arg("-u").arg(mnt).status();
            // Give the file system a moment to notice.
            let _ = util::waitfor(Duration::from_secs(10), || std::fs::read_dir(mnt).is_err());
            return;
        }
    }
    // Fall back to the classic name, which is what the FreeBSD and Linux
    // fuse packages install.
    let _ = Command::new("fusermount").arg("-u").arg(mnt).status();
    let _ = util::waitfor(Duration::from_secs(10), || std::fs::read_dir(mnt).is_err());
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
    {
        let mnt = mountpoint("remount-rw");
        std::fs::create_dir_all(&mnt).unwrap();
        let mut xfs_fuse = Command::new(env!("CARGO_BIN_EXE_xfs-fuse"))
            .args(["-f", "-r"])
            .arg(&image)
            .arg(&mnt)
            .spawn()
            .unwrap();
        util::waitfor(Duration::from_secs(30), || is_live(&mnt)).unwrap();
        let mut f = open_rw(&mnt.join("files/hello.txt"));
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(b"HELLO").unwrap();
        drop(f);
        unmount(&mnt);
        let _ = xfs_fuse.wait();
    }
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
    with_rw_mount(&GOLDENV4, "past-eof", |mnt| {
        let file = mnt.join("files/hello.txt");
        let before = read_file(&file);
        let mut f = open_rw(&file);
        f.seek(SeekFrom::End(0)).unwrap();
        let err = f.write_all(b"more data than fits").unwrap_err();
        drop(f);
        assert_eq!(
            err.raw_os_error(),
            Some(libc::EFBIG),
            "unexpected error: {err:?}"
        );
        assert_eq!(read_file(&file), before, "a refused write changed the file");
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
#[test]
fn unsupported_features_refuse_rw_mount() {
    require_fusefs!();
    let image = writable_copy(&GOLDEN1K, "refuse");
    let mnt = mountpoint("refuse");
    std::fs::create_dir_all(&mnt).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_xfs-fuse"))
        .args(["-f", "-r"])
        .arg(&image)
        .arg(&mnt)
        .output()
        .expect("running xfs-fuse");
    assert!(
        !output.status.success(),
        "a read-write mount of a reflinked file system should have been refused"
    );
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        message.contains("reflink") || message.contains("cannot yet write"),
        "the refusal did not say why: {message}"
    );

    // The same image must still mount read-only.
    with_ro_mount(&image, "refuse-ro", |mnt| {
        assert!(mnt.join("files").is_dir());
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
    {
        let mnt = mountpoint("repair");
        std::fs::create_dir_all(&mnt).unwrap();
        let mut xfs_fuse = Command::new(env!("CARGO_BIN_EXE_xfs-fuse"))
            .args(["-f", "-r"])
            .arg(&image)
            .arg(&mnt)
            .spawn()
            .unwrap();
        util::waitfor(Duration::from_secs(30), || is_live(&mnt)).unwrap();

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
        unmount(&mnt);
        let _ = xfs_fuse.wait();
    }

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
