mod history;
pub mod jsonl;
pub mod line;
mod markdown;
pub mod select;
pub mod tui;
pub mod tui_editor;

use crate::runtime::{Observer, Runtime};

enum Operation {
    Turn(String),
}

impl Operation {
    fn run(self, runtime: &mut Runtime, observer: &mut dyn Observer) -> Result<(), String> {
        match self {
            Self::Turn(prompt) => runtime.turn(prompt, observer),
        }
    }
}

pub(crate) fn help(_runtime: &Runtime) -> String {
    crate::cli::USAGE.to_string()
}

pub(crate) fn model_choices(config: &crate::config::Config, provider: &str) -> Vec<String> {
    let mut models = config
        .provider(provider)
        .map(|provider| provider.models)
        .unwrap_or_default();
    for model in &config.models {
        if !models.contains(model) {
            models.push(model.clone());
        }
    }
    models
}

/// Setup sees the provider addresses saved since the configuration loaded.
pub(crate) fn current_config(
    config: &crate::config::Config,
) -> Result<crate::config::Config, String> {
    let mut config = config.clone();
    if let Some(choice) = crate::provider_choice::ProviderChoice::load_default()? {
        config.apply_urls(&choice.urls)?;
    }
    Ok(config)
}

pub(crate) fn reset_notice(removed: usize) -> String {
    format!(
        "Gears was reset ({removed} entries removed). This run keeps its provider, model and key until Gears restarts."
    )
}

/// The built-in tools, one per line.
pub(crate) fn format_tools(runtime: &Runtime) -> String {
    runtime
        .tools()
        .tools()
        .iter()
        .map(|tool| {
            format!(
                "{} : {}",
                tool.name,
                if tool.read_only { "reads" } else { "writes" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The context window, its reserve and recent tail, and how much of the window
/// the active branch currently holds. `/context` shows this before a change.
pub(crate) fn context_report(runtime: &Runtime) -> Result<String, String> {
    let context = runtime.context();
    let (used, max) = runtime.context_usage()?;
    Ok(format!(
        "window: {max} tokens\nused: {used} tokens ({})\noutput reserve: {} tokens\nrecent tail: {} tokens",
        percent(used, max),
        context.output_reserve_tokens,
        context.recent_tail_tokens
    ))
}

/// A whole-number percentage of `max`, rounded to the nearest point.
pub(crate) fn percent(used: u64, max: u64) -> String {
    let max = max.max(1);
    format!(
        "{}%",
        used.saturating_mul(100).saturating_add(max / 2) / max
    )
}

/// `/context` reports the window and its usage; `/context set TOKENS` changes
/// the window and saves it as the default in the user's gears.toml.
pub(crate) fn context_command(runtime: &mut Runtime, argument: &str) -> Result<String, String> {
    let argument = argument.trim();
    if argument.is_empty() {
        return context_report(runtime);
    }
    let (action, value) = argument
        .split_once(char::is_whitespace)
        .map_or((argument, ""), |(action, value)| (action, value.trim()));
    match action {
        "set" => {
            let window = parse_tokens(value)?;
            runtime.set_context_window(window)?;
            let notice = match runtime.save_context_default() {
                Ok(()) => format!(
                    "context window set to {window} tokens and saved as the default in gears.toml"
                ),
                Err(error) => format!(
                    "context window set to {window} tokens for this run; saving the default failed: {error}"
                ),
            };
            Ok(format!("{notice}\n{}", context_report(runtime)?))
        }
        _ => Err("usage: /context [set TOKENS]".to_string()),
    }
}

/// A token count as typed, so thousands separators are accepted.
fn parse_tokens(value: &str) -> Result<u64, String> {
    let digits: String = value
        .chars()
        .filter(|character| !matches!(character, ',' | '_'))
        .collect();
    digits
        .parse()
        .map_err(|_| "usage: /context set TOKENS".to_string())
}

pub(crate) fn selection_and_default(argument: &str) -> (&str, bool) {
    let argument = argument.trim();
    if argument == "--default" {
        ("", true)
    } else if let Some(selection) = argument.strip_suffix(" --default") {
        (selection.trim(), true)
    } else {
        (argument, false)
    }
}
