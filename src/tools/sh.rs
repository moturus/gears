//! A shell for the workspace, with directory and exports kept per session.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::cancellation::Cancellation;
use crate::process::{self, Stream};
use crate::provider::ToolOutcome;
use crate::shell_state;
use crate::tools::{Context, MODEL_STREAM_BYTES};

pub const DESCRIPTION: &str = "Run a rush command; directory and exports persist within this \
     session. Use rg instead of grep; sysbox has head, tail, diff, find, wc, cat, less (no \
     tail -f), and mkdir -p. Use gix instead of git; sed is on the developer image. For missing \
     commands use sed or rg -o/-r for awk, : > FILE for touch, shell parameter expansion for \
     basename or dirname, redirection for tee, the tool timeout for timeout, and a fixed path \
     under /user/tmp for mktemp. Commands are not sandboxed.";

const SHELL: &str = "/system/bin/rush";
const FIXED_PATH: &str = "/system/bin:/devtools/bin";

const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_TIMEOUT_SECONDS: u64 = 120;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sh {
    command: String,
    #[serde(default = "timeout")]
    timeout: u64,
}

fn timeout() -> u64 {
    MAX_TIMEOUT_SECONDS
}

pub fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "description": "Shell command to execute."},
            "timeout": {"type": "integer", "minimum": 1, "maximum": 120,
                "description": "Optional timeout in seconds, from 1 to 120."}
        },
        "required": ["command"],
        "additionalProperties": false
    })
}

/// Restore this session's directory and exports, run `$1`, then save them.
fn script() -> String {
    format!(
        r#"if [ -n "$GEARS_SH_STATE_DIR" ] && [ -f "$GEARS_SH_STATE_DIR/env.sh" ]; then
    . "$GEARS_SH_STATE_DIR/env.sh" || exit 1
fi
PATH={FIXED_PATH}
export PATH
if [ -n "$GEARS_SH_STATE_DIR" ] && [ -f "$GEARS_SH_STATE_DIR/cwd" ]; then
    IFS= read -r GEARS_SH_SAVED_CWD < "$GEARS_SH_STATE_DIR/cwd" || exit 1
    cd "$GEARS_SH_SAVED_CWD" || cd "$GEARS_WORKSPACE" || exit 1
fi

eval "$1"
GEARS_SH_STATUS=$?
if [ -n "$GEARS_SH_STATE_DIR" ]; then
    umask 077
    pwd > "$GEARS_SH_STATE_DIR/cwd.pending" || exit 1
    export -p > "$GEARS_SH_STATE_DIR/env.pending" || exit 1
fi
exit "$GEARS_SH_STATUS"
"#
    )
}

/// The directory whose snapshots this session restores, without creating it.
pub fn state_directory(session_path: Option<&Path>) -> Option<PathBuf> {
    session_path.map(|path| path.with_extension("sh-state"))
}

pub fn execute(
    arguments: &str,
    context: &Context,
    cancellation: &Cancellation,
    observer: &mut dyn FnMut(Stream, &str),
) -> (Result<String, String>, ToolOutcome) {
    let arguments: Sh = match serde_json::from_str(arguments) {
        Ok(arguments) => arguments,
        Err(error) => {
            return (
                Err(format!("invalid sh arguments: {error}")),
                ToolOutcome::NotStarted,
            );
        }
    };
    if !(1..=MAX_TIMEOUT_SECONDS).contains(&arguments.timeout) {
        return (
            Err("sh timeout must be from 1 to 120 seconds".into()),
            ToolOutcome::NotStarted,
        );
    }
    match run(&arguments, context, cancellation, observer) {
        Ok(output) => {
            let interrupted = output.cancelled || output.timed_out;
            let content = output.model_text(MODEL_STREAM_BYTES);
            let result = if output.code == Some(0) && !interrupted {
                Ok(content)
            } else {
                Err(content)
            };
            let outcome = if interrupted {
                ToolOutcome::InterruptedUnknown
            } else {
                ToolOutcome::Completed
            };
            (result, outcome)
        }
        Err(error) => (
            Err(error.detail),
            if error.started {
                ToolOutcome::InterruptedUnknown
            } else {
                ToolOutcome::NotStarted
            },
        ),
    }
}

fn run(
    arguments: &Sh,
    context: &Context,
    cancellation: &Cancellation,
    observer: &mut dyn FnMut(Stream, &str),
) -> Result<process::Output, process::RunError> {
    let state = context.shell_state.as_deref();
    if let Some(directory) = state {
        shell_state::prepare(directory).map_err(|detail| process::RunError {
            detail: format!("cannot prepare shell state: {detail}"),
            started: false,
        })?;
    }
    let mut env = vec![
        (
            "GEARS_WORKSPACE".to_string(),
            context.workspace.display().to_string(),
        ),
        ("GEARS_SESSION_ID".to_string(), context.session.clone()),
    ];
    let mut remove_env = vec![crate::provider::KEY_ENV.to_string()];
    match state {
        Some(directory) => env.push((
            "GEARS_SH_STATE_DIR".to_string(),
            directory.display().to_string(),
        )),
        None => remove_env.push("GEARS_SH_STATE_DIR".to_string()),
    }
    let output = process::run(
        &process::Request {
            program: SHELL.to_string(),
            args: vec![
                "-c".to_string(),
                script(),
                "gears-sh".to_string(),
                arguments.command.clone(),
            ],
            cwd: context.workspace.clone(),
            stdin: None,
            timeout: Duration::from_secs(arguments.timeout),
            max_output_bytes: MAX_OUTPUT_BYTES,
            env,
            remove_env,
        },
        cancellation,
        observer,
    );
    if let Some(directory) = state {
        let interrupted = match &output {
            Ok(output) => output.cancelled || output.timed_out,
            Err(_) => true,
        };
        if let Err(detail) = shell_state::finish(directory, interrupted) {
            let detail = format!("cannot save shell state: {detail}");
            return Err(match output {
                Ok(_) => process::RunError {
                    detail,
                    started: true,
                },
                Err(mut error) => {
                    error.detail.push_str(&format!("; {detail}"));
                    error
                }
            });
        }
    }
    output
}
