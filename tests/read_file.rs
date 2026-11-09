//! The built-in `read_file` pages files and refuses the Gears state.

use gears::cancellation::Cancellation;
use gears::process::Stream;
use gears::provider::ToolOutcome;
use gears::tools::{Context, Registry};

fn run(context: &Context, arguments: &str) -> (Result<String, String>, ToolOutcome) {
    let registry = Registry::new();
    let tool = registry.get("read_file").unwrap();
    assert!(tool.read_only);
    tool.execute(
        arguments,
        context,
        &Cancellation::new(),
        &mut |_: Stream, _: &str| {},
    )
}

#[test]
fn pages_files_and_refuses_the_gears_state() {
    let root = std::env::temp_dir().join(format!("gears-read-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("workspace")).unwrap();
    std::fs::create_dir_all(root.join("state")).unwrap();
    std::fs::write(root.join("workspace/source"), "first\nsecond\n").unwrap();
    std::fs::write(root.join("state/key"), "secret").unwrap();
    let context = Context::new(
        root.join("workspace"),
        root.join("state"),
        "fixture".into(),
        None,
    )
    .unwrap();
    let (result, outcome) = run(&context, r#"{"path":"source"}"#);
    assert_eq!(outcome, ToolOutcome::Completed, "{result:?}");
    let page: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
    assert_eq!(page["content"], "1: first\n2: second\n");
    assert_eq!(page["all_pages_read"], true);
    let (refused, _) = run(
        &context,
        serde_json::json!({"path": root.join("state/key")})
            .to_string()
            .as_str(),
    );
    let error = refused.unwrap_err();
    assert!(error.contains("Gears directory"), "{error}");
    assert!(!error.contains("secret"));
    std::fs::remove_dir_all(root).unwrap();
}
