#![allow(dead_code)]

use std::path::Path;

pub const SHELL: &str = "/system/bin/rush";

pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    std::fs::write(path, contents)
}
