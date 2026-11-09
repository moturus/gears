//! File-tool paths and per-session data; these checks prevent accidents, and
//! symbolic links are resolved before them.

use std::path::{Component, Path, PathBuf};

use crate::state::StateRoot;

pub struct Context {
    pub workspace: PathBuf,
    // The whole Gears directory, not just its sessions subdirectory.
    pub state: StateRoot,
    pub session: String,
    pub shell_state: Option<PathBuf>,
}

impl Context {
    pub fn new(
        workspace: PathBuf,
        state: PathBuf,
        session: String,
        shell_state: Option<PathBuf>,
    ) -> Result<Self, String> {
        if session.is_empty()
            || session.len() > 128
            || !session
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err("invalid session id for tool data".into());
        }
        let workspace = resolve_links(&normalize(&workspace)?)?;
        if !workspace.is_dir() {
            return Err("selected workspace is not a directory".into());
        }
        let state = resolve_links(&normalize(&state)?)?;
        let state = StateRoot::new(state)?;
        Ok(Self {
            workspace,
            state,
            session,
            shell_state,
        })
    }

    pub fn resolve(&self, input: &str, writing: bool) -> Result<PathBuf, String> {
        if input.is_empty() {
            return Err("file path is empty".into());
        }
        let path = Path::new(input);
        let path = resolve_links(&normalize(&if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.join(path)
        })?)?;
        if path.starts_with(self.state.path()) {
            return Err(if writing {
                "file tools cannot write in the Gears directory".into()
            } else {
                "file tools cannot read the Gears directory".into()
            });
        }
        Ok(path)
    }

    pub fn relative(&self, path: &Path) -> PathBuf {
        let base: Vec<_> = self.workspace.components().collect();
        let target: Vec<_> = path.components().collect();
        let common = base.iter().zip(&target).take_while(|(a, b)| a == b).count();
        let mut relative = PathBuf::new();
        for _ in common..base.len() {
            relative.push("..");
        }
        for component in &target[common..] {
            relative.push(component.as_os_str());
        }
        relative
    }

    pub fn data_file(&self, name: &str) -> Result<PathBuf, String> {
        self.state.file(Path::new(&format!(
            "tools/sessions/{}/{name}",
            self.session
        )))
    }

    pub fn replace_data(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        self.state.replace_file(
            Path::new(&format!("tools/sessions/{}/{name}", self.session)),
            bytes,
        )
    }
}

/// Remove `.` and `..` lexically; as in POSIX, `..` at the root stays there.
fn normalize(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("tool paths must be absolute".into());
    }
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Normal(_) => result.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            _ => return Err(format!("invalid file path {}", path.display())),
        }
    }
    Ok(result)
}

/// Resolve symbolic links in the longest existing ancestor, so checks see the
/// real location; the part that does not exist yet is kept as written.
fn resolve_links(path: &Path) -> Result<PathBuf, String> {
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(mut real) => {
                real.extend(missing.iter().rev());
                return Ok(real);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
                    return Err(format!("{}: {error}", path.display()));
                };
                missing.push(name);
                existing = parent;
            }
            Err(error) => return Err(format!("{}: {error}", existing.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths_and_protects_gears_state() {
        let root = std::env::temp_dir().join(format!("gears-tools-path-{}", std::process::id()));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        let state = root.join("state");
        let context =
            Context::new(root.join("workspace"), state.clone(), "s1".into(), None).unwrap();
        assert_eq!(
            context.resolve("./src/../file name*", false).unwrap(),
            root.join("workspace/file name*")
        );
        assert_eq!(
            context.resolve("../outside", false).unwrap(),
            root.join("outside")
        );
        assert_eq!(
            context.relative(&root.join("outside")),
            Path::new("../outside")
        );
        assert_eq!(context.resolve("/../..", false).unwrap(), Path::new("/"));
        assert!(
            context
                .resolve(state.join("key").to_str().unwrap(), true)
                .is_err()
        );
        assert!(
            Context::new(
                context.workspace.clone(),
                state.clone(),
                "../escape".into(),
                None
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
