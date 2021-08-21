#![allow(dead_code)]
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
mod libxfuse;

use libxfuse::volume::Volume;

use clap::crate_version;
use fuse::mount;
use std::ffi::{OsStr, OsString};

fn main() {
    let app = clap::App::new("xfs-fuse")
        .version(crate_version!())
        .arg(
            clap::Arg::with_name("option")
                .help("Mount options")
                .short("o")
                .takes_value(true)
                .multiple(true)
                .require_delimiter(true),
        )
        .arg(clap::Arg::with_name("device").required(true))
        .arg(clap::Arg::with_name("mountpoint").required(true));
    let matches = app.get_matches();

    let mut opts = Vec::new();
    if let Some(it) = matches.values_of("option") {
        for o in it {
            // mount_fusefs expects to have a separate "-o" per option
            opts.push(OsString::from("-o"));
            opts.push(OsString::from(o));
        }
    };
    // We need a separate vec of references :(
    // https://github.com/zargony/rust-fuse/issues/117
    let opt_refs = opts.iter().map(|o| o.as_ref()).collect::<Vec<&OsStr>>();

    let device = matches.value_of("device").unwrap().to_string();
    let mountpoint = matches.value_of("mountpoint").unwrap().to_string();

    let vol = Volume::from(&device);

    mount(vol, &mountpoint, &opt_refs[..]).unwrap();
}
