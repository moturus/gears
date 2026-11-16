#![allow(dead_code)]

use std::path::Path;

pub const SHELL: &str = "/system/bin/rush";

/// The system prompt file every test configuration names, and its text.
pub const SYSTEM_PROMPT_FILE: &str = "SYSTEM.md";
pub const SYSTEM_PROMPT: &str = "You are a test agent. Keep responses short.\n";

pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

/// Write a gears.toml whose system prompt is the file it names beside it. The
/// body holds the sections: `version` and the prompt settings come first.
pub fn write_config(path: &Path, body: &str) -> std::io::Result<()> {
    let directory = path.parent().expect("a test configuration has a directory");
    write(directory.join(SYSTEM_PROMPT_FILE), SYSTEM_PROMPT)?;
    write(
        path,
        format!(
            "version = 1\nsystem_prompt_linux = \"{SYSTEM_PROMPT_FILE}\"\nsystem_prompt_motor_os = \"{SYSTEM_PROMPT_FILE}\"\n{body}"
        ),
    )
}
