//! A `gears.toml` fixture together with the system prompt it names.

use std::path::Path;

/// The prompt file every fixture configuration names, and its text.
pub const PROMPT_FILE: &str = "SYSTEM.md";
pub const PROMPT: &str = "You are a test agent.\n";

/// The settings as the sections of a gears.toml, with the prompt settings this
/// build requires in front of them.
pub fn text(settings: &str) -> String {
    format!(
        "version = 1\nsystem_prompt_linux = '{PROMPT_FILE}'\nsystem_prompt_motor_os = '{PROMPT_FILE}'\n{settings}"
    )
}

/// Write the settings as the sections of a gears.toml, with the prompt file it
/// names in the same directory.
pub fn write(path: &Path, settings: &str) -> Result<(), String> {
    let directory = path
        .parent()
        .ok_or("a test configuration has a directory")?;
    std::fs::write(directory.join(PROMPT_FILE), PROMPT).map_err(|error| error.to_string())?;
    std::fs::write(path, text(settings)).map_err(|error| error.to_string())
}
