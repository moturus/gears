//! One user-state directory shared by configuration, locks and session storage.

use std::path::PathBuf;

pub const ENV: &str = "GEARS_DIRECTORY";

pub fn default_root() -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os(ENV) {
        let root = PathBuf::from(value);
        if !root.is_absolute() {
            return Err("GEARS_DIRECTORY must be an absolute directory".into());
        }
        crate::state::StateRoot::new(root.clone())?;
        return Ok(root);
    }
    Ok(PathBuf::from("/user/cfg/gears"))
}
