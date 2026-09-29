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
//! The allocation machinery: the per-group headers and the blocks they point
//! at.
//!
//! An XFS file system is a set of equal-sized *allocation groups*.  Every file
//! and every directory in the file system has its blocks spread across the
//! groups, and each group is independently responsible for saying which of its
//! blocks are free.  A file that is being written to needs blocks from
//! somewhere, and this is where "somewhere" is found out.
//!
//! The three structures here are the whole of a group's fixed header, and they
//! are the only metadata in a file system that can be found without walking
//! the file system first:
//!
//! | Module | What it is |
//! |:-------|:-----------|
//! | [`agf`] | the group file: the group's size, its free space, and where that free space is indexed |
//! | [`agfl`] | the group free list: a small, pre-extracted supply of free blocks |
//! | [`free_space`] | the btrees the group's free space is indexed in, one node of them |
//!
//! # Why a group has two free space indexes
//!
//! A group's free space is not a bitmap of blocks but a list of *runs*: this
//! run of 400 free blocks starting here, that run of 9 starting there.  Two
//! questions are asked of that list often enough to be worth two indexes: "is
//! there a run that starts at or after this block", and "is there a run at
//! least this long".  The first is answered by the btree keyed by start block,
//! the second by the one keyed by run length, and a group header points at the
//! root of each.
//!
//! # How an allocation is meant to work
//!
//! 1. Choose a group.  A group that cannot satisfy the request from its
//!    [`agf::Agf::longest_free`] is skipped without being read any further.
//! 2. Take blocks from the group's free list
//!    ([`agfl::Agfl`]).  This is a flat array, so it is cheap, and the group
//!    header's window says which part of it is live.
//! 3. If the free list cannot supply the blocks, refill it from the free space
//!    btree: walk the btree for a run that is long enough, take the blocks out
//!    of that run in the btree, and put them in the free list.
//! 4. Move the window in the group header, and write the group header and the
//!    free list through the transaction.
//!
//! Steps 2 and 4 are what the free list is *for*: they turn "search a btree"
//! into "move three fields".  A group whose free list is empty still works; it
//! just has to do step 3 first, which is the common case after a file system
//! has been filled and emptied a few times.
//!
//! # What this module does not do yet
//!
//! Freeing a block is not here.  Returning blocks to a group means editing the
//! free space btree -- inserting a run, merging it with its neighbours, and
//! correcting the group's count and longest-run summaries -- and that belongs
//! with the operations that actually free blocks, rather than being written
//! before anything calls it.

pub mod agf;
pub mod agfl;
pub mod free_space;
