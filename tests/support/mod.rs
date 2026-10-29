#![allow(dead_code)]

use std::path::Path;

pub const SHELL: &str = if cfg!(unix) {
    "/bin/sh"
} else {
    "/system/bin/rush"
};

// Existing fixtures include shell paths in TOML and script shebangs. Adapt
// those paths when writing the same fixture on Motor OS.
pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    #[cfg(unix)]
    let contents = contents.as_ref().to_vec();
    #[cfg(not(unix))]
    let contents = String::from_utf8_lossy(contents.as_ref())
        .replace("/bin/sh", SHELL)
        .into_bytes();
    std::fs::write(path, contents)
}
