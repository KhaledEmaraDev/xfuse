use std::io::{BufRead, Seek};

use super::sb::Sb;

pub trait File<R: BufRead + Seek> {
    fn read(&mut self, buf_reader: &mut R, super_block: &Sb, offset: i64, size: u32) -> Vec<u8>;
}
