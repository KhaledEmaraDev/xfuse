/*
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

use bincode_next::{de::Decoder, error::DecodeError, Decode, Encode};

use super::definitions::*;
use super::inode::EXTENT_REC_SIZE;

#[derive(Debug, Clone, Copy)]
pub struct BmbtRec {
    pub br_startoff: XfsFileoff,
    pub br_startblock: XfsFsblock,
    pub br_blockcount: XfsFilblks,
    /// If set, indicates that the extent has been preallocated but has not yet been written
    /// (unwritten extent)
    pub br_flag: bool,
}

impl BmbtRec {
    /// Decode a `BmbtRec` from a bincode decoder.
    ///
    /// Extent records are always 16 bytes on disk (two `__be64` fields: `l0`
    /// and `l1`), for both v4 and v5 inodes.  The `is_v5` parameter is kept
    /// for API compatibility but does not change the record format — NREXT64
    /// only affects how the extent *count* is stored in the inode, not each
    /// record's size.
    pub fn decode_with_version<D: Decoder>(
        decoder: &mut D,
        _is_v5: bool,
    ) -> Result<BmbtRec, DecodeError> {
        let br: u128 = Decode::decode(decoder)?;
        let br_blockcount = (br & ((1u128 << 21) - 1)) as u64;
        let br = br >> 21;
        let br_startblock = (br & ((1u128 << 52) - 1)) as u64;
        let br = br >> 52;
        let br_startoff = (br & ((1u128 << 54) - 1)) as u64;
        let br_flag = (br >> 54) != 0;
        Ok(BmbtRec {
            br_startoff,
            br_startblock,
            br_blockcount,
            br_flag,
        })
    }

    /// Encode this `BmbtRec` to a bincode encoder.
    pub fn encode_with_version<E: bincode_next::enc::Encoder>(
        self,
        encoder: &mut E,
        _is_v5: bool,
    ) -> Result<(), bincode_next::error::EncodeError> {
        let mut br: u128 = 0;
        br |= (self.br_blockcount as u128) & ((1u128 << 21) - 1);
        br |= ((self.br_startblock as u128) & ((1u128 << 52) - 1)) << 21;
        br |= ((self.br_startoff as u128) & ((1u128 << 54) - 1)) << (21 + 52);
        if self.br_flag {
            br |= 1u128 << (21 + 52 + 54);
        }
        br.encode(encoder)
    }

    /// Decode a `BmbtRec` from a byte slice.
    pub fn from_bytes(bytes: &[u8]) -> Option<BmbtRec> {
        if bytes.len() < EXTENT_REC_SIZE {
            return None;
        }
        let br = u128::from_be_bytes(bytes[..EXTENT_REC_SIZE].try_into().unwrap());
        let br_blockcount = (br & ((1u128 << 21) - 1)) as u64;
        let br = br >> 21;
        let br_startblock = (br & ((1u128 << 52) - 1)) as u64;
        let br = br >> 52;
        let br_startoff = (br & ((1u128 << 54) - 1)) as u64;
        let br_flag = (br >> 54) != 0;
        Some(BmbtRec {
            br_startoff,
            br_startblock,
            br_blockcount,
            br_flag,
        })
    }

    /// Encode this `BmbtRec` into a byte slice.
    pub fn to_bytes(self, bytes: &mut [u8]) {
        let mut br: u128 = 0;
        br |= (self.br_blockcount as u128) & ((1u128 << 21) - 1);
        br |= ((self.br_startblock as u128) & ((1u128 << 52) - 1)) << 21;
        br |= ((self.br_startoff as u128) & ((1u128 << 54) - 1)) << (21 + 52);
        if self.br_flag {
            br |= 1u128 << (21 + 52 + 54);
        }
        bytes[..EXTENT_REC_SIZE].copy_from_slice(&br.to_be_bytes());
    }
}

/// An ordered list of [`BmbtRec`].
#[derive(Debug, Clone)]
pub struct Bmx(Vec<BmbtRec>);

impl Bmx {
    pub fn new<'a, I>(bmx: I) -> Self
    where
        I: IntoIterator<Item = &'a BmbtRec>,
    {
        // Do not filter out unwritten extents; they should be visible to the
        // write path so it can convert them to written when data is written.
        let bmx = bmx.into_iter().cloned().collect();
        Self(bmx)
    }

    /// The extent records, in ascending order of their offset within the file.
    pub fn extents(&self) -> &[BmbtRec] {
        &self.0
    }

    /// The extent records, for modification.
    pub fn extents_mut(&mut self) -> &mut Vec<BmbtRec> {
        &mut self.0
    }

    /// Return the extent, if any, that contains the given block within the file.
    /// Return its starting position as an FSblock, and its length in file system block units.
    /// If a hole's length extends to EoF, return None for length.
    pub fn get_extent(&self, dblock: XfsFileoff) -> (Option<XfsFsblock>, Option<u64>) {
        match self.0.partition_point(|entry| entry.br_startoff <= dblock) {
            0 => {
                // A hole at the beginning of the file
                let len = self.0.first().map(|b| b.br_startoff - dblock);
                (None, len)
            }
            i => {
                let entry = &self.0[i - 1];
                let skip = dblock - entry.br_startoff;
                if entry.br_startoff + entry.br_blockcount > dblock {
                    // Return the extent even if it's unwritten (br_flag=true).
                    // The caller is responsible for handling unwritten extents.
                    (
                        Some(entry.br_startblock + skip),
                        Some(entry.br_blockcount - skip),
                    )
                } else {
                    // It's a hole
                    let len = self
                        .0
                        .get(i)
                        .map(|e| e.br_startoff - entry.br_startoff - skip);
                    (None, len)
                }
            }
        }
    }

    pub fn first(&self) -> Option<&BmbtRec> {
        self.0.first()
    }

    pub fn lseek(&self, offset: u64, whence: i32) -> Result<u64, i32> {
        let sb = super::volume::try_superblock().ok_or(libc::ENODEV)?;

        let dblock = offset >> sb.sb_blocklog;
        // Filter out unwritten extents for lseek purposes (they read as zeroes)
        let written_extents: Vec<&BmbtRec> = self.0.iter().filter(|e| !e.br_flag).collect();

        match written_extents.partition_point(|entry| entry.br_startoff <= dblock) {
            0 => {
                // A hole at the beginning of the file
                if whence == libc::SEEK_HOLE {
                    Ok(offset)
                } else {
                    written_extents
                        .first()
                        .map(|b| b.br_startoff << sb.sb_blocklog)
                        .ok_or(libc::ENXIO)
                }
            }
            i => {
                let cur_entry = written_extents[i - 1];
                let br_end = cur_entry.br_startoff + cur_entry.br_blockcount;
                if dblock < br_end {
                    // In a data region
                    if whence == libc::SEEK_HOLE {
                        // Scan for the next hole
                        for j in (i - 1)..written_extents.len().saturating_sub(1) {
                            let before = written_extents[j];
                            let after = written_extents[j + 1];
                            let br_end = before.br_startoff + before.br_blockcount;
                            if after.br_startoff > br_end {
                                return Ok(br_end << sb.sb_blocklog);
                            }
                        }
                        // Reached EOF without finding another hole.  Return the virtual hole at
                        // EOF
                        let entry = written_extents.last().unwrap();
                        let br_end = entry.br_startoff + entry.br_blockcount;
                        Ok(br_end << sb.sb_blocklog)
                    } else {
                        Ok(offset)
                    }
                } else {
                    // In a hole
                    if whence == libc::SEEK_HOLE {
                        Ok(offset)
                    } else {
                        match written_extents.get(i) {
                            Some(next_entry) => Ok(next_entry.br_startoff << sb.sb_blocklog),
                            None => Err(libc::ENXIO),
                        }
                    }
                }
            }
        }
    }

    pub fn map_dblock(&self, dblock: XfsDablk) -> Option<XfsFsblock> {
        let dblock = XfsFileoff::from(dblock);
        let i = self.0.partition_point(|rec| rec.br_startoff <= dblock);
        let rec = &self.0[i - 1];
        if i == 0 || rec.br_startoff > dblock || rec.br_startoff + rec.br_blockcount <= dblock {
            None
        } else {
            Some(rec.br_startblock + dblock - rec.br_startoff)
        }
    }
}

impl<I: IntoIterator<Item = BmbtRec>> From<I> for Bmx {
    // Do not filter out unwritten extents; they should be visible to the
    // write path so it can convert them to written when data is written.
    fn from(i: I) -> Self {
        let bmx = i.into_iter().collect();
        Self(bmx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_dblock() {
        let bmx = Bmx::new(&[
            BmbtRec {
                br_startoff: 0,
                br_startblock: 20,
                br_blockcount: 2,
                br_flag: false,
            },
            BmbtRec {
                br_startoff: 2,
                br_startblock: 30,
                br_blockcount: 3,
                br_flag: false,
            },
            BmbtRec {
                br_startoff: 5,
                br_startblock: 40,
                br_blockcount: 2,
                br_flag: false,
            },
        ]);

        assert_eq!(bmx.map_dblock(6), Some(41));
    }

    /// Verify that the 16-byte record format matches the on-disk `xfs_bmbt_rec`
    /// layout from `xfs_format.h`: `l0` carries the flag and startoff in its
    /// high bits, `l0` and `l1` together carry startblock, and `l1` carries
    /// blockcount in its low 21 bits.  This layout is identical for v4 and v5
    /// inodes — NREXT64 only changes how the *count* is stored in the inode,
    /// not the record size — so a round-trip through `from_bytes`/`to_bytes`
    /// must reproduce the exact bytes.
    #[test]
    fn round_trip_16_byte_record() {
        let rec = BmbtRec {
            br_startoff: 0,
            br_startblock: 2272,
            br_blockcount: 8,
            br_flag: false,
        };
        let mut buf = [0u8; EXTENT_REC_SIZE];
        rec.to_bytes(&mut buf);
        assert_eq!(&buf[..], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0x1c, 0, 0, 8]);
        let decoded = BmbtRec::from_bytes(&buf).unwrap();
        assert_eq!(decoded.br_startoff, 0);
        assert_eq!(decoded.br_startblock, 2272);
        assert_eq!(decoded.br_blockcount, 8);
        assert!(!decoded.br_flag);
    }
}
