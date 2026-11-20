#![allow(dead_code)]

use std::path::Path;

pub const SHELL: &str = "/user/bin/rush";

pub const SYSTEM_PROMPT_FILE: &str = "SYSTEM.md";
pub const SYSTEM_PROMPT: &str = "You are a test agent. Keep responses short.\n";

pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

/// Write a test configuration and its sibling SYSTEM.md.
pub fn write_config(path: &Path, body: &str) -> std::io::Result<()> {
    let directory = path.parent().expect("a test configuration has a directory");
    write(directory.join(SYSTEM_PROMPT_FILE), SYSTEM_PROMPT)?;
    write(path, format!("version = 1\n{body}"))
}
