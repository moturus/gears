//! One provider loop with sessions, authorization, and compaction.

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{Config, ContextConfig};
use crate::process::{Cancellation, Stream};
use crate::prompt::Effective;
use crate::provider::{
    ContentBlock, EventSink, Message, Provider, ProviderError, Request, StreamEvent, ToolCall,
    ToolOutcome, ToolSpec, Usage, UsageMeter,
};
use crate::session::{Compaction, Session, SessionInfo, Store, TreeItem};
use crate::tools::{Context, Registry};

/// What a tool call returned to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    Text(String),
    Reasoning(String),
    ToolStart {
        call_id: String,
        name: String,
        detail: String,
    },
    ToolOutput {
        name: String,
        stream: Stream,
        text: String,
    },
    ToolEnd {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
        outcome: ToolOutcome,
    },
    Notice(String),
    Usage {
        request: Usage,
        total: UsageMeter,
    },
    ModelChanged(String),
    SessionChanged(SessionSummary),
    TurnEnd {
        status: &'static str,
        tool_rounds: usize,
        last_entry_id: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub path: Option<PathBuf>,
    pub name: Option<String>,
    pub entries: usize,
    pub ephemeral: bool,
}

pub trait Observer {
    fn event(&mut self, event: Event) -> Result<(), String>;
}

impl<F> Observer for F
where
    F: FnMut(Event) -> Result<(), String>,
{
    fn event(&mut self, event: Event) -> Result<(), String> {
        self(event)
    }
}

pub type ProviderFactory = Arc<dyn Fn(&str) -> Result<Arc<dyn Provider>, String> + Send + Sync>;

pub struct Runtime {
    provider: Arc<dyn Provider>,
    provider_name: String,
    tools: Registry,
    provider_factory: Option<ProviderFactory>,
    store: Store,
    session: Session,
    prompt: Effective,
    model: String,
    max_tool_rounds: usize,
    context: ContextConfig,
    cancellation: Cancellation,
    usage: UsageMeter,
    startup_notices: Vec<String>,
    interactive: bool,
}

impl Runtime {
    pub fn new(
        provider: Arc<dyn Provider>,
        store: Store,
        mut session: Session,
        config: &Config,
        model: String,
    ) -> Result<Self, String> {
        let tools = Registry::new();
        crate::process::ensure_cleanup()?;
        let cancellation = Cancellation::new();
        let previous_identity = session.latest_runtime_identity();
        let mut startup_notices = Vec::new();
        if session.recovered_entries() > 0 {
            startup_notices.push(format!(
                "discarded {} incomplete session records after a crash",
                session.recovered_entries()
            ));
        }
        let provider_name = session.provider().unwrap_or_else(|| "default".to_string());
        let prompt = effective_prompt(store.workspace(), &tools)?;
        let identity = prompt.identity();
        if previous_identity
            .as_ref()
            .is_some_and(|previous| previous != &identity)
        {
            startup_notices.push(
                "prompt or tool resources changed since this session was last opened".to_string(),
            );
        }
        session.append_runtime_identity(&identity)?;
        if session.model().as_deref() != Some(model.as_str()) {
            session.set_model(&model)?;
        }
        let usage = session.usage();
        Ok(Self {
            provider,
            provider_name,
            provider_factory: None,
            store,
            session,
            tools,
            prompt,
            model,
            max_tool_rounds: config.max_tool_rounds,
            context: config.context,
            cancellation,
            usage,
            startup_notices,
            interactive: false,
        })
    }

    pub fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    pub fn tools(&self) -> &Registry {
        &self.tools
    }

    pub fn take_startup_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.startup_notices)
    }

    /// Interfaces opt in explicitly; library and one-shot runs are unattended.
    pub fn set_interactive(&mut self, interactive: bool) -> Result<(), String> {
        self.prepare_write()?;
        self.interactive = interactive;
        Ok(())
    }

    fn prepare_write(&mut self) -> Result<(), String> {
        crate::process::ensure_cleanup()
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    pub fn install_provider_factory(&mut self, factory: ProviderFactory) {
        self.provider_factory = Some(factory);
    }

    pub fn usage(&self) -> &UsageMeter {
        &self.usage
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.session.id().to_string(),
            path: self.session.path().map(PathBuf::from),
            name: self.session.name(),
            entries: self.session.entries().len(),
            ephemeral: self.session.is_ephemeral(),
        }
    }

    pub fn turn(&mut self, input: String, observer: &mut dyn Observer) -> Result<(), String> {
        self.prepare_write()?;
        self.reload_prompt()?;
        emit_notices(observer, self.take_startup_notices())?;
        self.cancellation.reset();
        if input.trim().is_empty() {
            return Err("prompt must not be empty".to_string());
        }
        self.session.append_message(&Message::user(input.clone()))?;

        let entries_before = self.session.entries().len();
        let mut tool_rounds = 0;
        let result = self.run_turn(observer, &mut tool_rounds).or_else(|error| {
            let recorded = self.session.entries()[entries_before..]
                .iter()
                .any(|entry| entry.kind == "turn_error");
            if recorded {
                Err(error)
            } else {
                self.fail_turn(error, self.cancellation.cancelled(), observer)
            }
        });
        let status = match &result {
            Ok(()) => "completed",
            Err(_) if self.turn_was_cancelled(entries_before) => "cancelled",
            Err(_) => "error",
        };
        let last_entry_id = self.session.active_leaf().map(str::to_string);
        let turn_end = observer.event(Event::TurnEnd {
            status,
            tool_rounds,
            last_entry_id,
        });
        result.and(turn_end)
    }

    /// Whether the turn recorded since `entries_before` ended in cancellation.
    fn turn_was_cancelled(&self, entries_before: usize) -> bool {
        self.session.entries()[entries_before..]
            .iter()
            .rev()
            .find(|entry| entry.kind == "turn_error")
            .and_then(|entry| entry.data.get("cancelled"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or_else(|| self.cancellation.cancelled())
    }

    fn run_turn(
        &mut self,
        observer: &mut dyn Observer,
        tool_rounds: &mut usize,
    ) -> Result<(), String> {
        // An attended turn runs until the model stops or the user cancels it;
        // the round budget protects runs nobody is watching.
        let budget = (!self.interactive).then_some(self.max_tool_rounds);
        loop {
            if budget.is_some_and(|budget| *tool_rounds >= budget) {
                let error = format!("tool round limit ({}) reached", self.max_tool_rounds);
                return self.fail_turn(error, false, observer);
            }
            self.maybe_compact(false, None, observer)?;
            let messages = self.session.context_messages()?;
            self.ensure_fits(&messages)?;
            let request = self.request(messages, true);
            let completion = {
                let mut sink = RuntimeSink {
                    cancellation: &self.cancellation,
                    observer,
                };
                self.provider.complete(&request, &mut sink)
            };
            let completion = match completion {
                Ok(completion) => completion,
                Err(error) => {
                    let cancelled =
                        matches!(error, ProviderError::Aborted(_)) || self.cancellation.cancelled();
                    return self.fail_turn(error.to_string(), cancelled, observer);
                }
            };
            self.usage.add(&completion.usage);
            if completion.tool_calls.is_empty() && completion.content.trim().is_empty() {
                self.session.append_usage(&completion.usage)?;
                observer.event(Event::Usage {
                    request: completion.usage,
                    total: self.usage,
                })?;
                return self.fail_turn(
                    "model returned an empty reply".to_string(),
                    false,
                    observer,
                );
            }
            let assistant = completion.message();
            self.session.append_message(&assistant)?;
            self.session.append_usage(&completion.usage)?;
            observer.event(Event::Usage {
                request: completion.usage,
                total: self.usage,
            })?;
            if completion.tool_calls.is_empty() {
                return Ok(());
            }
            *tool_rounds += 1;
            let calls = completion.tool_calls;
            for (index, call) in calls.iter().cloned().enumerate() {
                if let Err(error) = self.execute_call(call, observer) {
                    self.record_unstarted_calls(
                        &calls[index + 1..],
                        "an earlier call ended the round",
                        observer,
                    )?;
                    return self.fail_turn(error, self.cancellation.cancelled(), observer);
                }
                if self.cancellation.cancelled() {
                    self.record_unstarted_calls(&calls[index + 1..], "turn cancelled", observer)?;
                    return self.fail_turn("turn cancelled".to_string(), true, observer);
                }
            }
        }
    }

    fn fail_turn(
        &mut self,
        error: String,
        cancelled: bool,
        observer: &mut dyn Observer,
    ) -> Result<(), String> {
        let cancelled = cancelled || self.cancellation.cancelled();
        self.session.append_error(&error, cancelled)?;
        let _ = observer;
        Err(error)
    }

    pub fn compact(
        &mut self,
        focus: Option<String>,
        observer: &mut dyn Observer,
    ) -> Result<(), String> {
        self.prepare_write()?;
        self.reload_prompt()?;
        self.cancellation.reset();
        if self.maybe_compact(true, focus, observer)? {
            Ok(())
        } else {
            Err("there is not enough history to compact".to_string())
        }
    }

    pub fn set_model(&mut self, model: String, observer: &mut dyn Observer) -> Result<(), String> {
        self.prepare_write()?;
        if model.trim().is_empty() {
            return Err("model id must not be empty".to_string());
        }
        self.cancellation.reset();
        let prompt = self.refresh_prompt()?;
        emit_notices(observer, self.take_startup_notices())?;
        self.session.set_model(&model)?;
        self.session.append_runtime_identity(&prompt.identity())?;
        self.model = model.clone();
        self.prompt = prompt;
        observer.event(Event::ModelChanged(model))
    }

    pub fn set_provider(&mut self, name: &str, observer: &mut dyn Observer) -> Result<(), String> {
        self.prepare_write()?;
        if name == self.provider_name {
            return Ok(());
        }
        let factory = self
            .provider_factory
            .as_ref()
            .ok_or("provider switching is not configured")?;
        let provider = factory(name)?;
        self.cancellation.reset();
        let prompt = self.refresh_prompt()?;
        emit_notices(observer, self.take_startup_notices())?;
        self.session.set_provider(name)?;
        self.session.append_runtime_identity(&prompt.identity())?;
        self.provider = provider;
        self.provider_name = name.to_string();
        self.prompt = prompt;
        observer.event(Event::Notice(format!("provider: {name}")))
    }

    /// Use what `/setup` chose. The same provider is reconnected too, since
    /// its address or key may have changed.
    pub fn apply_setup(
        &mut self,
        provider: &str,
        model: String,
        observer: &mut dyn Observer,
    ) -> Result<(), String> {
        self.prepare_write()?;
        if provider == self.provider_name {
            let factory = self
                .provider_factory
                .as_ref()
                .ok_or("provider switching is not configured")?;
            self.provider = factory(provider)?;
        } else {
            self.set_provider(provider, observer)?;
        }
        self.set_model(model, observer)
    }

    pub fn save_default_selection(&mut self) -> Result<(), String> {
        self.prepare_write()?;
        crate::provider_choice::ProviderChoice::save_selection(&self.provider_name, &self.model)
    }

    pub fn set_name(&mut self, name: &str) -> Result<(), String> {
        self.prepare_write()?;
        self.session.set_name(name)?;
        Ok(())
    }

    pub fn set_label(&mut self, entry_id: &str, label: Option<&str>) -> Result<(), String> {
        self.prepare_write()?;
        self.session.set_label(entry_id, label)?;
        Ok(())
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, String> {
        self.store.list()
    }

    pub fn tree(&self) -> Vec<TreeItem> {
        self.session.tree()
    }

    pub fn select_entry(&mut self, id: &str) -> Result<Option<String>, String> {
        self.prepare_write()?;
        let draft = self.session.select(id)?;

        Ok(draft)
    }

    pub fn new_session(&mut self, ephemeral: bool, name: Option<&str>) -> Result<(), String> {
        self.prepare_write()?;
        let session = self.store.create(ephemeral, name)?;
        self.install_session(session, "new")
    }

    /// Removes what the user and Gears added (see `crate::reset`) and continues
    /// in a new session. The provider, model and key of this run stay loaded.
    pub fn reset(&mut self) -> Result<usize, String> {
        // Leave the saved session first, so nothing writes into removed files.
        self.new_session(true, None)?;
        let removed = crate::reset::reset(self.store.directory(), self.store.workspace())?;
        self.new_session(false, None)?;
        Ok(removed)
    }

    pub fn resume(&mut self, selector: &str) -> Result<(), String> {
        self.prepare_write()?;
        self.cancellation.reset();
        if self.session.id() == selector {
            return Ok(());
        }
        let session = self.store.open(selector)?;
        self.install_session(session, "resume")
    }

    pub fn clone_session(&mut self) -> Result<(), String> {
        self.prepare_write()?;
        let session = self.store.clone_active(&self.session)?;
        self.install_session(session, "clone")
    }

    pub fn fork_at(&mut self, entry_id: &str) -> Result<String, String> {
        self.prepare_write()?;
        let (session, draft) = self.store.fork_at_user(&self.session, entry_id)?;
        self.install_session(session, "fork")?;
        Ok(draft)
    }

    pub fn close(&mut self) -> Vec<String> {
        if let Err(error) = self.prepare_write() {
            self.startup_notices.push(error);
            return std::mem::take(&mut self.startup_notices);
        }
        let notices = self.take_startup_notices();
        self.cancellation.reset();
        notices
    }

    /// Re-read the prompt resources. The workspace instructions are read from
    /// disk here, so a request never carries an AGENTS.md that was edited since
    /// the session was opened, and a compaction cannot drop it.
    fn reload_prompt(&mut self) -> Result<(), String> {
        self.prompt = effective_prompt(self.store.workspace(), &self.tools)?;
        Ok(())
    }

    fn refresh_prompt(&mut self) -> Result<Effective, String> {
        let prompt = effective_prompt(self.store.workspace(), &self.tools)?;
        if self
            .session
            .latest_runtime_identity()
            .is_none_or(|previous| previous != prompt.identity())
        {
            self.startup_notices.push(
                "prompt or tool resources changed since this session was last opened".to_string(),
            );
        }
        Ok(prompt)
    }

    fn install_session(&mut self, mut session: Session, _reason: &str) -> Result<(), String> {
        self.cancellation.reset();
        let provider_name = session
            .provider()
            .unwrap_or_else(|| self.provider_name.clone());
        let provider = if provider_name != self.provider_name {
            let factory = self
                .provider_factory
                .as_ref()
                .ok_or("provider switching is not configured")?;
            Some(factory(&provider_name)?)
        } else {
            None
        };
        if session.provider().is_none() {
            session.set_provider(&provider_name)?;
        }
        if session.recovered_entries() > 0 {
            self.startup_notices.push(format!(
                "discarded {} incomplete session records after a crash",
                session.recovered_entries()
            ));
        }
        let model = session.model().unwrap_or_else(|| self.model.clone());
        if session.model().is_none() {
            session.set_model(&model)?;
        }
        let prompt = effective_prompt(self.store.workspace(), &self.tools)?;
        let identity = prompt.identity();
        if session
            .latest_runtime_identity()
            .as_ref()
            .is_some_and(|previous| previous != &identity)
        {
            self.startup_notices.push(
                "prompt or tool resources changed since this session was last opened".to_string(),
            );
        }
        session.append_runtime_identity(&identity)?;
        if let Some(provider) = provider {
            self.provider = provider;
            self.provider_name = provider_name;
        }
        self.session = session;
        self.model = model;
        self.prompt = prompt;
        self.usage = self.session.usage();
        Ok(())
    }

    fn execute_call(&mut self, call: ToolCall, observer: &mut dyn Observer) -> Result<(), String> {
        let Some(tool) = self.tools.get(&call.name).cloned() else {
            let reason = format!("unknown tool {:?}", call.name);
            return self.record_unstarted_calls(std::slice::from_ref(&call), &reason, observer);
        };
        let detail = match tool.display_detail(&call.arguments, self.session.path()) {
            Ok(detail) => detail,
            Err(error) => {
                return self.record_unstarted_calls(std::slice::from_ref(&call), &error, observer);
            }
        };
        if let Err(error) = observer.event(Event::ToolStart {
            call_id: call.id.clone(),
            name: call.name.clone(),
            detail,
        }) {
            self.record_unstarted_calls(
                std::slice::from_ref(&call),
                "tool start event failed",
                observer,
            )?;
            return Err(error);
        }
        if self.cancellation.cancelled() {
            self.record_unstarted_calls(std::slice::from_ref(&call), "turn cancelled", observer)?;
            return Err("turn cancelled".into());
        }
        let (result, outcome) = match self.execute_tool(&tool, &call, observer) {
            Ok(result) => result,
            Err(error) => (
                ToolResult {
                    content: error,
                    is_error: true,
                },
                ToolOutcome::NotStarted,
            ),
        };
        let end = observer.event(Event::ToolEnd {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: result.content.clone(),
            is_error: result.is_error,
            outcome,
        });
        self.session
            .append_message(&Message::tool_result_with_outcome(
                &call.id,
                &call.name,
                &result.content,
                result.is_error,
                outcome,
            ))?;
        end?;
        crate::process::ensure_cleanup()
    }

    fn record_unstarted_calls(
        &mut self,
        calls: &[ToolCall],
        reason: &str,
        observer: &mut dyn Observer,
    ) -> Result<(), String> {
        for call in calls {
            let content = format!("tool call not started: {reason}");
            self.session
                .append_message(&Message::tool_result_with_outcome(
                    &call.id,
                    &call.name,
                    &content,
                    true,
                    ToolOutcome::NotStarted,
                ))?;
            let _ = observer.event(Event::ToolEnd {
                call_id: call.id.clone(),
                name: call.name.clone(),
                content,
                is_error: true,
                outcome: ToolOutcome::NotStarted,
            });
        }
        Ok(())
    }

    fn execute_tool(
        &mut self,
        tool: &crate::tools::Tool,
        call: &ToolCall,
        observer: &mut dyn Observer,
    ) -> Result<(ToolResult, ToolOutcome), String> {
        let shell_state = if tool.name == "sh" {
            Some(self.session.shell_state_directory()?)
        } else {
            None
        };
        let context = Context::new(
            self.store.workspace().to_path_buf(),
            self.store.directory().to_path_buf(),
            self.session.id().to_string(),
            shell_state,
        )?;
        let mut observer_error = None;
        let (result, outcome) = tool.execute(
            &call.arguments,
            &context,
            &self.cancellation,
            &mut |stream, text| {
                if let Err(error) = observer.event(Event::ToolOutput {
                    name: call.name.clone(),
                    stream,
                    text: text.to_string(),
                }) {
                    observer_error = Some(error);
                    self.cancellation.cancel();
                }
            },
        );
        if let Some(error) = observer_error {
            return Ok((
                ToolResult {
                    content: error,
                    is_error: true,
                },
                ToolOutcome::InterruptedUnknown,
            ));
        }
        Ok((
            ToolResult {
                is_error: result.is_err(),
                content: match result {
                    Ok(content) => content,
                    Err(error) => error,
                },
            },
            outcome,
        ))
    }

    fn request(&self, messages: Vec<Message>, tools: bool) -> Request {
        let request = Request::new(&self.model, messages)
            .with_system(self.prompt.fragments().to_vec())
            .with_cancellation(self.cancellation.clone());
        if tools {
            let specs = self.tools.tools().iter().map(crate::tools::Tool::spec);
            request.with_tools(specs.collect())
        } else {
            request
        }
    }

    fn maybe_compact(
        &mut self,
        force: bool,
        focus: Option<String>,
        observer: &mut dyn Observer,
    ) -> Result<bool, String> {
        let messages = self.session.context_messages()?;
        let prefix = self.request(Vec::new(), true);
        let tokens_before = estimate_request_tokens(&prefix.system, &messages, &prefix.tools);
        let threshold = self
            .context
            .window_tokens
            .saturating_sub(self.context.output_reserve_tokens);
        if !force && tokens_before <= threshold {
            return Ok(false);
        }
        let reserve = self.context.output_reserve_tokens;
        let tail_tokens = self.context.recent_tail_tokens;
        validate_compaction_limits(self.context.window_tokens, reserve, tail_tokens)?;
        let (older, retained_tail) = split_tail(&messages, tail_tokens);
        if older.is_empty() {
            return Ok(false);
        }
        let mut system = vec![crate::prompt::COMPACTION.trim().to_string()];
        // The summarizer keeps what it is told about the workspace, and the
        // model that continues afterwards still has the instructions in front
        // of it even though the messages that carried them are gone.
        if let Some(instructions) = self.prompt.instructions() {
            system.push(instructions.to_string());
        }
        if let Some(focus) = focus.as_deref() {
            system.push(format!(
                "Additional focus: {}",
                clipped_text(focus, self.context.window_tokens as usize / 2)
            ));
        }
        let input = compaction_input(&older, self.context.window_tokens);
        let request = Request::new(&self.model, input)
            .with_system(system)
            .with_cancellation(self.cancellation.clone());
        if estimate_request_tokens(&request.system, &request.messages, &[])
            >= self.context.window_tokens
        {
            return Err("compaction instructions exceed the model limit".to_string());
        }
        let mut sink = CancellationSink {
            cancellation: &self.cancellation,
        };
        let completion = self
            .provider
            .complete(&request, &mut sink)
            .map_err(|error| format!("compaction failed: {error}"))?;
        self.usage.add(&completion.usage);
        self.session.append_usage(&completion.usage)?;
        observer.event(Event::Usage {
            request: completion.usage,
            total: self.usage,
        })?;
        let summary = completion.content;
        if summary.trim().is_empty() {
            return Err("compaction returned an empty summary".to_string());
        }
        let retained_tail = cap_tool_results(retained_tail, self.context.window_tokens);
        let compact_context = {
            let mut context = vec![Message::user(format!("[Conversation summary]\n{summary}"))];
            context.extend(retained_tail.clone());
            context
        };
        let hard_limit = self.context.window_tokens.saturating_sub(reserve);
        if estimate_request_tokens(&prefix.system, &compact_context, &prefix.tools) > hard_limit {
            return Err("compacted context still exceeds the model limit".to_string());
        }
        let entry = Compaction {
            summary,
            retained_tail,
            tokens_before,
        };
        self.session.append_compaction(&entry)?;
        self.reload_prompt()?;
        observer.event(Event::Notice("context compacted".to_string()))?;
        Ok(true)
    }

    fn ensure_fits(&self, messages: &[Message]) -> Result<(), String> {
        let limit = self
            .context
            .window_tokens
            .saturating_sub(self.context.output_reserve_tokens);
        let prefix = self.request(Vec::new(), true);
        let tokens = estimate_request_tokens(&prefix.system, messages, &prefix.tools);
        if tokens > limit {
            Err(format!(
                "context is approximately {tokens} tokens; limit is {limit}"
            ))
        } else {
            Ok(())
        }
    }
}

fn emit_notices(observer: &mut dyn Observer, notices: Vec<String>) -> Result<(), String> {
    for notice in notices {
        observer.event(Event::Notice(notice))?;
    }
    Ok(())
}

struct RuntimeSink<'a> {
    cancellation: &'a Cancellation,
    observer: &'a mut dyn Observer,
}

impl EventSink for RuntimeSink<'_> {
    fn on_event(&mut self, event: StreamEvent) -> std::io::Result<()> {
        if self.cancellation.cancelled() {
            return Err(std::io::Error::other("turn cancelled"));
        }
        let event = match event {
            StreamEvent::Text(text) => Event::Text(text),
            StreamEvent::Reasoning(text) => Event::Reasoning(text),
            StreamEvent::Notice(text) => Event::Notice(text),
        };
        self.observer.event(event).map_err(std::io::Error::other)
    }
}

struct CancellationSink<'a> {
    cancellation: &'a Cancellation,
}

impl EventSink for CancellationSink<'_> {
    fn on_event(&mut self, _event: StreamEvent) -> std::io::Result<()> {
        if self.cancellation.cancelled() {
            Err(std::io::Error::other("turn cancelled"))
        } else {
            Ok(())
        }
    }
}

fn effective_prompt(workspace: &std::path::Path, tools: &Registry) -> Result<Effective, String> {
    let manifest = serde_json::json!({
        "tools": tools.tools().iter().map(|tool| tool.spec()).collect::<Vec<_>>(),
    });
    Effective::new(workspace, &manifest)
}
fn estimate_request_tokens(prompt: &[String], messages: &[Message], tools: &[ToolSpec]) -> u64 {
    let bytes = serde_json::to_vec(&(prompt, messages, tools))
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX);
    u64::try_from(bytes.saturating_add(3) / 4)
        .unwrap_or(u64::MAX)
        .saturating_add(32)
}

fn estimate_messages(messages: &[Message]) -> u64 {
    let bytes = serde_json::to_vec(messages)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX);
    u64::try_from(bytes.saturating_add(3) / 4).unwrap_or(u64::MAX)
}

fn split_tail(messages: &[Message], tail_tokens: u64) -> (Vec<Message>, Vec<Message>) {
    let mut start = messages.len();
    while start > 0 {
        let candidate = start - 1;
        if estimate_messages(&messages[candidate..]) > tail_tokens && start < messages.len() {
            break;
        }
        start = candidate;
    }
    while start > 0 && messages[start].role == crate::provider::Role::Tool {
        start -= 1;
    }
    (messages[..start].to_vec(), messages[start..].to_vec())
}

fn clipped_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let end = text.floor_char_boundary(max_bytes);
    format!("{} [truncated]", &text[..end])
}

fn cap_tool_results(mut messages: Vec<Message>, window_tokens: u64) -> Vec<Message> {
    let limit = window_tokens.saturating_div(8).saturating_mul(4) as usize;
    for message in &mut messages {
        for block in &mut message.content {
            if let ContentBlock::ToolResult { content, .. } = block
                && content.len() > limit
            {
                *content = clipped_text(content, limit);
            }
        }
    }
    messages
}

fn compaction_input(messages: &[Message], window_tokens: u64) -> Vec<Message> {
    let total_limit = window_tokens.saturating_div(2).saturating_mul(4) as usize;
    let item_limit = window_tokens.saturating_div(8).saturating_mul(4) as usize;
    let mut chunks = Vec::new();
    let mut used = 0;
    for message in messages.iter().rev() {
        let mut chunk = format!("{:?}:\n", message.role);
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => chunk.push_str(&clipped_text(text, item_limit)),
                ContentBlock::ToolCall { call } => chunk.push_str(&format!(
                    "tool call {} {} {}",
                    call.id,
                    call.name,
                    clipped_text(&call.arguments, item_limit)
                )),
                ContentBlock::ToolResult {
                    tool_name,
                    content,
                    outcome,
                    ..
                } => chunk.push_str(&format!(
                    "tool result {tool_name} {outcome:?}: {}",
                    clipped_text(content, item_limit)
                )),
            }
            chunk.push('\n');
        }
        let remaining = total_limit.saturating_sub(used);
        if remaining == 0 {
            break;
        }
        let chunk = clipped_text(&chunk, remaining);
        used = used.saturating_add(chunk.len());
        chunks.push(chunk);
    }
    chunks.reverse();
    vec![Message::user(chunks.concat())]
}

fn validate_compaction_limits(window: u64, reserve: u64, tail: u64) -> Result<(), String> {
    if reserve >= window || tail >= window.saturating_sub(reserve) {
        return Err("invalid compaction thresholds".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_does_not_begin_with_a_tool_result() {
        let messages = vec![
            Message::user("old"),
            Message::assistant_response(String::new(), vec![ToolCall::new("one", "sh", "{}")]),
            Message::tool_result("one", "sh", "result", false),
            Message::assistant("done"),
        ];
        let (older, tail) = split_tail(&messages, 20);
        assert!(!older.is_empty());
        assert_ne!(tail.first().unwrap().role, crate::provider::Role::Tool);
    }

    #[test]
    fn retained_tool_result_is_capped_without_changing_history() {
        let original = vec![Message::tool_result("one", "sh", "x".repeat(10_000), false)];
        let retained = cap_tool_results(original.clone(), 800);
        assert!(retained[0].text_content().len() < 500);
        assert!(retained[0].text_content().contains("[truncated]"));
        assert_eq!(original[0].text_content().len(), 10_000);
    }

    #[test]
    fn bad_compaction_thresholds_are_rejected() {
        assert!(validate_compaction_limits(100, 100, 1).is_err());
        assert!(validate_compaction_limits(100, 20, 80).is_err());
        assert!(validate_compaction_limits(100, 20, 10).is_ok());
    }
}
