//! Atomic replacements preserve the writer's access on Motor OS.

use std::path::Path;

use gears::state::StateRoot;

pub fn smoke() -> Result<(), String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("gears-file-access-{}-{stamp}", std::process::id()));
    let state = StateRoot::new(root.clone())?;
    for access in [3_u64, 5, 7, 1] {
        let name = format!("access-{access}");
        let relative = Path::new(&name);
        state.replace_file(relative, b"first")?;
        let file = state.file(relative)?;
        let mode = {
            use moto_rt::fs::{PERM_EXEC, PERM_READ, set_perm};
            if access & PERM_EXEC != 0 {
                set_perm(file.to_str().unwrap(), PERM_READ | PERM_EXEC)
                    .map_err(|error| format!("{error:?}"))?;
            }
            if access != PERM_READ | PERM_EXEC {
                set_perm(file.to_str().unwrap(), access).map_err(|error| format!("{error:?}"))?;
            }
            moto_rt::fs::stat(file.to_str().unwrap())
                .map_err(|error| format!("{error:?}"))?
                .perm
        };
        state.replace_file_preserving_access(relative, b"replacement")?;
        if std::fs::read(&file).map_err(|error| error.to_string())? != b"replacement" {
            return Err("replacement contents differ".into());
        }
        if moto_rt::fs::stat(file.to_str().unwrap())
            .map_err(|error| format!("{error:?}"))?
            .perm
            != mode
        {
            return Err("replacement caller access differs".into());
        }
    }
    std::fs::remove_dir_all(root).map_err(|error| error.to_string())
}
