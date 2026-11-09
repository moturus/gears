//! Assembly and identity of reviewable prompt resources.

use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

const DEFAULT_SYSTEM: &str = include_str!("../prompts/system.md");
pub const COMPACTION: &str = include_str!("../prompts/compact.md");
/// The workspace file whose contents every request carries.
pub const INSTRUCTIONS: &str = "AGENTS.md";

#[derive(Debug, Clone)]
pub struct Effective {
    fragments: Vec<String>,
    instructions: Option<String>,
    prompt_hash: String,
    manifest_hash: String,
}

impl Effective {
    /// Load the prompt a request is built from: the built-in system text, the
    /// working directory, and the workspace instructions, if present.
    pub fn new(workspace: &Path, manifest: &Value) -> Result<Self, String> {
        let instructions = instructions(workspace)?;
        let mut fragments = vec![
            DEFAULT_SYSTEM.trim().to_string(),
            format!("Current working directory: {}", workspace.display()),
        ];
        fragments.extend(instructions.clone());
        let prompt_hash = hash_json(&fragments);
        let manifest_hash = hash_json(manifest);
        Ok(Self {
            fragments,
            instructions,
            prompt_hash,
            manifest_hash,
        })
    }

    pub fn fragments(&self) -> &[String] {
        &self.fragments
    }

    /// The workspace instructions fragment, when the workspace has one.
    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    pub fn identity(&self) -> crate::session::RuntimeIdentity {
        crate::session::RuntimeIdentity {
            prompt_hash: self.prompt_hash.clone(),
            manifest_hash: self.manifest_hash.clone(),
        }
    }
}

/// The workspace's AGENTS.md, framed so that the model reads it as workspace
/// instructions rather than as its own system text. Every request built from
/// `Effective` repeats it, so a compaction cannot drop it.
fn instructions(workspace: &Path) -> Result<Option<String>, String> {
    let path = workspace.join(INSTRUCTIONS);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "The workspace file {INSTRUCTIONS} applies to all work in this directory:\n\n{}",
        text.trim_end()
    )))
}

fn hash_json(value: &impl serde::Serialize) -> String {
    let encoded = serde_json::to_vec(value).expect("prompt identity is serializable");
    let digest = Sha256::digest(encoded);
    let mut result = String::with_capacity(digest.len() * 2);
    for byte in digest {
        result.push_str(&format!("{byte:02x}"));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary workspace directory, removed when the test ends.
    struct Workspace(std::path::PathBuf);

    impl Workspace {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("gears-prompt-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn workspace_and_manifest_affect_identity() {
        let first =
            Effective::new(Path::new("/work"), &serde_json::json!({"tools": ["sh"]})).unwrap();
        let moved = Effective::new(
            Path::new("/elsewhere"),
            &serde_json::json!({"tools": ["sh"]}),
        )
        .unwrap();
        assert_ne!(first.identity().prompt_hash, moved.identity().prompt_hash);
        let changed = Effective::new(
            Path::new("/work"),
            &serde_json::json!({"tools": ["sh", "extra"]}),
        )
        .unwrap();
        assert_ne!(
            first.identity().manifest_hash,
            changed.identity().manifest_hash
        );
    }

    #[test]
    fn working_directory_is_explicit() {
        let prompt = Effective::new(Path::new("/chosen"), &Value::Null).unwrap();
        assert!(
            prompt
                .fragments()
                .iter()
                .any(|fragment| fragment.contains("/chosen"))
        );
    }

    #[test]
    fn workspace_instructions_are_sent_and_change_the_identity() {
        let workspace = Workspace::new("instructions");
        let absent = Effective::new(&workspace.0, &Value::Null).unwrap();
        assert!(absent.instructions().is_none());
        assert!(
            absent
                .fragments()
                .iter()
                .all(|fragment| !fragment.contains(INSTRUCTIONS))
        );

        std::fs::write(workspace.0.join(INSTRUCTIONS), "Run the tests.\n").unwrap();
        let present = Effective::new(&workspace.0, &Value::Null).unwrap();
        let instructions = present.instructions().unwrap();
        assert!(instructions.contains("Run the tests."));
        assert!(instructions.contains(INSTRUCTIONS));
        assert!(
            present
                .fragments()
                .iter()
                .any(|fragment| fragment == instructions)
        );
        assert_ne!(
            absent.identity().prompt_hash,
            present.identity().prompt_hash
        );
    }

    #[test]
    fn empty_instructions_are_not_sent() {
        let workspace = Workspace::new("empty-instructions");
        std::fs::write(workspace.0.join(INSTRUCTIONS), " \n").unwrap();
        let prompt = Effective::new(&workspace.0, &Value::Null).unwrap();
        assert!(prompt.instructions().is_none());
    }
}
