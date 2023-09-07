/**
 * BSD 2-Clause License
 *
 * Copyright (c) 2021, Khaled Emara
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
use fuser::FileType;

use super::dir3::{XFS_DIR3_FT_DIR, XFS_DIR3_FT_REG_FILE, XFS_DIR3_FT_SYMLINK};

use libc::{c_int, mode_t, ENOENT, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use bincode::{
    Decode,
    de::{
        Decoder,
        read::Reader
    },
    error::DecodeError,
    impl_borrow_decode
};
use tracing::error;

/// xfs-fuse UUID type
/// 
/// This is just like the `Uuid` from the `uuid` crate, except that it
/// serializes as a fixed-size array instead of a slice
// The Uuid crate serializes to a slice, and its maintainers have ruled out ever
// serializing to a fixed-size array instead.
// See Also [Uuid #557](https://github.com/uuid-rs/uuid/issues/557)
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct Uuid(uuid::Uuid);

impl Uuid {
    pub const fn from_u128(x: u128) -> Self {
        Self(uuid::Uuid::from_u128(x))
    }

    pub const fn from_uuid(uuid: uuid::Uuid) -> Self {
        Self(uuid)
    }

    pub fn nil() -> Self {
        Self(uuid::Uuid::nil())
    }

    pub fn parse_str(input: &str) -> std::result::Result<Uuid, uuid::Error> {
        uuid::Uuid::parse_str(input).map(Uuid)
    }
}

impl bincode::Decode for Uuid {
    fn decode<D: Decoder>(decoder: &mut D) -> Result<Self, DecodeError> {
        <[u8; 16]>::decode(decoder)
        .map(|v| Uuid(uuid::Uuid::from_bytes(v)))
    }
}
impl_borrow_decode!(Uuid);

pub enum FileKind {
    Type(u8),
    Mode(u16),
}

pub fn get_file_type(kind: FileKind) -> Result<FileType, c_int> {
    match kind {
        FileKind::Type(file_type) => match file_type {
            XFS_DIR3_FT_REG_FILE => Ok(FileType::RegularFile),
            XFS_DIR3_FT_DIR => Ok(FileType::Directory),
            XFS_DIR3_FT_SYMLINK => Ok(FileType::Symlink),
            _ => {
                error!("Unknown file type {:?}.", file_type);
                Err(ENOENT)
            }
        },
        FileKind::Mode(file_mode) => match (file_mode as mode_t) & S_IFMT {
            S_IFREG => Ok(FileType::RegularFile),
            S_IFDIR => Ok(FileType::Directory),
            S_IFLNK => Ok(FileType::Symlink),
            _ => {
                error!("Unknown file type {:?}.", (file_mode as mode_t) & S_IFMT);
                Err(ENOENT)
            }
        },
    }
}

/// Decode a Bincode structure from a byte slice.
pub fn decode<T>(bytes: &[u8]) -> Result<(T, usize), DecodeError>
    where T: Decode
{
    let config = bincode::config::standard()
        .with_big_endian()
        .with_fixed_int_encoding();
    bincode::decode_from_slice(bytes, config)
}

/// Decode a Bincode structure from a Reader
pub fn decode_from<T, R>(r: R) -> Result<T, DecodeError>
    where T: Decode,
          R: Reader
{
    let config = bincode::config::standard()
        .with_big_endian()
        .with_fixed_int_encoding();
    bincode::decode_from_reader(r, config)
}
