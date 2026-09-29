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
//! The error type used by everything that can modify the file system.
//!
//! A read-only file system can be sloppy about errors, because a mistake costs
//! nothing but a wrong answer.  A writable one cannot: an operation that fails
//! half way through has already changed the image, and the caller has to be
//! able to tell "the caller asked for something impossible" apart from "the
//! image did not do what we told it to".
//!
//! [`FsError`] therefore separates those two cases.  A corrupt structure is
//! [`FsError::Corrupt`], which means the file system must not be written to
//! again until it has been repaired; a failed device write is
//! [`FsError::Io`], which means the transaction that was running did not
//! complete.  Neither of them is a reason to panic, and neither of them may be
//! reported to FUSE as success.

use std::{fmt, io, num::TryFromIntError, path::PathBuf};

use nix::errno;

/// An error produced by the file system.
///
/// The variants carry both an `errno` value, so that the FUSE layer can hand
/// the error to the kernel unchanged, and enough context to be logged.
#[derive(Debug)]
pub enum FsError {
    /// The caller asked for something the file system will not do: a write
    /// past the end of a file, an unknown feature, a write to a directory.
    /// Carries the `errno` to report and a description for the log.
    Invalid { errno: i32, msg: String },
    /// The name does not exist.
    NoEntry { name: Vec<u8> },
    /// The name already exists.
    Exists { name: Vec<u8> },
    /// A directory that was supposed to be empty was not.
    NotEmpty { path: PathBuf },
    /// The file system is full.
    NoSpace,
    /// The file system was mounted read-only, or the operation needs a feature
    /// that this implementation cannot maintain.
    ReadOnly { msg: String },
    /// The operation needs a feature that has not been implemented yet.
    Unsupported { feature: String },
    /// On-disk metadata did not make sense.  The file system is damaged.
    Corrupt { what: String },
    /// The device or the operating system failed.
    Io(io::Error),
}

impl FsError {
    /// Build an [`FsError::Invalid`].
    pub fn invalid(errno: i32, msg: impl Into<String>) -> Self {
        FsError::Invalid {
            errno,
            msg: msg.into(),
        }
    }

    /// Build an [`FsError::Corrupt`].
    pub fn corrupt(what: impl Into<String>) -> Self {
        FsError::Corrupt { what: what.into() }
    }

    /// Build an [`FsError::ReadOnly`].
    pub fn read_only(msg: impl Into<String>) -> Self {
        FsError::ReadOnly { msg: msg.into() }
    }

    /// Build an [`FsError::Unsupported`].
    pub fn unsupported(feature: impl Into<String>) -> Self {
        FsError::Unsupported {
            feature: feature.into(),
        }
    }

    /// The `errno` that this error should be reported to FUSE as.
    pub fn errno(&self) -> i32 {
        match self {
            FsError::Invalid { errno, .. } => *errno,
            FsError::NoEntry { .. } => libc::ENOENT,
            FsError::Exists { .. } => libc::EEXIST,
            FsError::NotEmpty { .. } => libc::ENOTEMPTY,
            FsError::NoSpace => libc::ENOSPC,
            FsError::ReadOnly { .. } => libc::EROFS,
            FsError::Unsupported { .. } => libc::ENOSYS,
            FsError::Corrupt { .. } => crate::libxfuse::EUCLEAN,
            FsError::Io(e) => e.raw_os_error().unwrap_or(libc::EIO),
        }
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FsError::Invalid { errno, msg } => {
                write!(f, "{} (errno {})", msg, errno)
            }
            FsError::NoEntry { name } => {
                write!(
                    f,
                    "no such file or directory: {}",
                    String::from_utf8_lossy(name)
                )
            }
            FsError::Exists { name } => {
                write!(f, "file exists: {}", String::from_utf8_lossy(name))
            }
            FsError::NotEmpty { path } => {
                write!(f, "directory not empty: {}", path.display())
            }
            FsError::NoSpace => write!(f, "no space left on device"),
            FsError::ReadOnly { msg } => write!(f, "read-only file system: {msg}"),
            FsError::Unsupported { feature } => {
                write!(f, "not supported: {feature}")
            }
            FsError::Corrupt { what } => write!(f, "corrupt file system: {what}"),
            FsError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for FsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FsError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for FsError {
    fn from(e: io::Error) -> Self {
        // A short read means a file system structure ran off the end of the
        // image.  That is damage, not a transient failure, and it must not be
        // mistaken for an ordinary I/O error.
        if e.kind() == io::ErrorKind::UnexpectedEof {
            FsError::Corrupt {
                what: "structure extends past the end of the image".into(),
            }
        } else {
            FsError::Io(e)
        }
    }
}

impl From<i32> for FsError {
    /// Wrap a bare `errno` from the read path, which still reports its failures
    /// that way.
    fn from(errno: i32) -> Self {
        FsError::Invalid {
            errno,
            msg: format!("operation failed: {}", errno::Errno::from_raw(errno)),
        }
    }
}

impl From<TryFromIntError> for FsError {
    fn from(e: TryFromIntError) -> Self {
        FsError::Invalid {
            errno: libc::EINVAL,
            msg:   e.to_string(),
        }
    }
}

/// The result type used by the write path.
pub type FsResult<T> = Result<T, FsError>;

/// Convenience for a name that could not be found.
pub fn no_entry(name: &[u8]) -> FsError {
    FsError::NoEntry {
        name: name.to_vec(),
    }
}

#[cfg(test)]
mod t {
    use super::*;

    #[test]
    fn errno_mapping() {
        assert_eq!(FsError::NoSpace.errno(), libc::ENOSPC);
        assert_eq!(FsError::read_only("x").errno(), libc::EROFS);
        assert_eq!(FsError::unsupported("reflink").errno(), libc::ENOSYS);
        assert_eq!(FsError::corrupt("x").errno(), crate::libxfuse::EUCLEAN);
        assert_eq!(FsError::invalid(libc::EISDIR, "x").errno(), libc::EISDIR);
        assert_eq!(
            FsError::from(io::Error::from_raw_os_error(libc::EIO)).errno(),
            libc::EIO
        );
    }

    /// A read that ran off the end of the image is damage to the file system,
    /// and must not be reported as a plain I/O error.
    #[test]
    fn short_read_is_corruption() {
        let e: FsError = io::Error::new(io::ErrorKind::UnexpectedEof, "boom").into();
        assert!(matches!(e, FsError::Corrupt { .. }));
        assert_eq!(e.errno(), crate::libxfuse::EUCLEAN);
    }

    /// Messages must name the problem, since they end up in the log and in the
    /// reason a read-write mount is refused.
    #[test]
    fn messages_are_useful() {
        let s = FsError::unsupported("reflink").to_string();
        assert!(s.contains("reflink"));
        let s = FsError::NoEntry {
            name: b"foo".to_vec(),
        }
        .to_string();
        assert!(s.contains("foo"));
    }
}
