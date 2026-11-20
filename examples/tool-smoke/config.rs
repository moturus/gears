//! A configuration fixture and its sibling SYSTEM.md.

use std::path::Path;

pub const PROMPT: &str = "You are a test agent.\n";

pub fn text(settings: &str) -> String {
    format!("version = 1\n{settings}")
}

pub fn write(path: &Path, settings: &str) -> Result<(), String> {
    let directory = path
        .parent()
        .ok_or("a test configuration has a directory")?;
    std::fs::write(directory.join("SYSTEM.md"), PROMPT).map_err(|error| error.to_string())?;
    std::fs::write(path, text(settings)).map_err(|error| error.to_string())
}
