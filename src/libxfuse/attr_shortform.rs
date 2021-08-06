use std::io::{BufRead, Seek};

use super::{
    attr::{get_namespace_from_flags, get_namespace_size_from_flags, Attr},
    sb::Sb,
};

use byteorder::{BigEndian, ReadBytesExt};

#[derive(Debug, Clone)]
pub struct AttrSfHdr {
    pub totsize: u16,
    pub count: u8,
    pub padding: u8,
}

impl AttrSfHdr {
    pub fn from<R: BufRead>(buf_reader: &mut R) -> AttrSfHdr {
        let totsize = buf_reader.read_u16::<BigEndian>().unwrap();
        let count = buf_reader.read_u8().unwrap();
        let padding = buf_reader.read_u8().unwrap();

        AttrSfHdr {
            totsize,
            count,
            padding,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AttrSfEntry {
    pub namelen: u8,
    pub valuelen: u8,
    pub flags: u8,
    pub nameval: Vec<u8>,
}

impl AttrSfEntry {
    pub fn from<R: BufRead>(buf_reader: &mut R) -> AttrSfEntry {
        let namelen = buf_reader.read_u8().unwrap();
        let valuelen = buf_reader.read_u8().unwrap();
        let flags = buf_reader.read_u8().unwrap();

        let mut nameval = Vec::<u8>::new();
        for _i in 0..(namelen + valuelen) {
            nameval.push(buf_reader.read_u8().unwrap());
        }

        AttrSfEntry {
            namelen,
            valuelen,
            flags,
            nameval,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AttrShortform {
    pub hdr: AttrSfHdr,
    pub list: Vec<AttrSfEntry>,

    pub total_size: u32,
}

impl AttrShortform {
    pub fn from<R: BufRead>(buf_reader: &mut R) -> AttrShortform {
        let hdr = AttrSfHdr::from(buf_reader.by_ref());

        let mut list = Vec::<AttrSfEntry>::new();
        let mut total_size: u32 = 0;
        for _i in 0..hdr.count {
            let entry = AttrSfEntry::from(buf_reader.by_ref());

            total_size += get_namespace_size_from_flags(entry.flags) + u32::from(entry.namelen) + 1;
            list.push(entry);
        }

        AttrShortform {
            hdr,
            list,
            total_size,
        }
    }
}

impl<R: BufRead + Seek> Attr<R> for AttrShortform {
    fn get_total_size(&mut self, _buf_reader: &mut R, _super_block: &Sb) -> u32 {
        self.total_size
    }

    fn get_size(&self, _buf_reader: &mut R, _super_block: &Sb, name: &str) -> u32 {
        println!("Attr: {:?}", self);
        println!("Attr: {:?}", name.as_bytes());

        for entry in &self.list {
            let entry_name = entry.nameval[0..(entry.namelen as usize)].to_vec();

            if name.as_bytes().to_vec() == entry_name {
                return entry.valuelen.into();
            }
        }

        panic!("Couldn't find entry!");
    }

    fn list(&mut self, buf_reader: &mut R, super_block: &Sb) -> Vec<u8> {
        let mut list: Vec<u8> =
            Vec::with_capacity(self.get_total_size(buf_reader.by_ref(), &super_block) as usize);

        for entry in self.list.iter() {
            list.extend_from_slice(&get_namespace_from_flags(entry.flags).as_bytes().to_vec());
            let namelen = entry.namelen as usize;
            list.extend_from_slice(&entry.nameval[0..namelen].to_vec());
            list.push(0)
        }

        list
    }

    fn get(&self, _buf_reader: &mut R, _super_block: &Sb, name: &str) -> Vec<u8> {
        println!("Attr: {:?}", self);
        println!("Attr: {:?}", name.as_bytes());

        for entry in &self.list {
            let entry_name = entry.nameval[0..(entry.namelen as usize)].to_vec();

            if name.as_bytes().to_vec() == entry_name {
                let namelen = entry.namelen as usize;

                return entry.nameval[namelen..].to_vec();
            }
        }

        panic!("Couldn't find entry!");
    }
}
