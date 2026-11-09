//! Assembly and identity of reviewable prompt resources.

use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

const DEFAULT_SYSTEM: &str = include_str!("../prompts/system.md");
pub const COMPACTION: &str = include_str!("../prompts/compact.md");

#[derive(Debug, Clone)]
pub struct Effective {
    fragments: Vec<String>,
    prompt_hash: String,
    manifest_hash: String,
}

impl Effective {
    pub fn new(workspace: &Path, manifest: &Value) -> Self {
        let fragments = vec![
            DEFAULT_SYSTEM.trim().to_string(),
            format!("Current working directory: {}", workspace.display()),
        ];
        let prompt_hash = hash_json(&fragments);
        let manifest_hash = hash_json(manifest);
        Self {
            fragments,
            prompt_hash,
            manifest_hash,
        }
    }

    pub fn fragments(&self) -> &[String] {
        &self.fragments
    }

    pub fn identity(&self) -> crate::session::RuntimeIdentity {
        crate::session::RuntimeIdentity {
            prompt_hash: self.prompt_hash.clone(),
            manifest_hash: self.manifest_hash.clone(),
        }
    }
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

    #[test]
    fn workspace_and_manifest_affect_identity() {
        let first = Effective::new(Path::new("/work"), &serde_json::json!({"tools": ["sh"]}));
        let moved = Effective::new(
            Path::new("/elsewhere"),
            &serde_json::json!({"tools": ["sh"]}),
        );
        assert_ne!(first.identity().prompt_hash, moved.identity().prompt_hash);
        let changed = Effective::new(
            Path::new("/work"),
            &serde_json::json!({"tools": ["sh", "extra"]}),
        );
        assert_ne!(
            first.identity().manifest_hash,
            changed.identity().manifest_hash
        );
    }

    #[test]
    fn working_directory_is_explicit() {
        let prompt = Effective::new(Path::new("/chosen"), &Value::Null);
        assert!(
            prompt
                .fragments()
                .iter()
                .any(|fragment| fragment.contains("/chosen"))
        );
    }
}
