use fuse::FileType;

use super::dir3::{XFS_DIR3_FT_DIR, XFS_DIR3_FT_REG_FILE, XFS_DIR3_FT_SYMLINK};

use libc::{c_int, mode_t, ENOENT, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};

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
                println!("Unknown file type.");
                Err(ENOENT)
            }
        },
        FileKind::Mode(file_mode) => match (file_mode as mode_t) & S_IFMT {
            S_IFREG => Ok(FileType::RegularFile),
            S_IFDIR => Ok(FileType::Directory),
            S_IFLNK => Ok(FileType::Symlink),
            _ => {
                println!("Unknown file type.");
                Err(ENOENT)
            }
        },
    }
}
