#![allow(dead_code)]
mod libxfuse;

use libxfuse::volume::Volume;

use clap::crate_version;
use fuse::mount;
use std::{
    ffi::{OsStr, OsString},
    io::{BufReader, Read},
};

use crate::libxfuse::{
    dinode::{DiU, Dinode},
    dir2_block::Dir2Block,
};

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

    // let dinode = Dinode::from(BufReader::new(&vol.device).by_ref(), &vol.sb, 2097792);
    // if let DiU::DiBmx(list) = dinode.di_u {
    //     let blk_dir = Dir2Block::from(
    //         BufReader::new(&vol.device).by_ref(),
    //         &vol.sb,
    //         list[0].br_startblock,
    //     );
    //     println!("{:#?}", blk_dir);
    // } else {
    //     panic!("Nont yet supported");
    // }

    mount(vol, &mountpoint, &opt_refs[..]).unwrap();
}
