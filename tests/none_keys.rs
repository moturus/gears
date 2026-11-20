mod support;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use gears::mock::{MockServer, sse_response};
use gears::provider::KEY_ENV;
use moto_sys::caps::MOTOR_OS_CAPS_ENV_KEY;

// Spawn, network and filesystem writes, without Interactive.
const NONE_CAPS: &str = "0x304";

fn run(root: &Path, args: &[&str], env_key: Option<&str>, input: &[u8]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gears"));
    command
        .current_dir(root)
        .args(args)
        .env(MOTOR_OS_CAPS_ENV_KEY, NONE_CAPS)
        .env(gears::directory::ENV, root)
        .env_remove("GEARS_LOCK_OWNER")
        .env_remove(KEY_ENV)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(key) = env_key {
        command.env(KEY_ENV, key);
    }
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn none_refuses_stored_keys_and_authenticates_with_transient_keys() {
    let root = Path::new("/none/tmp").join(format!("gears-none-keys-{}", std::process::id()));
    assert!(
        Command::new("/user/bin/mkdir")
            .env(MOTOR_OS_CAPS_ENV_KEY, NONE_CAPS)
            .arg(&root)
            .status()
            .unwrap()
            .success()
    );
    let reply = || {
        sse_response(&[
            r#"{"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":"stop"}]}"#,
        ])
    };
    let args = serde_json::json!({"command": r#"[ -z "${OPENROUTER_API_KEY+x}" ] && echo clean > child-env"#});
    let call = serde_json::json!({"index": 0, "id": "env", "function": {"name": "sh", "arguments": args.to_string()}});
    let check_env = serde_json::json!({"choices": [{"index": 0, "delta": {"tool_calls": [call]}, "finish_reason": "tool_calls"}]}).to_string();
    let server = MockServer::start(vec![
        sse_response(&[&check_env]),
        reply(),
        reply(),
        reply(),
        reply(),
        reply(),
    ])
    .unwrap();
    let config = format!(
        "default_provider = 'first'\ndefault_model = 'test/model'\n\
        [net]\negress_allowlist = ['127.0.0.1']\nplain_http_allowlist = ['127.0.0.1']\n\
        [providers.first]\nbase_url = '{url}'\napi_key_file = 'first.key'\nmodels = ['test/model']\n\
        [providers.second]\nbase_url = '{url}'\napi_key_file = 'second.key'\nmodels = ['test/model']\n\
        [providers.local]\nbase_url = '{url}'\n",
        url = server.url("/v1")
    );
    support::write_config(&root.join("gears.toml"), &config).unwrap();
    let other = root.join("second.key");
    std::fs::write(&other, "stored-fixture-key\n").unwrap();
    let prompt = root.join(support::SYSTEM_PROMPT_FILE);
    std::fs::remove_file(&prompt).unwrap();
    let refused = run(
        &root,
        &["--log-file", "trace", "--ui", "tui"],
        Some("env-fixture-key"),
        b"",
    );
    assert!(!refused.status.success());
    assert!(refused.stdout.is_empty());
    assert_eq!(
        String::from_utf8(refused.stderr).unwrap(),
        format!(
            "API key available to None user at {}: please remove it.\n",
            other.display()
        )
    );
    assert!(server.requests().is_empty());
    assert!(!root.join("trace").exists() && !root.join("sessions").exists());
    std::fs::remove_file(&other).unwrap();
    std::fs::write(prompt, support::SYSTEM_PROMPT).unwrap();

    let cancelled = run(&root, &["ask", "hello"], None, b"");
    assert!(!cancelled.status.success());
    assert!(server.requests().is_empty());

    let supplied = run(
        &root,
        &["--ephemeral", "-p", "check environment"],
        Some("env-fixture-key"),
        b"",
    );
    assert!(supplied.status.success(), "{supplied:?}");
    assert!(!String::from_utf8_lossy(&supplied.stderr).contains("Paste"));
    assert_eq!(
        std::fs::read_to_string(root.join("child-env")).unwrap(),
        "clean\n"
    );

    let pasted = run(&root, &["ask", "hello"], None, b"ask-fixture-key\n");
    assert!(pasted.status.success(), "{pasted:?}");
    assert_eq!(pasted.stdout, b"hello\n");
    assert!(String::from_utf8_lossy(&pasted.stderr).contains("Paste the API key for first"));
    assert!(!String::from_utf8_lossy(&pasted.stderr).contains("ask-fixture-key"));

    let jsonl = run(
        &root,
        &["--ephemeral", "-p", "hello", "--output", "jsonl"],
        None,
        b"json-fixture-key\n",
    );
    assert!(jsonl.status.success(), "{jsonl:?}");
    for line in String::from_utf8(jsonl.stdout).unwrap().lines() {
        serde_json::from_str::<serde_json::Value>(line).unwrap();
    }

    let setup = run(&root, &["--ui", "line", "--log-file", "trace", "--provider", "local"], None,
        b"/provider first\nfirst-fixture-key\n/setup\n3\n1\n1\nsecond-fixture-key\nhello\n/provider first\nhello\n/quit\n");
    assert!(setup.status.success(), "{setup:?}");
    for stream in [&setup.stdout, &setup.stderr] {
        assert!(!String::from_utf8_lossy(stream).contains("fixture-key"));
    }
    let requests = server.requests();
    let expected = [
        "env-fixture-key",
        "env-fixture-key",
        "ask-fixture-key",
        "json-fixture-key",
        "second-fixture-key",
        "first-fixture-key",
    ];
    assert_eq!(requests.len(), expected.len());
    for (request, key) in requests.iter().zip(expected) {
        assert_eq!(
            request.header("Authorization"),
            Some(format!("Bearer {key}").as_str())
        );
    }
    assert!(!root.join("first.key").exists() && !other.exists());
    // Include the trace, saved session, shell state and provider settings.
    let leaks = Command::new("/user/bin/rg")
        .args([
            "--hidden",
            "--no-ignore",
            "--text",
            "--files-with-matches",
            "fixture-key",
        ])
        .arg(&root)
        .output()
        .unwrap();
    assert_eq!(leaks.status.code(), Some(1), "{leaks:?}");
    std::fs::remove_dir_all(root).unwrap();
}
