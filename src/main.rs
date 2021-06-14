#![allow(dead_code)]
mod libxfuse;

use libxfuse::volume::Volume;

use fuse::mount;

fn main() {
    let device = match std::env::args().nth(1) {
        Some(path) => path,
        None => {
            eprintln!(
                "Usage: {} <DEVICE> <MOUNTPOINT>.  Missing mountpoint argument",
                std::env::args().nth(0).unwrap()
            );
            return;
        }
    };

    let mountpoint = match std::env::args().nth(2) {
        Some(path) => path,
        None => {
            eprintln!(
                "Usage: {} <DEVICE> <MOUNTPOINT>.  Missing mountpoint argument",
                std::env::args().nth(0).unwrap()
            );
            return;
        }
    };

    let vol = Volume::from(&device);

    mount(vol, &mountpoint, &[]).unwrap();
}
