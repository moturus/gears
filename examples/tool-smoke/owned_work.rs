//! Owned nested-run cleanup on Motor OS.

use std::path::Path;
use std::time::Duration;

use gears::mock::{MockServer, sse_response};
use gears::process::{Cancellation, Request, Stream, run};
use serde_json::{Value, json};

pub fn smoke(gears: &Path) -> Result<(), String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("gears-owned-work-{}-{stamp}", std::process::id()));
    std::fs::create_dir(&root).map_err(|error| error.to_string())?;
    let result = check(gears, &root);
    if result.is_ok() {
        std::fs::remove_dir_all(&root).map_err(|error| error.to_string())?;
    } else {
        eprintln!("owned-work fixtures retained in {}", root.display());
    }
    result
}

fn check(gears: &Path, root: &Path) -> Result<(), String> {
    for mode in ["normal", "cancel", "timeout"] {
        let workspace = root.join(mode);
        std::fs::create_dir(&workspace).map_err(|error| error.to_string())?;
        let tree = "printf '%s' $$ > child.pid; sleep 30 & printf '%s' $! > grandchild.pid; printf nested-ready; wait";
        let done = sse_response(&[
            r#"{"choices":[{"index":0,"delta":{"content":"nested done"},"finish_reason":"stop"}]}"#,
        ]);
        // A normal run's background job must not outlive its tool or block the
        // next call; a nested timeout stops only its tool and the run goes on.
        let scripts = match mode {
            "normal" => vec![
                sse_response(&[&call(
                    "first",
                    "sleep 30 & printf '%s' $! > leftover.pid; printf nested-normal",
                    30,
                )]),
                sse_response(&[&call("second", "printf second-ran", 30)]),
                done,
            ],
            "cancel" => vec![sse_response(&[&call("tree", tree, 30)])],
            _ => vec![sse_response(&[&call("tree", tree, 1)]), done],
        };
        let server = MockServer::start(scripts).map_err(|error| error.to_string())?;
        let key = workspace.join("key");
        std::fs::write(&key, "fixture-owned-key").map_err(|error| error.to_string())?;
        let config = workspace.join("gears.toml");
        std::fs::write(&config, format!(
            "version = 1\n[provider]\nbase_url = {:?}\nmodel = 'test/model'\napi_key_file = {:?}\n[net]\negress_allowlist = ['127.0.0.1']\nplain_http_allowlist = ['127.0.0.1']\n",
            server.url("/v1"), key,
        )).map_err(|error| error.to_string())?;
        let cancellation = Cancellation::new();
        let mut streamed = String::new();
        let output = run(
            &Request {
                program: gears.display().to_string(),
                args: vec![
                    "--config".into(),
                    config.display().to_string(),
                    "--workspace".into(),
                    workspace.display().to_string(),
                    "--ephemeral".into(),
                    "--output".into(),
                    "jsonl".into(),
                    "-p".into(),
                    "nested".into(),
                ],
                cwd: workspace.clone(),
                stdin: None,
                timeout: Duration::from_secs(10),
                max_output_bytes: 65536,
                env: vec![
                    ("HOME".into(), workspace.display().to_string()),
                    (
                        gears::directory::ENV.into(),
                        workspace.join(".gears").display().to_string(),
                    ),
                ],
                remove_env: vec![gears::provider::KEY_ENV.into()],
            },
            &cancellation,
            &mut |stream, text| {
                if stream == Stream::Stdout {
                    streamed.push_str(text);
                    if mode == "cancel"
                        && streamed
                            .lines()
                            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                            .any(|record| {
                                record["type"] == "tool_output"
                                    && record["text"]
                                        .as_str()
                                        .is_some_and(|text| text.contains("nested-ready"))
                            })
                    {
                        cancellation.cancel();
                    }
                }
            },
        )
        .map_err(|error| format!("{mode}: {error}"))?;
        if output.timed_out {
            return Err(format!("{mode}: owning runner timed out: {output:?}"));
        }
        if mode == "cancel" {
            if !output.cancelled || output.stdout.contains("\"type\":\"final\"") {
                return Err(format!("cancel: nested run was not killed: {output:?}"));
            }
        } else if !output.success() || !output.stdout.contains("nested done") {
            return Err(format!("{mode}: nested run did not finish: {output:?}"));
        }
        let pids: &[&str] = if mode == "normal" {
            if !output.stdout.contains("second-ran") {
                return Err(format!(
                    "normal: the next tool call did not run: {output:?}"
                ));
            }
            &["leftover.pid"]
        } else {
            if !streamed.contains("nested-ready")
                || (mode == "timeout" && !output.stdout.contains("timed out"))
            {
                return Err(format!("{mode}: missing progress: {output:?}"));
            }
            &["child.pid", "grandchild.pid"]
        };
        for name in pids {
            let pid = std::fs::read_to_string(workspace.join(name))
                .map_err(|error| error.to_string())?
                .parse::<u32>()
                .map_err(|error| error.to_string())?;
            if !stopped(pid) {
                return Err(format!("{mode}: descendant {pid} still running"));
            }
        }
    }
    Ok(())
}

/// Death is not instantaneous, so give a killed descendant a moment to go.
fn stopped(pid: u32) -> bool {
    fn gone(pid: u32) -> bool {
        !gears::platform::process_alive(pid)
    }
    for _ in 0..100 {
        if gone(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn call(id: &str, command: &str, timeout: u64) -> String {
    json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
        "index": 0, "id": id, "function": {"name": "sh",
        "arguments": json!({"command": command, "timeout": timeout}).to_string()}
    }]}, "finish_reason": "tool_calls"}]})
    .to_string()
}
