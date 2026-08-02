use core::cell::RefCell;

use embassy_rp::flash::{Blocking, Flash};
use embassy_rp::peripherals::FLASH;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use heapless::{String, Vec};
use littlefs2::consts::{U8, U256};
use littlefs2::driver::Storage;
use littlefs2::fs::{Filesystem, OpenOptions};
use littlefs2::io::{Error as FsError, SeekFrom};
use littlefs2::path::{Path, PathBuf};

const STORAGE_OFFSET: usize = 6 * 1024 * 1024;
pub(crate) const STORAGE_SIZE: usize = 2 * 1024 * 1024;
pub(crate) const MAX_FILES: usize = 32;
pub(crate) const MAX_UPLOAD_CHUNK: usize = 512;

type BoardFlash = Flash<'static, FLASH, Blocking, { crate::board::FLASH_SIZE }>;

static FLASH_DEVICE: Mutex<ThreadModeRawMutex, RefCell<Option<BoardFlash>>> =
    Mutex::new(RefCell::new(None));

pub(crate) async fn init(flash: BoardFlash) {
    FLASH_DEVICE.lock(|slot| slot.replace(Some(flash)));
}

struct AppStorage;

impl AppStorage {
    fn with_flash<R>(f: impl FnOnce(&mut BoardFlash) -> Result<R, FsError>) -> Result<R, FsError> {
        FLASH_DEVICE.lock(|slot| {
            let mut slot = slot.borrow_mut();
            let flash = slot.as_mut().ok_or(FsError::IO)?;
            f(flash)
        })
    }
}

impl Storage for AppStorage {
    const READ_SIZE: usize = 1;
    const WRITE_SIZE: usize = 256;
    const BLOCK_SIZE: usize = 4096;
    const BLOCK_COUNT: usize = STORAGE_SIZE / Self::BLOCK_SIZE;
    const BLOCK_CYCLES: isize = 500;
    type CACHE_SIZE = U256;
    type LOOKAHEAD_SIZE = U8;

    fn read(&mut self, off: usize, buf: &mut [u8]) -> littlefs2::io::Result<usize> {
        Self::with_flash(|flash| {
            flash
                .blocking_read((STORAGE_OFFSET + off) as u32, buf)
                .map_err(|_| FsError::IO)?;
            Ok(buf.len())
        })
    }

    fn write(&mut self, off: usize, data: &[u8]) -> littlefs2::io::Result<usize> {
        Self::with_flash(|flash| {
            flash
                .blocking_write((STORAGE_OFFSET + off) as u32, data)
                .map_err(|_| FsError::IO)?;
            Ok(data.len())
        })
    }

    fn erase(&mut self, off: usize, len: usize) -> littlefs2::io::Result<usize> {
        Self::with_flash(|flash| {
            let from = (STORAGE_OFFSET + off) as u32;
            flash
                .blocking_erase(from, from + len as u32)
                .map_err(|_| FsError::IO)?;
            Ok(len)
        })
    }
}

#[derive(Clone)]
pub(crate) struct FileInfo {
    pub(crate) name: String<64>,
    pub(crate) size: usize,
}

pub(crate) struct Status {
    pub(crate) formatted: bool,
    pub(crate) total: usize,
    pub(crate) available: usize,
    pub(crate) files: Vec<FileInfo, MAX_FILES>,
}

fn app_path(name: &str) -> Result<PathBuf, FsError> {
    if name.is_empty()
        || name.len() > 63
        || !name.ends_with(".html")
        || name.starts_with('.')
        || name
            .bytes()
            .any(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')))
    {
        return Err(FsError::INVALID);
    }

    let mut path = String::<72>::new();
    path.push_str("/apps/").map_err(|_| FsError::IO)?;
    path.push_str(name).map_err(|_| FsError::IO)?;
    PathBuf::try_from(path.as_str()).map_err(|_| FsError::INVALID)
}

pub(crate) fn status() -> Result<Status, FsError> {
    let mut storage = AppStorage;
    if !Filesystem::is_mountable(&mut storage) {
        return Ok(Status {
            formatted: false,
            total: STORAGE_SIZE,
            available: 0,
            files: Vec::new(),
        });
    }

    Filesystem::mount_and_then(&mut storage, |fs| {
        let mut files = Vec::new();
        let apps = Path::from_bytes_with_nul(b"/apps\0").map_err(|_| FsError::IO)?;
        if fs.exists(apps) {
            fs.read_dir_and_then(apps, |entries| {
                for entry in entries.skip(2) {
                    let entry = entry?;
                    if !entry.file_type().is_file() || files.is_full() {
                        continue;
                    }
                    let name = entry.file_name().as_str();
                    let Ok(name) = String::try_from(name) else {
                        continue;
                    };
                    files
                        .push(FileInfo {
                            name,
                            size: entry.metadata().len(),
                        })
                        .map_err(|_| FsError::NO_SPACE)?;
                }
                Ok(())
            })?;
        }
        Ok(Status {
            formatted: true,
            total: fs.total_space(),
            available: fs.available_space()?,
            files,
        })
    })
}

pub(crate) fn format() -> Result<(), FsError> {
    let mut storage = AppStorage;
    Filesystem::format(&mut storage)?;
    Filesystem::mount_and_then(&mut storage, |fs| {
        fs.create_dir(Path::from_bytes_with_nul(b"/apps\0").map_err(|_| FsError::IO)?)
    })
}

pub(crate) fn write_chunk(name: &str, offset: usize, data: &[u8]) -> Result<(), FsError> {
    if data.len() > MAX_UPLOAD_CHUNK {
        return Err(FsError::INVALID);
    }
    let path = app_path(name)?;
    let mut storage = AppStorage;
    Filesystem::mount_and_then(&mut storage, |fs| {
        let apps = Path::from_bytes_with_nul(b"/apps\0").map_err(|_| FsError::IO)?;
        if !fs.exists(apps) {
            fs.create_dir(apps)?;
        }
        let mut options = OpenOptions::new();
        options.write(true).create(true);
        if offset == 0 {
            options.truncate(true);
        }
        options.open_and_then(fs, &path, |file| {
            file.seek(SeekFrom::Start(
                offset.try_into().map_err(|_| FsError::FILE_TOO_BIG)?,
            ))?;
            let mut written = 0;
            while written < data.len() {
                written += file.write(&data[written..])?;
            }
            file.sync()
        })
    })
}

pub(crate) fn remove(name: &str) -> Result<(), FsError> {
    let path = app_path(name)?;
    let mut storage = AppStorage;
    Filesystem::mount_and_then(&mut storage, |fs| fs.remove(&path))
}

pub(crate) fn read_chunk<const N: usize>(
    name: &str,
    offset: usize,
) -> Result<(Vec<u8, N>, usize), FsError> {
    let path = app_path(name)?;
    let mut storage = AppStorage;
    Filesystem::mount_and_then(&mut storage, |fs| {
        fs.read_chunk::<N>(
            &path,
            littlefs2::io::OpenSeekFrom::Start(
                offset.try_into().map_err(|_| FsError::FILE_TOO_BIG)?,
            ),
        )
    })
}
