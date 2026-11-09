mod support;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gears::config::{Config, ContextConfig};
use gears::mock::{MockServer, Script, sse_response};
use gears::net::EgressPolicy;
use gears::net::motor_curl::MotorCurl;
use gears::provider::{
    Completion, ContentBlock, Endpoint, EventSink, FinishReason, Message, OpenAiCompat, Provider,
    ProviderError, Quirks, Request, StreamEvent, ToolCall, ToolOutcome,
};
use gears::runtime::{Event, Runtime};
use gears::session::Store;

struct Scripted {
    responses: Mutex<VecDeque<Result<Completion, ProviderError>>>,
    requests: Mutex<Vec<Request>>,
}

impl Scripted {
    fn new(responses: Vec<Result<Completion, ProviderError>>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
}

impl Provider for Scripted {
    fn complete(
        &self,
        request: &Request,
        sink: &mut dyn EventSink,
    ) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted response");
        if let Ok(completion) = &response {
            if !completion.reasoning.is_empty() {
                sink.on_event(StreamEvent::Reasoning(completion.reasoning.clone()))
                    .map_err(|error| ProviderError::Aborted(error.to_string()))?;
            }
            if !completion.content.is_empty() {
                sink.on_event(StreamEvent::Text(completion.content.clone()))
                    .map_err(|error| ProviderError::Aborted(error.to_string()))?;
            }
        }
        response
    }
}

fn answer(text: &str) -> Result<Completion, ProviderError> {
    Ok(Completion {
        content: text.to_string(),
        finish_reason: Some(FinishReason::Stop),
        ..Completion::default()
    })
}

fn priced_answer(text: &str) -> Result<Completion, ProviderError> {
    Ok(Completion {
        content: text.to_string(),
        finish_reason: Some(FinishReason::Stop),
        usage: gears::provider::Usage {
            prompt_tokens: 10,
            completion_tokens: 2,
            ..gears::provider::Usage::default()
        },
        ..Completion::default()
    })
}

fn tool(name: &str, arguments: &str) -> Result<Completion, ProviderError> {
    Ok(Completion {
        tool_calls: vec![ToolCall::new("call_1", name, arguments)],
        finish_reason: Some(FinishReason::ToolCalls),
        ..Completion::default()
    })
}

#[test]
fn context_usage_counts_the_conversation_against_the_window() {
    let fixture = Fixture::new("context-usage");
    let config = Config::default();
    let provider = Scripted::new(vec![answer("done")]);
    let mut runtime = fixture.runtime(provider, &config);
    let (before, window) = runtime.context_usage().unwrap();
    assert_eq!(window, config.context.window_tokens);
    runtime.turn("hello".to_string(), &mut |_| Ok(())).unwrap();
    let (after, _) = runtime.context_usage().unwrap();
    assert!(after > before, "{after} should exceed {before}");
}

#[test]
fn context_usage_events_track_each_addition_during_a_turn() {
    let fixture = Fixture::new("context-usage-events");
    let config = Config::default();
    let provider = Scripted::new(vec![
        tool("sh", r#"{"command":"printf forbidden"}"#),
        answer("done"),
    ]);
    let mut runtime = fixture.runtime(provider, &config);
    let mut reported = Vec::new();
    runtime
        .turn("work".into(), &mut |event| {
            if let Event::ContextUsage { used, max } = event {
                reported.push((used, max));
            }
            Ok(())
        })
        .unwrap();
    // The prompt, the assistant tool call, the tool result and the final answer
    // each grow the context, so a status line never goes stale mid-turn.
    assert_eq!(reported.len(), 4, "{reported:?}");
    assert!(
        reported.windows(2).all(|pair| pair[0].0 < pair[1].0),
        "{reported:?}"
    );
    assert!(
        reported
            .iter()
            .all(|(_, max)| *max == config.context.window_tokens)
    );
}

#[test]
fn runtime_events_pair_tool_ids_and_report_numeric_usage() {
    let fixture = Fixture::new("structured-events");
    let config = Config::default();
    let provider = Scripted::new(vec![
        tool("sh", r#"{"command":"printf forbidden"}"#),
        priced_answer("denied"),
    ]);
    let mut runtime = fixture.runtime(provider, &config);
    let mut events = Vec::new();
    runtime
        .turn("work".into(), &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();
    let started = events
        .iter()
        .find_map(|event| match event {
            Event::ToolStart { call_id, .. } => Some(call_id),
            _ => None,
        })
        .unwrap();
    assert_eq!(started, "call_1");
    let ended = events
        .iter()
        .find_map(|event| match event {
            Event::ToolEnd {
                call_id,
                outcome,
                is_error,
                ..
            } => Some((call_id, outcome, is_error)),
            _ => None,
        })
        .unwrap();
    assert_eq!(ended, (started, &ToolOutcome::Completed, &false));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::ToolOutput { text, .. } if text == "forbidden"))
    );
    let usage = events
        .iter()
        .filter_map(|event| match event {
            Event::Usage { request, total } => Some((request, total)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), 2);
    assert_eq!(usage[1].0.total(), 12);
    assert_eq!(usage[1].1.completions, 2);
    assert_eq!(usage[1].1.total_tokens(), 12);
}

#[test]
fn compaction_reports_its_usage_as_a_numeric_event() {
    let fixture = Fixture::new("compaction-usage-event");
    let config = Config {
        context: ContextConfig {
            window_tokens: 10_000,
            output_reserve_tokens: 2_000,
            recent_tail_tokens: 50,
        },
        ..Config::default()
    };
    let provider = Scripted::new(vec![
        priced_answer(&"a".repeat(300)),
        priced_answer(&"b".repeat(300)),
        priced_answer("summary"),
    ]);
    let mut runtime = fixture.runtime(provider, &config);
    for prompt in ["one", "two"] {
        runtime.turn(prompt.into(), &mut |_| Ok(())).unwrap();
    }
    let mut reported = Vec::new();
    runtime
        .compact(None, &mut |event| {
            if let Event::Usage { request, total } = event {
                reported.push((request, total));
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(reported.len(), 1);
    assert_eq!(reported[0].0.total(), 12);
    assert_eq!(reported[0].1.completions, 3);
    assert_eq!(reported[0].1.total_tokens(), 36);
}

#[test]
fn turn_end_metadata_counts_rounds_and_identifies_the_last_entry_on_all_exits() {
    for (case, responses, status, rounds, limit) in [
        ("chat", vec![answer("done")], "completed", 0, None),
        (
            "tools",
            vec![
                Ok(Completion {
                    tool_calls: vec![
                        ToolCall::new("one", "sh", r#"{"command":"printf one"}"#),
                        ToolCall::new("two", "sh", r#"{"command":"printf two"}"#),
                    ],
                    ..Completion::default()
                }),
                answer("done"),
            ],
            "completed",
            1,
            None,
        ),
        (
            "error",
            vec![Err(ProviderError::Protocol("broken".into()))],
            "error",
            0,
            None,
        ),
        (
            "cancel",
            vec![Err(ProviderError::Aborted("cancelled".into()))],
            "cancelled",
            0,
            None,
        ),
        (
            "limit",
            vec![tool("sh", r#"{"command":"printf limited"}"#)],
            "error",
            1,
            Some(1),
        ),
    ] {
        let fixture = Fixture::new(&format!("turn-end-{case}"));
        let config = Config {
            max_tool_rounds: limit.unwrap_or(Config::default().max_tool_rounds),
            ..Config::default()
        };
        let mut runtime = fixture.runtime(Scripted::new(responses), &config);
        // The round budget applies to unattended runs only.
        runtime.set_interactive(limit.is_none()).unwrap();
        let mut end = None;
        let result = runtime.turn("work".into(), &mut |event| {
            if let Event::TurnEnd {
                status,
                tool_rounds,
                last_entry_id,
            } = event
            {
                end = Some((status, tool_rounds, last_entry_id));
            }
            Ok(())
        });
        assert_eq!(result.is_ok(), status == "completed", "{case}: {result:?}");
        let id = runtime.session().active_leaf().unwrap();
        assert_eq!(end, Some((status, rounds, Some(id.to_string()))));
        assert!(runtime.session().entry(id).is_some());
    }
}

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("gears-runtime-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        Self { root, workspace }
    }

    fn runtime(&self, provider: Arc<dyn Provider>, config: &Config) -> Runtime {
        let store = Store::with_root(&self.workspace, self.root.join("state")).unwrap();
        let session = store.create(false, None).unwrap();
        Runtime::new(provider, store, session, config, "test/model".to_string()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn reset_removes_what_the_user_added_and_keeps_the_built_in_tools() {
    let fixture = Fixture::new("reset");
    let directory = fixture.root.join("global");
    let store = Store::with_root(&fixture.workspace, directory.clone()).unwrap();
    let session = store.create(false, None).unwrap();
    let old = session.path().unwrap().to_path_buf();
    assert!(old.starts_with(directory.join("sessions")));
    let mut runtime = Runtime::new(
        Scripted::new(vec![]),
        store,
        session,
        &Config::default(),
        "test/model".into(),
    )
    .unwrap();
    support::write(directory.join("gears.toml"), "version = 1\n").unwrap();
    support::write(directory.join("local.key"), "key").unwrap();
    std::fs::create_dir_all(fixture.workspace.join(".gears")).unwrap();
    support::write(fixture.workspace.join(".gears/MEMORY.md"), "fact").unwrap();
    runtime.reset().unwrap();
    assert!(directory.join("gears.toml").is_file());
    assert!(!directory.join("local.key").exists());
    assert!(!old.exists());
    assert!(!fixture.workspace.join(".gears").exists());
    assert!(runtime.session().path().unwrap().is_file());
    assert!(runtime.tools().get("sh").is_some());
}

#[test]
fn shipped_sh_tool_runs_from_its_directory() {
    let fixture = Fixture::new("shipped-sh");
    let config = Config::default();
    let provider = Scripted::new(vec![
        tool(
            "sh",
            r#"{"command":"test -d \"$GEARS_SH_STATE_DIR\" && printf made > result.txt"}"#,
        ),
        answer("done"),
    ]);
    let mut runtime = fixture.runtime(provider.clone(), &config);
    runtime
        .turn("make it".to_string(), &mut |_| Ok(()))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("result.txt")).unwrap(),
        "made"
    );
    assert!(
        runtime
            .session()
            .path()
            .unwrap()
            .with_extension("sh-state")
            .is_dir()
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .filter(|spec| spec.name == "sh")
            .count(),
        1
    );
}

#[test]
fn shipped_sh_restores_directory_and_exports_after_resume() {
    let fixture = Fixture::new("sh-state-resume");
    std::fs::create_dir(fixture.workspace.join("nested")).unwrap();
    let config = Config::default();
    let provider = Scripted::new(vec![
        tool(
            "sh",
            r#"{"command":"cd nested && export SH_TEST_VALUE='two words'"}"#,
        ),
        tool(
            "sh",
            r#"{"command":"printf '%s' \"$SH_TEST_VALUE\" > first.txt"}"#,
        ),
        answer("done"),
    ]);
    let mut runtime = fixture.runtime(provider, &config);
    runtime
        .turn("set state".to_string(), &mut |_| Ok(()))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("nested/first.txt")).unwrap(),
        "two words"
    );
    let path = runtime.session().path().unwrap().to_path_buf();
    let state = path.with_extension("sh-state");
    let exports = std::fs::read_to_string(state.join("env.sh")).unwrap();
    assert!(!exports.lines().any(|line| line.contains(" GEARS_")));
    assert!(!exports.lines().any(|line| line.contains(" PATH=")));
    drop(runtime);

    let store = Store::with_root(&fixture.workspace, fixture.root.join("state")).unwrap();
    let session = store.open_path(&path).unwrap();
    let provider = Scripted::new(vec![
        tool(
            "sh",
            r#"{"command":"printf '%s' \"$SH_TEST_VALUE\" > resumed.txt"}"#,
        ),
        answer("done"),
    ]);
    let mut resumed =
        Runtime::new(provider, store, session, &config, "test/model".to_string()).unwrap();
    resumed
        .turn("use state".to_string(), &mut |_| Ok(()))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("nested/resumed.txt")).unwrap(),
        "two words"
    );
}

#[test]
fn shipped_sh_uses_a_bounded_per_call_timeout() {
    let fixture = Fixture::new("sh-call-timeout");
    let config = Config::default();
    let provider = Scripted::new(vec![
        tool("sh", r#"{"command":"export SH_TEST_VALUE=kept"}"#),
        tool(
            "sh",
            r#"{"command":"printf bad > unstarted.txt","timeout":121}"#,
        ),
        tool(
            "sh",
            r#"{"command":"export SH_TEST_VALUE=lost; sleep 5","timeout":1}"#,
        ),
        tool(
            "sh",
            r#"{"command":"printf '%s' \"$SH_TEST_VALUE\" > state.txt"}"#,
        ),
        answer("done"),
    ]);
    let mut runtime = fixture.runtime(provider, &config);
    runtime.turn("run".to_string(), &mut |_| Ok(())).unwrap();
    assert!(!fixture.workspace.join("unstarted.txt").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("state.txt")).unwrap(),
        "kept"
    );
    let results = runtime
        .session()
        .context_messages()
        .unwrap()
        .into_iter()
        .flat_map(|message| message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        results
            .iter()
            .any(|result| result.contains("from 1 to 120 seconds"))
    );
    assert!(
        results
            .iter()
            .any(|result| result.contains("timed out after 1s"))
    );
}
#[test]
fn text_only_turn_streams_and_is_durable() {
    let fixture = Fixture::new("text");
    let provider = Scripted::new(vec![answer("hello")]);
    let mut runtime = fixture.runtime(provider, &Config::default());
    let mut events = Vec::new();
    runtime
        .turn("question".to_string(), &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Text(text) if text == "hello"))
    );
    let messages = runtime.session().context_messages().unwrap();
    assert_eq!(messages[0].text_content(), "question");
    assert_eq!(messages[1].text_content(), "hello");
}

#[test]
fn reasoning_only_reply_is_an_error_without_an_empty_assistant_message() {
    let fixture = Fixture::new("empty-reply");
    let provider = Scripted::new(vec![Ok(Completion {
        reasoning: "thinking".to_string(),
        finish_reason: Some(FinishReason::Stop),
        ..Completion::default()
    })]);
    let mut runtime = fixture.runtime(provider, &Config::default());
    let error = runtime
        .turn("question".to_string(), &mut |_| Ok(()))
        .unwrap_err();
    assert!(error.contains("empty reply"), "{error}");
    let messages = runtime.session().context_messages().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text_content(), "question");
}

#[test]
fn zero_tool_round_budget_reports_an_error_instead_of_panicking() {
    let fixture = Fixture::new("zero-rounds");
    let config = Config {
        max_tool_rounds: 0,
        ..Config::default()
    };
    let provider = Scripted::new(Vec::new());
    let mut runtime = fixture.runtime(provider.clone(), &config);
    let error = runtime
        .turn("question".to_string(), &mut |_| Ok(()))
        .unwrap_err();
    assert!(error.contains("tool round limit (0) reached"), "{error}");
    assert!(provider.requests.lock().unwrap().is_empty());
    assert!(
        runtime
            .session()
            .entries()
            .iter()
            .any(|entry| entry.kind == "turn_error")
    );
}

#[test]
fn tool_round_limit_stops_after_the_last_result() {
    let fixture = Fixture::new("round-limit");
    let config = Config {
        max_tool_rounds: 1,
        ..Config::default()
    };
    let provider = Scripted::new(vec![tool("sh", r#"{"command":": > no-file"}"#)]);
    let mut runtime = fixture.runtime(provider.clone(), &config);
    let error = runtime
        .turn("question".to_string(), &mut |_| Ok(()))
        .unwrap_err();
    assert!(error.contains("tool round limit (1) reached"), "{error}");
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let messages = runtime.session().context_messages().unwrap();
    assert_eq!(messages.len(), 4);
    assert!(fixture.workspace.join("no-file").is_file());
    assert!(messages[3].text_content().contains("ended early"));
}

#[test]
fn an_interactive_turn_runs_past_the_round_budget() {
    let fixture = Fixture::new("attended-rounds");
    let config = Config {
        max_tool_rounds: 1,
        ..Config::default()
    };
    let provider = Scripted::new(vec![
        tool("sh", r#"{"command":": > no-file"}"#),
        tool("sh", r#"{"command":": > no-file"}"#),
        answer("done"),
    ]);
    let mut runtime = fixture.runtime(provider.clone(), &config);
    runtime.set_interactive(true).unwrap();
    let mut rounds = None;
    runtime
        .turn("question".to_string(), &mut |event| {
            if let Event::TurnEnd { tool_rounds, .. } = event {
                rounds = Some(tool_rounds);
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 3);
    assert_eq!(rounds, Some(2));
}
#[test]
fn an_unattended_sh_call_runs_in_the_workspace() {
    let fixture = Fixture::new("sh");
    let provider = Scripted::new(vec![
        tool("sh", r#"{"command":"printf made > result.txt"}"#),
        answer("done"),
    ]);
    let mut runtime = fixture.runtime(provider, &Config::default());
    runtime
        .turn("make it".to_string(), &mut |_| Ok(()))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("result.txt")).unwrap(),
        "made"
    );
}

#[test]
fn manual_compaction_appends_a_checkpoint_without_deleting_history() {
    let fixture = Fixture::new("compact");
    let provider = Scripted::new(vec![
        answer(&"a".repeat(300)),
        answer(&"b".repeat(300)),
        answer(&"c".repeat(300)),
        answer("summary of prior work"),
    ]);
    let config = Config {
        context: ContextConfig {
            window_tokens: 10_000,
            output_reserve_tokens: 2_000,
            recent_tail_tokens: 50,
        },
        ..Config::default()
    };
    let mut runtime = fixture.runtime(provider.clone(), &config);
    for prompt in ["one", "two", "three"] {
        runtime.turn(prompt.to_string(), &mut |_| Ok(())).unwrap();
    }
    let entries_before = runtime.session().entries().len();
    runtime.compact(None, &mut |_| Ok(())).unwrap();
    assert!(runtime.session().entries().len() > entries_before);
    assert!(
        runtime.session().context_messages().unwrap()[0]
            .text_content()
            .contains("summary of prior work")
    );
    let requests = provider.requests.lock().unwrap();
    assert!(requests.last().unwrap().tools.is_empty());
}

#[test]
fn compaction_bounds_a_large_tool_result_sent_to_the_summarizer() {
    let fixture = Fixture::new("large-compaction-input");
    let store = Store::with_root(&fixture.workspace, fixture.root.join("state")).unwrap();
    let mut session = store.create(false, None).unwrap();
    session.append_message(&Message::user("start")).unwrap();
    session
        .append_message(&Message::assistant_response(
            String::new(),
            vec![ToolCall::new("one", "sh", "{}")],
        ))
        .unwrap();
    session
        .append_message(&Message::tool_result(
            "one",
            "sh",
            "large output ".repeat(20_000),
            false,
        ))
        .unwrap();
    session.append_message(&Message::assistant("done")).unwrap();
    session.append_message(&Message::user("continue")).unwrap();
    let provider = Scripted::new(vec![answer("summary")]);
    let config = Config {
        context: ContextConfig {
            window_tokens: 2000,
            output_reserve_tokens: 200,
            recent_tail_tokens: 50,
        },
        ..Config::default()
    };
    let mut runtime = Runtime::new(
        provider.clone(),
        store,
        session,
        &config,
        "test/model".to_string(),
    )
    .unwrap();
    runtime.compact(None, &mut |_| Ok(())).unwrap();
    let requests = provider.requests.lock().unwrap();
    let request = requests.last().unwrap();
    let bytes = serde_json::to_vec(&(&request.system, &request.messages)).unwrap();
    assert!(bytes.len() < 2000 * 4, "summarizer input is too large");
    assert!(request.messages[0].text_content().contains("[truncated]"));
}

#[test]
fn provider_failures_are_recorded() {
    let fixture = Fixture::new("error");
    let provider = Scripted::new(vec![Err(ProviderError::Unavailable(
        "scripted".to_string(),
    ))]);
    let mut runtime = fixture.runtime(provider, &Config::default());
    assert!(
        runtime
            .turn("question".to_string(), &mut |_| Ok(()),)
            .is_err()
    );
    assert!(
        runtime
            .session()
            .entries()
            .iter()
            .any(|entry| entry.kind == "turn_error")
    );
}

#[test]
fn an_oversized_first_turn_records_its_error() {
    let fixture = Fixture::new("oversized-first-turn");
    let config = Config {
        context: ContextConfig {
            window_tokens: 2000,
            output_reserve_tokens: 200,
            recent_tail_tokens: 50,
        },
        ..Config::default()
    };
    let mut runtime = fixture.runtime(Scripted::new(Vec::new()), &config);
    let error = runtime
        .turn("large input ".repeat(10_000), &mut |_| Ok(()))
        .unwrap_err();
    assert!(error.contains("limit"), "{error}");
    assert!(runtime.session().entries().iter().any(|entry| {
        entry.kind == "turn_error" && entry.data["detail"].as_str() == Some(error.as_str())
    }));
}

#[test]
fn resume_restores_context_model_and_cumulative_usage() {
    let fixture = Fixture::new("resume");
    let store = Store::with_root(&fixture.workspace, fixture.root.join("state")).unwrap();
    let session = store.create(false, None).unwrap();
    let first = Scripted::new(vec![priced_answer("first answer")]);
    let mut runtime = Runtime::new(
        first,
        store.clone(),
        session,
        &Config::default(),
        "test/one".to_string(),
    )
    .unwrap();
    runtime
        .turn("first question".to_string(), &mut |_| Ok(()))
        .unwrap();
    let id = runtime.summary().id;
    drop(runtime);

    let session = store.open(&id).unwrap();
    let resumed_model = session.model().unwrap();
    let second = Scripted::new(vec![answer("second answer")]);
    let mut resumed = Runtime::new(
        second.clone(),
        store,
        session,
        &Config::default(),
        resumed_model,
    )
    .unwrap();
    assert_eq!(resumed.model(), "test/one");
    assert_eq!(resumed.usage().total_tokens(), 12);
    resumed
        .turn("follow up".to_string(), &mut |_| Ok(()))
        .unwrap();
    let requests = second.requests.lock().unwrap();
    assert!(
        requests[0]
            .messages
            .iter()
            .any(|message| message.text_content() == "first answer")
    );
}

#[test]
fn resume_reports_discarded_crash_tail_and_completes_a_new_turn() {
    let fixture = Fixture::new("crash-recovery");
    let store = Store::with_root(&fixture.workspace, fixture.root.join("state")).unwrap();
    let mut session = store.create(false, None).unwrap();
    session.append_message(&Message::user("before")).unwrap();
    session
        .append_message(&Message::assistant_response(
            String::new(),
            vec![
                ToolCall::new("one", "sh", "{}"),
                ToolCall::new("two", "sh", "{}"),
            ],
        ))
        .unwrap();
    session
        .append_message(&Message::tool_result("one", "sh", "done", false))
        .unwrap();
    let id = session.id().to_string();
    drop(session);

    let mut runtime = Runtime::new(
        Scripted::new(vec![answer("recovered")]),
        store.clone(),
        store.open(&id).unwrap(),
        &Config::default(),
        "test/model".to_string(),
    )
    .unwrap();
    assert!(
        runtime
            .take_startup_notices()
            .iter()
            .any(|notice| notice.contains("discarded 2 incomplete session records"))
    );
    runtime.turn("next".to_string(), &mut |_| Ok(())).unwrap();
    assert_eq!(runtime.session().context_messages().unwrap().len(), 3);
}

#[test]
fn a_provider_switch_is_recorded_and_used_on_the_next_turn() {
    let fixture = Fixture::new("provider-switch");
    let first = Scripted::new(vec![answer("first answer")]);
    let second = Scripted::new(vec![answer("second answer")]);
    let mut runtime = fixture.runtime(first.clone(), &Config::default());
    let next = second.clone();
    runtime.install_provider_factory(Arc::new(move |name| {
        if name == "second" {
            Ok(next.clone() as Arc<dyn Provider>)
        } else {
            Err(format!("unknown provider {name}"))
        }
    }));
    runtime.turn("one".to_string(), &mut |_| Ok(())).unwrap();
    runtime.set_provider("second", &mut |_| Ok(())).unwrap();
    assert_eq!(runtime.provider_name(), "second");
    assert_eq!(runtime.session().provider().as_deref(), Some("second"));
    runtime.turn("two".to_string(), &mut |_| Ok(())).unwrap();
    assert_eq!(first.requests.lock().unwrap().len(), 1);
    assert_eq!(second.requests.lock().unwrap().len(), 1);
    assert!(
        runtime
            .session()
            .context_messages()
            .unwrap()
            .iter()
            .any(|message| message.text_content() == "second answer")
    );
}

#[test]
fn switched_provider_sees_reasoning_cache_markers_and_retry_notice() {
    let fixture = Fixture::new("provider-phase-two");
    let first = MockServer::start_one(sse_response(&[
        r#"{"choices":[{"index":0,"delta":{"reasoning":"considered"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"first answer"},"finish_reason":"stop"}]}"#,
    ]))
    .unwrap();
    let rate_limit = Script::new()
        .write("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\n\r\n");
    let second = MockServer::start(vec![
        rate_limit,
        sse_response(&[r#"{"choices":[{"index":0,"delta":{"content":"second answer"},"finish_reason":"stop"}]}"#]),
    ])
    .unwrap();
    let make_provider = |url: &str, name: &str| -> Arc<dyn Provider> {
        let policy = EgressPolicy::new(&["127.0.0.1".into()])
            .with_plain_http_allowlist(&["127.0.0.1".into()]);
        let endpoint = Endpoint::new(url)
            .unwrap()
            .with_quirks(Quirks::for_host("openrouter.ai"));
        Arc::new(
            OpenAiCompat::new(MotorCurl::new(policy).unwrap(), endpoint)
                .without_key()
                .with_cache_identity(name),
        )
    };
    let mut runtime = fixture.runtime(
        make_provider(&first.url("/v1"), "default"),
        &Config::default(),
    );
    let second_url = second.url("/v1");
    runtime.install_provider_factory(Arc::new(move |name| {
        if name == "second" {
            Ok(make_provider(&second_url, name))
        } else {
            Err(format!("unknown provider {name}"))
        }
    }));
    runtime.turn("one".into(), &mut |_| Ok(())).unwrap();
    runtime.set_provider("second", &mut |_| Ok(())).unwrap();
    let mut events = Vec::new();
    runtime
        .turn("two".into(), &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Notice(text) if text.contains("retrying")))
    );
    assert_eq!(second.requests().len(), 2);
    let request: serde_json::Value = serde_json::from_slice(&second.requests()[1].body).unwrap();
    assert_eq!(
        request["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(request["messages"][2]["reasoning"], "considered");
    assert_eq!(
        request["messages"][2]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(
        request["messages"][3]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert!(
        request["prompt_cache_key"]
            .as_str()
            .unwrap()
            .starts_with("gears-")
    );
    assert_eq!(first.requests().len(), 1);
    let first_request: serde_json::Value =
        serde_json::from_slice(&first.requests()[0].body).unwrap();
    assert_ne!(
        request["prompt_cache_key"],
        first_request["prompt_cache_key"]
    );
}

#[test]
fn resuming_another_session_restores_its_provider() {
    let fixture = Fixture::new("provider-session-switch");
    let store = Store::with_root(&fixture.workspace, fixture.root.join("state")).unwrap();
    let first_session = store.create(false, None).unwrap();
    let mut second_session = store.create(false, None).unwrap();
    second_session.set_provider("second").unwrap();
    let second_id = second_session.id().to_string();
    drop(second_session);

    let first = Scripted::new(Vec::new());
    let second = Scripted::new(vec![answer("from second")]);
    let mut runtime = Runtime::new(
        first.clone(),
        store,
        first_session,
        &Config::default(),
        "test/model".to_string(),
    )
    .unwrap();
    let next = second.clone();
    runtime.install_provider_factory(Arc::new(move |name| {
        if name == "second" {
            Ok(next.clone() as Arc<dyn Provider>)
        } else {
            Err(format!("unknown provider {name}"))
        }
    }));
    runtime.resume(&second_id).unwrap();
    assert_eq!(runtime.provider_name(), "second");
    runtime.turn("hello".to_string(), &mut |_| Ok(())).unwrap();
    assert!(first.requests.lock().unwrap().is_empty());
    assert_eq!(second.requests.lock().unwrap().len(), 1);
}

struct UntilCancelled;

impl Provider for UntilCancelled {
    fn complete(
        &self,
        _request: &Request,
        sink: &mut dyn EventSink,
    ) -> Result<Completion, ProviderError> {
        loop {
            sink.on_event(StreamEvent::Text(".".to_string()))
                .map_err(|error| ProviderError::Aborted(error.to_string()))?;
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[test]
fn cancellation_stops_a_streaming_turn_and_records_it() {
    let fixture = Fixture::new("cancel");
    let mut runtime = fixture.runtime(Arc::new(UntilCancelled), &Config::default());
    let cancellation = runtime.cancellation();
    let mut saw_text = false;
    let error = runtime
        .turn("keep going".to_string(), &mut |event| {
            if matches!(event, Event::Text(_)) {
                saw_text = true;
                cancellation.cancel();
            }
            Ok(())
        })
        .unwrap_err();
    assert!(saw_text);
    assert!(error.contains("cancelled"));
    assert!(
        runtime
            .session()
            .entries()
            .iter()
            .any(|entry| { entry.kind == "turn_error" && entry.data["cancelled"] == true })
    );
}

#[test]
fn cancellation_pairs_every_call_in_a_tool_round() {
    let fixture = Fixture::new("cancel-round");
    let provider = Scripted::new(vec![Ok(Completion {
        tool_calls: vec![
            ToolCall::new("call_1", "sh", r#"{"command":"printf started; sleep 2"}"#),
            ToolCall::new("call_2", "sh", r#"{"command":": > never"}"#),
        ],
        finish_reason: Some(FinishReason::ToolCalls),
        ..Completion::default()
    })]);
    let mut runtime = fixture.runtime(provider, &Config::default());
    let cancellation = runtime.cancellation();
    let mut saw_output = false;
    let error = runtime
        .turn("go".to_string(), &mut |event| {
            if matches!(event, Event::ToolOutput { .. }) {
                saw_output = true;
                cancellation.cancel();
            }
            Ok(())
        })
        .unwrap_err();
    assert!(saw_output);
    assert!(error.contains("cancelled"), "{error}");
    assert!(!fixture.workspace.join("never").exists());
    let outcomes = runtime
        .session()
        .context_messages()
        .unwrap()
        .into_iter()
        .flat_map(|message| message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                call_id, outcome, ..
            } => Some((call_id, outcome)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes,
        [
            ("call_1".to_string(), ToolOutcome::InterruptedUnknown),
            ("call_2".to_string(), ToolOutcome::NotStarted),
        ]
    );
}

#[test]
fn an_observer_error_pairs_calls_that_never_started() {
    let fixture = Fixture::new("observer-before-tools");
    let provider = Scripted::new(vec![Ok(Completion {
        tool_calls: vec![
            ToolCall::new("call_1", "sh", r#"{"command":": > never-one"}"#),
            ToolCall::new("call_2", "sh", r#"{"command":": > never-two"}"#),
        ],
        finish_reason: Some(FinishReason::ToolCalls),
        ..Completion::default()
    })]);
    let mut runtime = fixture.runtime(provider, &Config::default());
    let error = runtime
        .turn("go".to_string(), &mut |event| {
            if matches!(event, Event::ToolStart { .. }) {
                Err("observer refused tool start event".to_string())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
    assert!(error.contains("observer refused"), "{error}");
    for name in ["never-one", "never-two"] {
        assert!(!fixture.workspace.join(name).exists());
    }
    let outcomes = runtime
        .session()
        .context_messages()
        .unwrap()
        .into_iter()
        .flat_map(|message| message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(outcomes, [ToolOutcome::NotStarted, ToolOutcome::NotStarted]);
    assert!(
        runtime
            .session()
            .entries()
            .iter()
            .any(|entry| entry.kind == "turn_error")
    );
}

#[test]
fn automatic_compaction_runs_before_an_oversized_follow_up() {
    let fixture = Fixture::new("automatic-compact");
    let provider = Scripted::new(vec![
        answer(&"large ".repeat(1_000)),
        answer("summary"),
        answer("after compaction"),
    ]);
    let config = Config {
        context: ContextConfig {
            window_tokens: 2000,
            output_reserve_tokens: 200,
            recent_tail_tokens: 50,
        },
        ..Config::default()
    };
    let mut runtime = fixture.runtime(provider.clone(), &config);
    runtime.turn("first".to_string(), &mut |_| Ok(())).unwrap();
    runtime
        .turn("follow up".to_string(), &mut |_| Ok(()))
        .unwrap();
    assert!(
        runtime
            .session()
            .entries()
            .iter()
            .any(|entry| entry.kind == "compaction")
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 3);
}

#[test]
fn file_tools_protect_keys_and_settings_beside_the_session_store() {
    let fixture = Fixture::new("gears-directory-boundary");
    let directory = fixture.root.join("state");
    std::fs::create_dir_all(&directory).unwrap();
    for name in ["local.key", "gears.toml"] {
        let path = directory.join(name);
        support::write(&path, "private contents").unwrap();
        for (name, arguments) in [
            ("read_file", serde_json::json!({"path": path})),
            (
                "write_file",
                serde_json::json!({"path": path, "content": "bad"}),
            ),
            (
                "patch",
                serde_json::json!({"path": path, "old_text": "private", "new_text": "bad"}),
            ),
        ] {
            let provider = Scripted::new(vec![tool(name, &arguments.to_string()), answer("done")]);
            let mut runtime = fixture.runtime(provider.clone(), &Config::default());
            runtime
                .turn("try the file tool".into(), &mut |_| Ok(()))
                .unwrap();
            let requests = provider.requests.lock().unwrap();
            assert!(
                requests[1]
                    .messages
                    .iter()
                    .any(|message| { message.text_content().contains("Gears directory") }),
                "{name}: {:?}",
                requests[1].messages
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "private contents");
        }
    }
}

#[test]
fn workspace_instructions_reach_every_request_including_after_compaction() {
    let fixture = Fixture::new("instructions");
    support::write(
        fixture.workspace.join("AGENTS.md"),
        "Build with lorry. Run `uname` to see the OS.\n",
    )
    .unwrap();
    let provider = Scripted::new(vec![
        answer(&"a".repeat(300)),
        answer(&"b".repeat(300)),
        answer("summary of prior work"),
        answer("after compaction"),
    ]);
    let config = Config {
        context: ContextConfig {
            window_tokens: 10_000,
            output_reserve_tokens: 2_000,
            recent_tail_tokens: 50,
        },
        ..Config::default()
    };
    let mut runtime = fixture.runtime(provider.clone(), &config);
    runtime.turn("one".to_string(), &mut |_| Ok(())).unwrap();
    runtime.turn("two".to_string(), &mut |_| Ok(())).unwrap();
    // An edit made while the session runs reaches the request after compaction.
    support::write(
        fixture.workspace.join("AGENTS.md"),
        "Build with lorry and stay offline.\n",
    )
    .unwrap();
    runtime.compact(None, &mut |_| Ok(())).unwrap();
    runtime.turn("three".to_string(), &mut |_| Ok(())).unwrap();

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for request in requests.iter() {
        assert!(
            request
                .system
                .iter()
                .any(|fragment| fragment.contains("Build with lorry")),
            "workspace instructions are missing from {:#?}",
            request.system
        );
    }
    let edited = |request: &Request| {
        request
            .system
            .iter()
            .any(|fragment| fragment.contains("stay offline"))
    };
    assert!(!edited(&requests[0]) && !edited(&requests[1]));
    assert!(edited(&requests[2]), "the compaction request sees the edit");
    assert!(
        edited(&requests[3]),
        "the turn after compaction sees the edit"
    );
}
