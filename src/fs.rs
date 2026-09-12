//! Filesystem adapter for [`Lua::set_fs_file_reader`].
//!
//! Only compiled with the (default-on) `fs` feature. The core interpreter
//! never touches the filesystem itself: this module is the one place that
//! does, and only when an embedder asks for it.

use crate::vm::Lua;

impl Lua {
    /// Installs a reading adapter confined to `root`: relative candidates
    /// are resolved under `root`, and absolute paths, `..`, or symlinks that
    /// escape it are refused.
    pub fn set_fs_file_reader(&mut self, root: impl AsRef<std::path::Path>) {
        let root = root.as_ref().to_path_buf();
        self.set_file_reader(move |path| read_confined(&root, path));
    }
}

/// Canonicalizes candidates under `root` and refuses anything that escapes.
fn read_confined(root: &std::path::Path, path: &str) -> Result<Option<Vec<u8>>, String> {
    use std::io::ErrorKind;
    use std::path::Path;
    let root = match root.canonicalize() {
        Ok(p) => p,
        Err(e) => return Err(format!("cannot access reader root: {e}")),
    };
    let candidate = {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        }
    };
    let real = match candidate.canonicalize() {
        Ok(p) => p,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot open {}: {e}", candidate.display())),
    };
    if !real.starts_with(&root) {
        return Err(format!("access denied: '{path}' is outside the reader root"));
    }
    match std::fs::read(&real) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot open {}: {e}", real.display())),
    }
}
