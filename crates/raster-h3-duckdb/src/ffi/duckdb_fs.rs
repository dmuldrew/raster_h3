//! RAII wrappers over the DuckDB stable-C file system API plus a
//! [`RemoteTransport`] implementation that routes raster byte-range reads
//! through DuckDB's file layer (local files, httpfs/HTTP, S3, Azure, GCS,
//! `CREATE SECRET` credentials, and reader options).

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use raster_h3_core::error::RasterH3Error;
use raster_h3_core::error::Result;
use raster_h3_core::raster::http_range::{ProbeResult, RemoteTransport};

use super::duckdb_c::*;
use super::duckdb_c_v156::*;

fn fs_err(message: String) -> RasterH3Error {
    RasterH3Error::InvalidParameter(message)
}

/// Owned `duckdb_error_data` that formats its message on request.
struct ErrorData(duckdb_error_data);

impl ErrorData {
    fn message(&self) -> String {
        if self.0.is_null() {
            return "unknown DuckDB error".to_string();
        }
        // SAFETY: DuckDB owns the message of an error_data it handed us; it is
        // NUL-terminated for the error_data lifetime.
        let msg = unsafe { duckdb_error_data_message(self.0) };
        if msg.is_null() {
            return "unknown DuckDB error".to_string();
        }
        // SAFETY: NUL-terminated C string owned by the error data.
        unsafe { std::ffi::CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for ErrorData {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: owned error data, destroyed once.
            unsafe { duckdb_destroy_error_data(&mut self.0) };
        }
    }
}

/// Owned DuckDB file-system instance obtained from a client context.
/// The handle is a thin wrapper over the DuckDB `FileSystem` of the database
/// instance and is valid for the lifetime of that instance.
pub struct DuckDbFileSystem {
    handle: duckdb_file_system,
}

// The wrapped FileSystem is DuckDB-internal and documented thread-safe for
// concurrent OpenFile/ReadFile callers.
// SAFETY: the wrapped DuckDB handle is an opaque pointer; access goes through
// the (internally synchronized) DuckDB C API.
unsafe impl Send for DuckDbFileSystem {}
// SAFETY: same reasoning as `Send`.
unsafe impl Sync for DuckDbFileSystem {}

impl DuckDbFileSystem {
    /// Take ownership of a raw `duckdb_file_system`.
    ///
    /// # Safety
    /// `handle` must be a live, un-owned `duckdb_file_system`; it is destroyed
    /// on drop.
    pub unsafe fn from_raw(handle: duckdb_file_system) -> Self {
        Self { handle }
    }

    fn last_error(&self) -> String {
        // SAFETY: handle is live; error_data is owned by us afterwards.
        let ed = unsafe { duckdb_file_system_error_data(self.handle) };
        ErrorData(ed).message()
    }

    /// Open `path` for reading, with no extra flags.
    pub fn open_read(&self, path: &str) -> Result<DuckDbFileHandle> {
        self.open(path, false)
    }

    /// Open `path` for writing (created/truncated).
    pub fn open_write(&self, path: &str) -> Result<DuckDbFileHandle> {
        self.open(path, true)
    }

    fn open(&self, path: &str, for_write: bool) -> Result<DuckDbFileHandle> {
        let c_path = std::ffi::CString::new(path)
            .map_err(|e| fs_err(format!("invalid file path {path:?}: {e}")))?;
        // SAFETY: allocating a fresh options object we own until destroyed.
        let options = unsafe { duckdb_create_file_open_options() };
        if options.is_null() {
            return Err(fs_err("duckdb_create_file_open_options failed".to_string()));
        }
        let mut handle: duckdb_file_handle = std::ptr::null_mut();
        // SAFETY: all handles valid; out_handle written on success.
        let state = unsafe {
            if for_write {
                duckdb_file_open_options_set_flag(options, DuckDBFileFlag::Write, true);
                duckdb_file_open_options_set_flag(options, DuckDBFileFlag::Create, true);
            } else {
                duckdb_file_open_options_set_flag(options, DuckDBFileFlag::Read, true);
            }
            duckdb_file_system_open(self.handle, c_path.as_ptr(), options, &mut handle)
        };
        // SAFETY: options object owned by us.
        let mut options = options;
        // SAFETY: `options` is a valid options handle created above.
        unsafe { duckdb_destroy_file_open_options(&mut options) };
        if state != DuckDBState::Success || handle.is_null() {
            return Err(fs_err(format!(
                "DuckDB could not open {path}: {}",
                self.last_error()
            )));
        }
        Ok(DuckDbFileHandle { handle })
    }
}

impl Drop for DuckDbFileSystem {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: owned file-system handle destroyed once.
            unsafe { duckdb_destroy_file_system(&mut self.handle) };
        }
    }
}

// DuckDB flag setting for open options is per-flag, not bitmask-combinable.
// Correcting the open() helper above.

/// Owned DuckDB file handle. Not internally synchronized: wrap in a mutex or
/// pool when accessed from multiple threads (as [`DuckDbFileTransport`] does).
pub struct DuckDbFileHandle {
    handle: duckdb_file_handle,
}

// SAFETY: the wrapped handle is only used through the DuckDB C API.
unsafe impl Send for DuckDbFileHandle {}

impl DuckDbFileHandle {
    fn last_error(&self) -> String {
        // SAFETY: handle is live.
        let ed = unsafe { duckdb_file_handle_error_data(self.handle) };
        ErrorData(ed).message()
    }

    /// Seek to an absolute byte offset.
    pub fn seek(&self, offset: u64) -> Result<()> {
        // SAFETY: handle is live.
        let state = unsafe { duckdb_file_handle_seek(self.handle, offset as i64) };
        if state != DuckDBState::Success {
            return Err(fs_err(format!(
                "DuckDB seek to offset {offset} failed: {}",
                self.last_error()
            )));
        }
        Ok(())
    }

    /// Read up to `buf.len()` bytes at the current position.
    /// Returns the number of bytes actually read.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        // SAFETY: handle live, buf writable for its length.
        let n = unsafe {
            duckdb_file_handle_read(
                self.handle,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as i64,
            )
        };
        if n < 0 {
            return Err(fs_err(format!("DuckDB read failed: {}", self.last_error())));
        }
        Ok(n as usize)
    }

    /// Write all of `buf` at the current position.
    pub fn write_all(&self, mut buf: &[u8]) -> Result<()> {
        while !buf.is_empty() {
            // SAFETY: handle live, buf readable for its length.
            let n = unsafe {
                duckdb_file_handle_write(
                    self.handle,
                    buf.as_ptr() as *const c_void,
                    buf.len() as i64,
                )
            };
            if n <= 0 {
                return Err(fs_err(format!(
                    "DuckDB write failed: {}",
                    self.last_error()
                )));
            }
            buf = &buf[n as usize..];
        }
        Ok(())
    }

    /// Current file position.
    pub fn tell(&self) -> Result<u64> {
        // SAFETY: handle is live.
        let pos = unsafe { duckdb_file_handle_tell(self.handle) };
        if pos < 0 {
            return Err(fs_err(format!("DuckDB tell failed: {}", self.last_error())));
        }
        Ok(pos as u64)
    }

    /// Total file size in bytes.
    pub fn size(&self) -> Result<u64> {
        // SAFETY: handle is live.
        let size = unsafe { duckdb_file_handle_size(self.handle) };
        if size < 0 {
            return Err(fs_err(format!(
                "DuckDB file size failed: {}",
                self.last_error()
            )));
        }
        Ok(size as u64)
    }

    /// Flush buffers for a write handle.
    pub fn sync(&self) -> Result<()> {
        // SAFETY: handle is live.
        let state = unsafe { duckdb_file_handle_sync(self.handle) };
        if state != DuckDBState::Success {
            return Err(fs_err(format!("DuckDB sync failed: {}", self.last_error())));
        }
        Ok(())
    }
}

impl Drop for DuckDbFileHandle {
    fn drop(&mut self) {
        // SAFETY: owned handle; close errors are unrecoverable here so they
        // are ignored, then the wrapper is destroyed.
        unsafe {
            let _ = duckdb_file_handle_close(self.handle);
            duckdb_destroy_file_handle(&mut self.handle);
        }
    }
}

/// [`RemoteTransport`] backed by a pool of DuckDB file handles.
///
/// Each `fetch_range` checks out a handle, performs `seek + read`, and returns
/// it to the pool, so up to `fetch_workers` range requests progress in
/// parallel instead of serializing behind one file position.
pub struct DuckDbFileTransport {
    fs: Arc<DuckDbFileSystem>,
    path: String,
    total_size: u64,
    pool: Mutex<Vec<DuckDbFileHandle>>,
}

impl DuckDbFileTransport {
    /// Open `path` through the DuckDB file system and determine its size.
    pub fn open(fs: Arc<DuckDbFileSystem>, path: &str) -> Result<Self> {
        let first = fs.open_read(path)?;
        let total_size = first.size()?;
        Ok(Self {
            fs,
            path: path.to_string(),
            total_size,
            pool: Mutex::new(vec![first]),
        })
    }

    fn read_exact_at(&self, start: u64, len: usize) -> Result<Vec<u8>> {
        let handle = {
            // SAFETY of semantics: pool poisoning only occurs on panic; a
            // poisoned pool is unrecoverable mid-scan and propagating it as an
            // error is correct.
            match self.pool.lock() {
                Ok(mut pool) => match pool.pop() {
                    Some(h) => h,
                    None => self.fs.open_read(&self.path)?,
                },
                Err(e) => return Err(fs_err(format!("DuckDB handle pool poisoned: {e}"))),
            }
        };
        let result = (|| {
            handle.seek(start)?;
            let mut buf = vec![0u8; len];
            let mut filled = 0usize;
            while filled < len {
                let n = handle.read(&mut buf[filled..])?;
                if n == 0 {
                    return Err(fs_err(format!(
                        "short read on {}: got {} of {} bytes at offset {}",
                        self.path, filled, len, start
                    )));
                }
                filled += n;
            }
            Ok(buf)
        })();
        if let Ok(mut pool) = self.pool.lock() {
            pool.push(handle);
        }
        result
    }
}

impl RemoteTransport for DuckDbFileTransport {
    fn fetch_range(&self, start: u64, end: u64, expected_total: Option<u64>) -> Result<Vec<u8>> {
        let total = expected_total.unwrap_or(self.total_size);
        if start > end || end >= total {
            return Err(fs_err(format!(
                "requested byte range {start}-{end} lies outside {} bytes of {}",
                total, self.path
            )));
        }
        self.read_exact_at(start, (end - start + 1) as usize)
    }

    fn probe(&self, initial_fetch_len: usize) -> Result<ProbeResult> {
        if self.total_size == 0 {
            return Ok(ProbeResult::Complete {
                data: Vec::new(),
                total_size: 0,
            });
        }
        let len = (initial_fetch_len as u64).min(self.total_size);
        let data = self.read_exact_at(0, len as usize)?;
        if len == self.total_size {
            Ok(ProbeResult::Complete {
                data,
                total_size: self.total_size,
            })
        } else {
            Ok(ProbeResult::Partial {
                data,
                total_size: self.total_size,
            })
        }
    }
}

/// `io::Write`/`io::Seek` adapter over a write [`DuckDbFileHandle`], so
/// `COPY ... TO` outputs can be written through the DuckDB file layer to
/// `file://`, `s3://`, and other DuckDB-scheme destinations.
pub struct DuckDbFileWriter {
    handle: DuckDbFileHandle,
}

impl DuckDbFileWriter {
    /// Take ownership of a handle already opened for writing.
    pub fn new(handle: DuckDbFileHandle) -> Self {
        Self { handle }
    }

    /// Flush buffered data to the underlying file system.
    pub fn sync(&self) -> Result<()> {
        self.handle.sync()
    }
}

impl std::io::Write for DuckDbFileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.handle
            .write_all(buf)
            .map(|()| buf.len())
            .map_err(std::io::Error::other)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.handle.sync().map_err(std::io::Error::other)
    }
}

impl std::io::Seek for DuckDbFileWriter {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let target = match pos {
            std::io::SeekFrom::Start(o) => o as i64,
            std::io::SeekFrom::Current(d) => {
                self.handle.tell().map_err(std::io::Error::other)? as i64 + d
            }
            std::io::SeekFrom::End(d) => {
                self.handle.size().map_err(std::io::Error::other)? as i64 + d
            }
        };
        if target < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before start of file",
            ));
        }
        self.handle
            .seek(target as u64)
            .map_err(std::io::Error::other)?;
        Ok(target as u64)
    }
}
