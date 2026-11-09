//! The five tools built into Gears: a shell, file editing, and session search.

pub mod context;
mod file_patch;
mod file_read;
mod file_write;
mod patch_match;
mod record;
mod search_session;
mod sh;

use std::path::Path;

use serde_json::{Value, json};

use crate::cancellation::Cancellation;
use crate::process::Stream;
use crate::provider::{ToolOutcome, ToolSpec};

pub use context::Context;

/// Model-facing bytes kept from each shell stream; the full output stays in
/// the session and the UI.
pub const MODEL_STREAM_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Sh,
    ReadFile,
    WriteFile,
    Patch,
    SessionSearch,
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
    pub read_only: bool,
    kind: Kind,
}

/// The five tools built into Gears; all of them are always enabled.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    tools: Vec<Tool>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            tools: vec![
                Tool::new(Kind::Sh, "sh", sh::DESCRIPTION, sh::parameters(), false),
                Tool::new(
                    Kind::ReadFile,
                    "read_file",
                    "Read numbered UTF-8 file text in bounded pages. Follow next_offset \
                     until all_pages_read before overwriting an existing file.",
                    json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "File to read."},
                            "offset": {"type": "integer", "minimum": 0,
                                "description": "Byte offset returned as next_offset."}
                        },
                        "required": ["path"],
                        "additionalProperties": false
                    }),
                    true,
                ),
                Tool::new(
                    Kind::WriteFile,
                    "write_file",
                    "Atomically write UTF-8 text. Read every page of an existing file first; \
                     preserve its BOM, Windows lines and access.",
                    json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "File to write."},
                            "content": {"type": "string",
                                "description": "Complete new contents."}
                        },
                        "required": ["path", "content"],
                        "additionalProperties": false
                    }),
                    false,
                ),
                Tool::new(
                    Kind::Patch,
                    "patch",
                    "Replace one unique text block in a completely read UTF-8 file, reindent it \
                     and return a diff. Ambiguous matches fail.",
                    json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "File to patch."},
                            "old_text": {"type": "string",
                                "description": "Text to replace, exactly as read."},
                            "new_text": {"type": "string", "description": "Replacement text."}
                        },
                        "required": ["path", "old_text", "new_text"],
                        "additionalProperties": false
                    }),
                    false,
                ),
                Tool::new(
                    Kind::SessionSearch,
                    "session_search",
                    "Search saved session text across every branch and workspace. Hits retain \
                     session and entry IDs and identify off-branch results.",
                    json!({
                        "type": "object",
                        "properties": {
                            "query": {"type": "string",
                                "description": "Case-insensitive text to find, up to 256 bytes."},
                            "offset": {"type": "integer", "minimum": 0,
                                "description": "Matches to skip; continue from next_offset."},
                            "limit": {"type": "integer", "minimum": 1, "maximum": 10,
                                "description": "Maximum hits to return; 10 by default."}
                        },
                        "required": ["query"],
                        "additionalProperties": false
                    }),
                    true,
                ),
            ],
        }
    }

    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.iter().find(|tool| tool.name == name)
    }
}

impl Tool {
    fn new(
        kind: Kind,
        name: &'static str,
        description: &'static str,
        parameters: Value,
        read_only: bool,
    ) -> Self {
        Self {
            name,
            description,
            parameters,
            read_only,
            kind,
        }
    }

    pub fn spec(&self) -> ToolSpec {
        ToolSpec::new(self.name, self.description, self.parameters.clone())
    }

    /// A one-line display of what the call does, for the session and the UI.
    pub fn display_detail(
        &self,
        arguments: &str,
        session_path: Option<&Path>,
    ) -> Result<String, String> {
        let values: Value = serde_json::from_str(arguments)
            .map_err(|error| format!("invalid arguments for {}: {error}", self.name))?;
        let text = |name: &str| {
            values
                .get(name)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{} display needs argument {name:?}", self.name))
        };
        Ok(match self.kind {
            Kind::Sh => {
                let command = text("command")?;
                match sh::state_directory(session_path) {
                    Some(directory) => format!("{command} (in {})", directory.display()),
                    None => command.to_string(),
                }
            }
            Kind::ReadFile | Kind::WriteFile | Kind::Patch => text("path")?.to_string(),
            Kind::SessionSearch => format!("{:?}", text("query")?),
        })
    }

    /// Run one call; `Err` is the tool result the model sees as a failure.
    pub fn execute(
        &self,
        arguments: &str,
        context: &Context,
        cancellation: &Cancellation,
        observer: &mut dyn FnMut(Stream, &str),
    ) -> (Result<String, String>, ToolOutcome) {
        if self.kind == Kind::Sh {
            return sh::execute(arguments, context, cancellation, observer);
        }
        let result = match self.kind {
            Kind::Sh => unreachable!(),
            Kind::ReadFile => file_read::execute(arguments, context),
            Kind::WriteFile => file_write::execute(arguments, context),
            Kind::Patch => file_patch::execute(arguments, context),
            Kind::SessionSearch => search_session::execute(arguments, context),
        };
        match result {
            Ok(value) => (Ok(value.to_string()), ToolOutcome::Completed),
            Err(error) => (Err(error), ToolOutcome::NotStarted),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_every_built_in_tool_in_order() {
        let tools = Registry::new();
        let names = tools
            .tools()
            .iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["sh", "read_file", "write_file", "patch", "session_search"]
        );
    }
}
