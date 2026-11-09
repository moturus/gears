//! Hermetic built-in tool checks, also installed as a developer-image test.

use std::path::Path;

use gears::process::{Cancellation, Stream};
use gears::provider::{Message, ToolOutcome};
use gears::session::Store;
use gears::tools::{Context, Registry};
use serde_json::{Value, json};

#[path = "tool-smoke/directory_lock.rs"]
mod directory_lock;
#[path = "tool-smoke/file_access.rs"]
mod file_access;
#[path = "tool-smoke/fork_entry.rs"]
mod fork_entry;
#[path = "tool-smoke/owned_work.rs"]
mod owned_work;
#[path = "tool-smoke/state_directory.rs"]
mod state_directory;

#[cfg(not(test))]
fn main() -> Result<(), String> {
    let directory = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .ok_or("usage: tool-smoke GEARS_DIRECTORY [GEARS_EXECUTABLE]")?;
    smoke(&directory)?;
    let gears = std::env::current_exe()
        .map_err(|error| error.to_string())?
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("gears");
    let gears = std::env::args_os()
        .nth(2)
        .map(std::path::PathBuf::from)
        .unwrap_or(gears);
    owned_work::smoke(&gears)?;
    directory_lock::smoke(&gears)?;
    fork_entry::smoke(&gears)?;
    file_access::smoke()?;
    state_directory::smoke(&gears)?;
    println!("gears shipped tools ALL PASS");
    Ok(())
}

fn smoke(directory: &Path) -> Result<(), String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let workspace = directory.join(format!("tool-smoke-{}-{stamp}", std::process::id()));
    let _ = std::fs::remove_dir_all(&workspace);
    std::fs::create_dir(&workspace).map_err(|error| error.to_string())?;
    let result = check_tools(&workspace);
    if result.is_ok() {
        std::fs::remove_dir_all(&workspace).map_err(|error| error.to_string())?;
    } else {
        eprintln!("tool smoke fixtures retained in {}", workspace.display());
    }
    result
}

fn execute(
    registry: &Registry,
    context: &Context,
    name: &str,
    arguments: Value,
) -> Result<String, String> {
    let tool = registry
        .get(name)
        .ok_or_else(|| format!("{name} is not a built-in tool"))?;
    let (result, outcome) = tool.execute(
        &arguments.to_string(),
        context,
        &Cancellation::new(),
        &mut |_: Stream, _: &str| {},
    );
    if outcome != ToolOutcome::Completed {
        return Err(format!("{name}: {}: {outcome:?}", result.unwrap_err()));
    }
    result.map_err(|error| format!("{name}: {error}"))
}

fn check_tools(workspace: &Path) -> Result<(), String> {
    let state = workspace.join("state");
    std::fs::create_dir(&state).map_err(|error| error.to_string())?;
    let shell_state = workspace.join("shell-state");
    std::fs::create_dir(&shell_state).map_err(|error| error.to_string())?;
    let registry = Registry::new();
    assert_eq!(
        registry
            .tools()
            .iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>(),
        ["sh", "read_file", "write_file", "patch", "session_search"]
    );
    let directory = "literal * [x]";
    std::fs::create_dir(workspace.join(directory)).map_err(|error| error.to_string())?;
    let context = Context::new(
        workspace.to_path_buf(),
        state.clone(),
        "tool-smoke".into(),
        Some(shell_state),
    )?;
    execute(
        &registry,
        &context,
        "write_file",
        json!({"path": "literal * [x]/a * [x].txt", "content": "first\nlast\n"}),
    )?;
    let page = execute(
        &registry,
        &context,
        "read_file",
        json!({"path": "literal * [x]/a * [x].txt"}),
    )?;
    let page: Value = serde_json::from_str(&page).map_err(|error| error.to_string())?;
    assert_eq!(page["content"], "1: first\n2: last\n");
    assert_eq!(page["all_pages_read"], true);
    let patched = execute(
        &registry,
        &context,
        "patch",
        json!({"path": "literal * [x]/a * [x].txt",
            "old_text": "first\n", "new_text": "changed\n"}),
    )?;
    assert!(patched.contains("changed"), "{patched}");
    assert_eq!(
        std::fs::read_to_string(workspace.join("literal * [x]/a * [x].txt")).unwrap(),
        "changed\nlast\n"
    );
    let refused = registry.get("write_file").unwrap().execute(
        &json!({"path": "state/intruder", "content": "x"}).to_string(),
        &context,
        &Cancellation::new(),
        &mut |_: Stream, _: &str| {},
    );
    assert!(
        refused
            .0
            .as_ref()
            .is_err_and(|error| error.contains("Gears directory")),
        "{refused:?}"
    );

    let store = Store::with_root(workspace, state.clone()).map_err(|error| error.to_string())?;
    let mut session = store
        .create(false, None)
        .map_err(|error| error.to_string())?;
    session
        .append_message(&Message::user("needle question"))
        .map_err(|error| error.to_string())?;
    session
        .append_message(&Message::assistant("needle answer"))
        .map_err(|error| error.to_string())?;
    let found = execute(
        &registry,
        &context,
        "session_search",
        json!({"query": "needle", "limit": 10}),
    )?;
    let found: Value = serde_json::from_str(&found).map_err(|error| error.to_string())?;
    assert_eq!(found["total_matches"], 2);
    assert_eq!(found["hits"][0]["session_id"], session.id());

    execute(
        &registry,
        &context,
        "sh",
        json!({"command": "[ -z \"${OPENROUTER_API_KEY+x}\" ] && export TOOL_SMOKE_STATE=retained && cd 'literal * [x]'"}),
    )?;
    let restored = execute(
        &registry,
        &context,
        "sh",
        json!({"command": "printf '%s\\n' \"$TOOL_SMOKE_STATE\"; pwd"}),
    )?;
    let expected = format!("retained\n{}\n", workspace.join(directory).display());
    assert!(restored.ends_with(&expected), "{restored}");
    assert!(restored.contains("exit status 0"), "{restored}");
    Ok(())
}

#[test]
fn built_in_tools_follow_their_contracts() {
    smoke(Path::new(env!("CARGO_TARGET_TMPDIR"))).unwrap();
}

#[test]
fn nested_work_stops_its_own_tools_and_parents_stop_everything() {
    owned_work::smoke(Path::new(env!("CARGO_BIN_EXE_gears"))).unwrap();
}

#[test]
fn another_gears_cannot_replace_a_live_directory_lock() {
    directory_lock::smoke(Path::new(env!("CARGO_BIN_EXE_gears"))).unwrap();
}

#[test]
fn atomic_workspace_replacement_keeps_executable_access() {
    file_access::smoke().unwrap();
}

#[test]
fn startup_forks_select_old_entries_without_locking_or_changing_the_parent() {
    fork_entry::smoke(Path::new(env!("CARGO_BIN_EXE_gears"))).unwrap();
}

#[test]
fn selected_directories_keep_defaults_locks_and_sessions_together() {
    state_directory::smoke(Path::new(env!("CARGO_BIN_EXE_gears"))).unwrap();
}
