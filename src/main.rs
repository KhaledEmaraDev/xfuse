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
use std::path::PathBuf;

use clap::{crate_version, Parser};
use fuser::{mount2, MountOption};
use libxfuse::volume::Volume;
use nix::unistd::daemon;
use tracing_subscriber::EnvFilter;

mod libxfuse;

#[derive(Parser, Clone, Debug)]
#[clap(version = crate_version!())]
struct App {
    /// Mount options, comma delimited.
    #[clap(short = 'o', long, value_delimiter(','))]
    options:    Vec<String>,
    device:     PathBuf,
    mountpoint: String,

    /// Run in the foreground
    #[arg(short)]
    foreground: bool,

    /// Mount read-write.
    ///
    /// Experimental: xfuse has no journal yet, so a machine that loses power
    /// in the middle of a write can leave the filesystem inconsistent.  A
    /// read-write mount is refused outright if the image uses a feature that
    /// xfuse cannot keep up to date.
    #[arg(long = "experimental-rw", short = 'r')]
    experimental_rw: bool,
}

fn main() {
    // `EnvFilter::from_default_env()` with `RUST_LOG` unset builds a filter that
    // matches **nothing**, so every `warn!` in the library is discarded before it
    // reaches stderr and only `eprintln!` output is visible.  That is worse than
    // it sounds: this is a file system whose refusals carry their reasons --
    // "this inode's data fork is full and here are the two numbers" is the
    // difference between a debuggable limit and `ENOSPC` from the void -- and a
    // diagnostic nobody can see is not one.
    //
    // So the default is `warn`: everything the code thinks is worth saying, and
    // nothing louder.  `info` and `debug` stay opt-in, because a FUSE daemon is
    // not a debugging session by default.  `RUST_LOG` still overrides all of it.
    //
    // The variable is read **directly** rather than through
    // `try_from_default_env`, because that returns `Ok` with a filter matching
    // *nothing* when `RUST_LOG` is unset -- so a `unwrap_or_else` around it never
    // runs, and the default silently stays silent.  That is the same failure this
    // is fixing, one layer down.
    let filter = std::env::var("RUST_LOG")
        .ok()
        .filter(|d| !d.trim().is_empty());
    let filter = filter
        .as_deref()
        .map_or_else(|| EnvFilter::new("warn"), EnvFilter::new);
    tracing_subscriber::fmt()
        .pretty()
        .with_env_filter(filter)
        .init();

    let app = App::parse();

    let mut opts = vec![
        MountOption::FSName("fusefs".to_string()),
        MountOption::Subtype("xfs".to_string()),
    ];
    if app.experimental_rw {
        eprintln!(
            "warning: mounting read-write.  This mode is experimental and is NOT crash safe: \
             xfuse has no log yet, so a machine that loses power in the middle of a write can \
             leave the filesystem inconsistent.  Use it for testing only."
        );
        opts.push(MountOption::RW);
    } else {
        opts.push(MountOption::RO);
    }
    // geteuid is always safe
    if unsafe { libc::geteuid() } == 0 {
        opts.push(MountOption::AllowOther);
        opts.push(MountOption::DefaultPermissions);
    }
    let mut rtdev: Option<PathBuf> = None;
    for o in app.options.iter() {
        if let Some((lhs, rhs)) = o.split_once('=') {
            if lhs == "rtdev" {
                rtdev = Some(PathBuf::from(rhs))
            }
        } else {
            opts.push(match o.as_str() {
                "auto_unmount" => MountOption::AutoUnmount,
                "allow_other" => MountOption::AllowOther,
                "allow_root" => MountOption::AllowRoot,
                "default_permissions" => MountOption::DefaultPermissions,
                "dev" => MountOption::Dev,
                "nodev" => MountOption::NoDev,
                "suid" => MountOption::Suid,
                "nosuid" => MountOption::NoSuid,
                "exec" => MountOption::Exec,
                "noexec" => MountOption::NoExec,
                "atime" => MountOption::Atime,
                "noatime" => MountOption::NoAtime,
                "dirsync" => MountOption::DirSync,
                "sync" => MountOption::Sync,
                "async" => MountOption::Async,
                custom => MountOption::CUSTOM(custom.to_string()),
            });
        }
    }

    // Open the image before going into the background, so that a refusal -- an
    // unwritable image, an image whose features this build cannot maintain --
    // is reported to whoever ran the command.
    let vol = match Volume::new(&app.device, rtdev.as_ref(), app.experimental_rw) {
        Ok(vol) => vol,
        Err(e) => {
            eprintln!("xfs-fuse: {e}");
            std::process::exit(1);
        }
    };

    if !app.foreground {
        daemon(false, false).unwrap();
    }
    mount2(vol, app.mountpoint, &opts[..]).unwrap();
}
