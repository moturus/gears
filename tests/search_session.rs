//! The built-in `search_session` pages saved sessions.

use gears::cancellation::Cancellation;
use gears::process::Stream;
use gears::provider::Message;
use gears::session::Store;
use gears::tools::{Context, Registry};
use serde_json::{Value, json};

fn run(context: &Context, arguments: Value) -> Result<Value, String> {
    let registry = Registry::new();
    let tool = registry.get("search_session").unwrap();
    assert!(tool.read_only);
    let (result, _) = tool.execute(
        &arguments.to_string(),
        context,
        &Cancellation::new(),
        &mut |_: Stream, _: &str| {},
    );
    let result = result?;
    serde_json::from_str(&result).map_err(|error| error.to_string())
}

#[test]
fn pages_saved_sessions_from_the_state_directory() {
    let root = std::env::temp_dir().join(format!("gears-session-search-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("workspace")).unwrap();
    let store = Store::with_root(&root.join("workspace"), root.join("state")).unwrap();
    let mut session = store.create(false, None).unwrap();
    session
        .append_message(&Message::user("needle question"))
        .unwrap();
    session
        .append_message(&Message::assistant("needle answer"))
        .unwrap();
    let context = Context::new(
        root.join("workspace"),
        root.join("state"),
        session.id().to_string(),
        None,
    )
    .unwrap();
    let page = run(&context, json!({"query": "needle", "limit": 1})).unwrap();
    assert_eq!(page["total_matches"], 2);
    assert_eq!(page["next_offset"], 1);
    assert_eq!(page["hits"][0]["session_id"], session.id());
    assert_eq!(page["hits"][0]["off_current_branch"], false);
    for invalid in [
        json!({"query": "needle", "offset": -1}),
        json!({"query": "needle", "limit": 0}),
        json!({"query": "needle", "unknown": true}),
    ] {
        assert!(run(&context, invalid).is_err());
    }
    drop(session);
    std::fs::remove_dir_all(root).unwrap();
}
