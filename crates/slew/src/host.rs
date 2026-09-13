//! Host capability seam.
//!
//! The interpreter core has **no ambient authority**: it never opens a file,
//! reads the environment, or spawns a process on its own. Everything in `io`
//! and `os` that would need the host is routed through a [`Host`] the embedder
//! installs explicitly with [`crate::Lua::set_host`]. Without one, `io` and
//! `os` do not exist at all (the globals are unset and `require` fails), so the
//! core cannot acquire authority by accident.
//!
//! [`StdHost`] is a convenient std-backed implementation (filesystem confined
//! to a root, captured std streams, UTC calendar) for tests and simple
//! embedders; production embedders can implement [`Host`] themselves.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::value::TableId;

/// A capability failure: a human message plus a C-style `errno` (`0` when not
/// applicable). `io.*` turns these into `nil, message, errno` returns.
#[derive(Debug, Clone)]
pub struct HostError {
    pub message: String,
    pub errno: i32,
}

impl HostError {
    pub fn new(message: impl Into<String>) -> Self {
        HostError {
            message: message.into(),
            errno: 0,
        }
    }

    pub fn with_errno(message: impl Into<String>, errno: i32) -> Self {
        HostError {
            message: message.into(),
            errno,
        }
    }
}

/// Which base a file seek is relative to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekWhence {
    Set,
    Cur,
    End,
}

/// Broken-down calendar time, as `os.date`/`os.time` exchange it with the
/// host. `wday` is 0 = Sunday .. 6 = Saturday; `yday` is 1..=366. Field order
/// mirrors the `struct tm` fields consultable from a Lua date table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateParts {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub min: i64,
    pub sec: i64,
    pub wday: i64,
    pub yday: i64,
    pub isdst: bool,
}

/// The host object a userdata wraps. File handles are host-owned ids; the
/// standard streams are handled by dedicated host methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostObject {
    Stdin,
    Stdout,
    Stderr,
    File(u64),
}

/// A userdata object: an optional metatable plus a host payload. The
/// interpreter owns the arena; the host owns the resources. When a file
/// userdata is swept (or explicitly closed) the interpreter tells the host to
/// release the handle, so host resources are not leaked.
pub struct Userdata {
    pub metatable: Option<TableId>,
    pub object: HostObject,
    pub closed: bool,
    pub finalized: bool,
    /// Bytes read ahead of the logical file position (for line/number reads).
    pub read_buf: Vec<u8>,
    pub read_pos: usize,
}

impl Default for Userdata {
    fn default() -> Self {
        Userdata {
            metatable: None,
            object: HostObject::Stdin,
            closed: true,
            finalized: false,
            read_buf: Vec::new(),
            read_pos: 0,
        }
    }
}

/// The capability surface `io`/`os` need. Methods are intentionally
/// coarse-grained and byte-oriented: read/write/seek raw bytes, and let the
/// interpreter buffer. Implementations choose their own sandboxing policy.
pub trait Host {
    // ---- standard streams ----
    /// Writes `bytes` to the standard output stream.
    ///
    /// # Errors
    /// Returns a [`HostError`] if the write fails.
    fn stdout_write(&mut self, bytes: &[u8]) -> Result<(), HostError>;
    /// Writes `bytes` to the standard error stream.
    ///
    /// # Errors
    /// Returns a [`HostError`] if the write fails.
    fn stderr_write(&mut self, bytes: &[u8]) -> Result<(), HostError>;
    /// Reads into `buf`, returning the number of bytes read.
    ///
    /// # Errors
    /// Returns a [`HostError`] if the read fails.
    fn stdin_read(&mut self, buf: &mut [u8]) -> Result<usize, HostError>;

    // ---- files ----
    /// Opens `path` according to `mode` (`r`/`w`/`a`, optional `+`, optional
    /// `b`; validated by the caller) and returns a host handle.
    ///
    /// # Errors
    /// Returns a [`HostError`] if the file cannot be opened.
    fn open(&mut self, path: &str, mode: &str) -> Result<u64, HostError>;
    /// Closes the file handle `handle`.
    ///
    /// # Errors
    /// Returns a [`HostError`] if `handle` is not an open file or the close fails.
    fn close(&mut self, handle: u64) -> Result<(), HostError>;
    /// Reads up to `buf.len()` bytes from `handle`, returning the count read.
    ///
    /// # Errors
    /// Returns a [`HostError`] if `handle` is not an open file or the read fails.
    fn read(&mut self, handle: u64, buf: &mut [u8]) -> Result<usize, HostError>;
    /// Writes `bytes` to `handle`, returning the number of bytes written.
    ///
    /// # Errors
    /// Returns a [`HostError`] if `handle` is not an open file or the write fails.
    fn write(&mut self, handle: u64, bytes: &[u8]) -> Result<usize, HostError>;
    /// Seeks `handle` relative to `whence` by `offset`, returning the new position.
    ///
    /// # Errors
    /// Returns a [`HostError`] if `handle` is not an open file or the seek fails.
    fn seek(&mut self, handle: u64, whence: SeekWhence, offset: i64) -> Result<u64, HostError>;
    /// Flushes any buffered data for `handle`.
    ///
    /// # Errors
    /// Returns a [`HostError`] if `handle` is not an open file or the flush fails.
    fn flush(&mut self, handle: u64) -> Result<(), HostError>;
    /// Sets the buffering mode for `handle`.
    ///
    /// # Errors
    /// Returns a [`HostError`] if the buffering configuration is invalid.
    fn setvbuf(&mut self, _handle: u64, _mode: &[u8], _size: usize) -> Result<(), HostError> {
        Ok(())
    }

    // ---- filesystem (os.*) ----
    /// Removes the file at `path`.
    ///
    /// # Errors
    /// Returns a [`HostError`] if the file cannot be removed.
    fn remove(&mut self, path: &str) -> Result<(), HostError>;
    /// Renames the file `from` to `to`.
    ///
    /// # Errors
    /// Returns a [`HostError`] if the rename fails.
    fn rename(&mut self, from: &str, to: &str) -> Result<(), HostError>;
    /// Returns a fresh temporary file name.
    ///
    /// # Errors
    /// Returns a [`HostError`] if a name cannot be generated.
    fn tmpname(&mut self) -> Result<String, HostError>;

    // ---- environment / time / locale ----
    fn getenv(&mut self, name: &str) -> Option<Vec<u8>>;
    fn clock(&mut self) -> f64;
    /// Current UTC time as seconds since the Unix epoch.
    fn time(&mut self) -> i64;
    /// Broken-down time for `t` (`utc=false` may apply the host timezone).
    fn time_parts(&mut self, t: i64, utc: bool) -> DateParts;
    /// Inverse of [`Host::time_parts`] for local time (`os.time(table)`).
    fn make_time(&mut self, parts: DateParts) -> i64;

    /// `os.setlocale`. `None` locale queries the current one; an unsupported
    /// locale request returns `None`.
    fn setlocale(&mut self, locale: Option<&str>, category: Option<&str>) -> Option<String>;

    /// `os.exit`: the core never terminates the host process; it records the
    /// request and raises a controlled error. A host may override this to
    /// observe the request.
    fn request_exit(&mut self, _code: i64, _close: bool) {}
}

/// A std-backed [`Host`] for tests and simple embedders. Filesystem access is
/// confined to `root`: paths are resolved under it and anything escaping
/// (absolute outside root, `..`, symlinks) is refused. std streams are
/// captured in-memory so tests can assert on them.
pub struct StdHost {
    root: PathBuf,
    files: HashMap<u64, std::fs::File>,
    next_handle: u64,
    stdout: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    stderr: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    stdin: Vec<u8>,
    stdin_pos: usize,
    start: std::time::Instant,
}

impl StdHost {
    /// A host rooted at `root` with empty stdin and captured stdout/stderr.
    pub fn new(root: impl AsRef<Path>) -> Self {
        StdHost {
            root: root.as_ref().to_path_buf(),
            files: HashMap::new(),
            next_handle: 1,
            stdout: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            stderr: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            stdin: Vec::new(),
            stdin_pos: 0,
            start: std::time::Instant::now(),
        }
    }

    /// Preloads the bytes that `io.stdin`/`io.read` will see.
    #[must_use]
    pub fn with_stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = bytes.into();
        self
    }

    /// A shared handle to the captured stdout bytes. Cloning the handle before
    /// the host is moved into the interpreter lets tests inspect the sink.
    #[must_use]
    pub fn stdout_sink(&self) -> std::rc::Rc<std::cell::RefCell<Vec<u8>>> {
        self.stdout.clone()
    }

    /// A shared handle to the captured stderr bytes.
    #[must_use]
    pub fn stderr_sink(&self) -> std::rc::Rc<std::cell::RefCell<Vec<u8>>> {
        self.stderr.clone()
    }

    fn confined(&self, path: &str, for_write: bool) -> Result<PathBuf, HostError> {
        let root = self
            .root
            .canonicalize()
            .map_err(|e| HostError::new(format!("cannot access host root: {e}")))?;
        let p = Path::new(path);
        let candidate = if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        };
        if for_write {
            let parent = candidate.parent().unwrap_or(&root);
            let real_parent = parent
                .canonicalize()
                .map_err(|e| HostError::new(format!("cannot access {}: {e}", parent.display())))?;
            if !real_parent.starts_with(&root) {
                return Err(HostError::new(format!(
                    "access denied: '{path}' is outside the host root"
                )));
            }
            let name = candidate
                .file_name()
                .ok_or_else(|| HostError::new(format!("access denied: invalid path '{path}'")))?;
            Ok(real_parent.join(name))
        } else {
            let real = candidate
                .canonicalize()
                .map_err(|e| HostError::with_errno(e.to_string(), 2))?;
            if !real.starts_with(&root) {
                return Err(HostError::new(format!(
                    "access denied: '{path}' is outside the host root"
                )));
            }
            Ok(real)
        }
    }
}

fn io_err(e: &std::io::Error) -> HostError {
    let errno = e.raw_os_error().unwrap_or(0);
    HostError::with_errno(e.to_string(), errno)
}

impl Host for StdHost {
    fn stdout_write(&mut self, bytes: &[u8]) -> Result<(), HostError> {
        self.stdout.borrow_mut().extend_from_slice(bytes);
        Ok(())
    }

    fn stderr_write(&mut self, bytes: &[u8]) -> Result<(), HostError> {
        self.stderr.borrow_mut().extend_from_slice(bytes);
        Ok(())
    }

    fn stdin_read(&mut self, buf: &mut [u8]) -> Result<usize, HostError> {
        let n = (self.stdin.len() - self.stdin_pos).min(buf.len());
        buf[..n].copy_from_slice(&self.stdin[self.stdin_pos..self.stdin_pos + n]);
        self.stdin_pos += n;
        Ok(n)
    }

    fn open(&mut self, path: &str, mode: &str) -> Result<u64, HostError> {
        use std::fs::OpenOptions;
        let real = self
            .confined(path, false)
            .or_else(|_| self.confined(path, true))?;
        let read = mode.contains('r') || mode.contains('+');
        let write = mode.contains('w') || mode.contains('a') || mode.contains('+');
        let mut opts = OpenOptions::new();
        opts.read(read).write(write);
        if mode.contains('w') {
            opts.create(true).truncate(true);
        } else if mode.contains('a') {
            opts.create(true).append(true);
        }
        let file = opts.open(&real).map_err(|e| io_err(&e))?;
        let h = self.next_handle;
        self.next_handle += 1;
        self.files.insert(h, file);
        Ok(h)
    }

    fn close(&mut self, handle: u64) -> Result<(), HostError> {
        self.files
            .remove(&handle)
            .map(|_| ())
            .ok_or_else(|| HostError::new("invalid file handle"))
    }

    fn read(&mut self, handle: u64, buf: &mut [u8]) -> Result<usize, HostError> {
        use std::io::Read;
        match self.files.get_mut(&handle) {
            Some(f) => f.read(buf).map_err(|e| io_err(&e)),
            None => Err(HostError::new("invalid file handle")),
        }
    }

    fn write(&mut self, handle: u64, bytes: &[u8]) -> Result<usize, HostError> {
        use std::io::Write;
        match self.files.get_mut(&handle) {
            Some(f) => f.write(bytes).map_err(|e| io_err(&e)),
            None => Err(HostError::new("invalid file handle")),
        }
    }

    fn seek(&mut self, handle: u64, whence: SeekWhence, offset: i64) -> Result<u64, HostError> {
        use std::io::Seek;
        let pos = match whence {
            SeekWhence::Set => std::io::SeekFrom::Start(offset.max(0) as u64),
            SeekWhence::Cur => std::io::SeekFrom::Current(offset),
            SeekWhence::End => std::io::SeekFrom::End(offset),
        };
        match self.files.get_mut(&handle) {
            Some(f) => f.seek(pos).map_err(|e| io_err(&e)),
            None => Err(HostError::new("invalid file handle")),
        }
    }

    fn flush(&mut self, handle: u64) -> Result<(), HostError> {
        use std::io::Write;
        match self.files.get_mut(&handle) {
            Some(f) => f.flush().map_err(|e| io_err(&e)),
            None => Err(HostError::new("invalid file handle")),
        }
    }

    fn remove(&mut self, path: &str) -> Result<(), HostError> {
        let real = self.confined(path, false)?;
        std::fs::remove_file(&real).map_err(|e| io_err(&e))
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), HostError> {
        let src = self.confined(from, false)?;
        let dst = self.confined(to, true)?;
        std::fs::rename(&src, &dst).map_err(|e| io_err(&e))
    }

    fn tmpname(&mut self) -> Result<String, HostError> {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        Ok(format!("slew_tmp_{pid}_{nanos}"))
    }

    fn getenv(&mut self, name: &str) -> Option<Vec<u8>> {
        std::env::var_os(name).map(|v| v.to_string_lossy().into_owned().into_bytes())
    }

    fn clock(&mut self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn time(&mut self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64)
    }

    fn time_parts(&mut self, t: i64, _utc: bool) -> DateParts {
        // No timezone database is available without extra dependencies, so
        // local time is treated as UTC. Deterministic and portable.
        civil_from_unix(t)
    }

    fn make_time(&mut self, parts: DateParts) -> i64 {
        unix_from_civil(parts)
    }

    fn setlocale(&mut self, locale: Option<&str>, _category: Option<&str>) -> Option<String> {
        match locale {
            None | Some("" | "C" | "POSIX") => Some("C".to_string()),
            Some(_) => None,
        }
    }
}

/// Days from civil date (Howard Hinnant's algorithm), then seconds.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse: civil date from days (Hinnant).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn civil_from_unix(t: i64) -> DateParts {
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    // 1970-01-01 was a Thursday (wday 4).
    let wday = (days + 4).rem_euclid(7);
    let yday = days - days_from_civil(year, 1, 1) + 1;
    DateParts {
        year,
        month,
        day,
        hour: secs / 3600,
        min: (secs / 60) % 60,
        sec: secs % 60,
        wday,
        yday,
        isdst: false,
    }
}

fn unix_from_civil(parts: DateParts) -> i64 {
    let days = days_from_civil(parts.year, parts.month, parts.day);
    days * 86400 + parts.hour * 3600 + parts.min * 60 + parts.sec
}
