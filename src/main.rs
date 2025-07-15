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
}

fn main() {
    tracing_subscriber::fmt()
        .pretty()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let app = App::parse();

    let mut opts = vec![
        MountOption::FSName("fusefs".to_string()),
        MountOption::Subtype("xfs".to_string()),
        MountOption::RO,
    ];
    // geteuid is always safe
    if unsafe { libc::geteuid() } == 0 {
        opts.push(MountOption::AllowOther);
        opts.push(MountOption::DefaultPermissions);
    }
    for o in app.options.iter() {
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

    let vol = Volume::from(&app.device);

    if !app.foreground {
        daemon(false, false).unwrap();
    }
    mount2(vol, app.mountpoint, &opts[..]).unwrap();
}
