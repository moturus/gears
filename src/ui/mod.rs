mod history;
pub mod jsonl;
pub mod line;
pub mod select;
pub mod tui;
pub mod tui_editor;

use crate::runtime::{Responder, Observer, Runtime, ToolStatus};
use crate::session::ToolState;

enum Operation {
    Turn(String),
    HookCommand { name: String, argument: String },
    Questions,
}

impl Operation {
    fn run(
        self,
        runtime: &mut Runtime,
        responder: &mut dyn Responder,
        observer: &mut dyn Observer,
    ) -> Result<(), String> {
        match self {
            Self::Turn(prompt) => runtime.turn(prompt, responder, observer),
            Self::HookCommand { name, argument } => {
                runtime.hook_command_with(&name, &argument, responder, observer)
            }
            Self::Questions => runtime.resolve_hook_questions(responder, observer),
        }
    }
}

fn question_answer(
    question: &crate::hooks::Question,
    answer: Option<&str>,
) -> Result<Option<String>, String> {
    let answer = answer.unwrap_or_default().trim();
    if answer.is_empty() {
        return Ok(None);
    }
    if let Some(choice) = question
        .choices
        .iter()
        .find(|choice| choice.value == answer)
    {
        return Ok(Some(choice.value.clone()));
    }
    if let Ok(number) = answer.parse::<usize>()
        && let Some(choice) = number
            .checked_sub(1)
            .and_then(|index| question.choices.get(index))
    {
        return Ok(Some(choice.value.clone()));
    }
    Err("choose a listed answer, or press Enter to skip".into())
}

pub(crate) fn help(runtime: &Runtime) -> String {
    let mut text = crate::cli::USAGE.to_string();
    for command in runtime.hook_commands() {
        text.push_str(&format!("/{} - {}\n", command.name, command.help));
    }
    text
}

/// Models to offer: the provider's own list, then the configured `[models] used`.
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

pub(crate) fn parse_tool_action(argument: &str) -> Result<(&str, ToolState, bool), String> {
    let parts = argument.split_whitespace().collect::<Vec<_>>();
    let ([action, name] | [action, name, "--global"]) = parts.as_slice() else {
        return Err("usage: /tool enable|disable|reset NAME [--global]".to_string());
    };
    let state = match *action {
        "enable" => ToolState::Enabled,
        "disable" => ToolState::Disabled,
        "reset" => ToolState::Inherit,
        _ => return Err("usage: /tool enable|disable|reset NAME [--global]".to_string()),
    };
    Ok((name, state, parts.len() == 3))
}

pub(crate) fn format_tools(runtime: &Runtime) -> Result<String, String> {
    let tools = runtime.tool_statuses()?;
    if tools.is_empty() {
        return Ok("no directory tools configured".to_string());
    }
    Ok(format_tool_rows(&tools))
}

/// Pads every column so the `:`, `(` and `)` line up: the state column is
/// padded before the bracket, and the source is padded inside it, because a
/// bracket only stays in column if the text before it does.
fn format_tool_rows(tools: &[ToolStatus]) -> String {
    let rows = tools
        .iter()
        .map(|tool| {
            (
                tool.name.as_str(),
                if tool.enabled { "enabled" } else { "disabled" },
                tool.source,
                if tool.available {
                    ""
                } else {
                    ", unavailable here"
                },
            )
        })
        .collect::<Vec<_>>();
    let name_width = widest(&rows, |row| field_width(row.0));
    let state_width = widest(&rows, |row| field_width(row.1));
    let source_width = widest(&rows, |row| field_width(row.2));
    rows.iter()
        .map(|row| {
            format!(
                "{:<name_width$} : {:<state_width$} ({:<source_width$}){}",
                row.0, row.1, row.2, row.3
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn widest<T>(rows: &[T], width: impl Fn(&T) -> usize) -> usize {
    rows.iter().map(width).max().unwrap_or(0)
}

fn field_width(text: &str) -> usize {
    text.chars().count()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_command_grammar_is_strict() {
        assert_eq!(
            parse_tool_action("enable sh --global").unwrap(),
            ("sh", ToolState::Enabled, true)
        );
        assert_eq!(
            parse_tool_action("reset sh").unwrap(),
            ("sh", ToolState::Inherit, false)
        );
        for invalid in [
            "",
            "enable",
            "enable sh extra",
            "enable --global sh",
            "other sh",
        ] {
            assert!(parse_tool_action(invalid).is_err(), "{invalid}");
        }
    }
    #[test]

    fn tool_rows_align_their_colons_and_brackets() {
        let rows = [
            ToolStatus {
                name: "sh".to_string(),
                enabled: true,
                available: true,
                source: "config",
            },
            ToolStatus {
                name: "web_search".to_string(),
                enabled: false,
                available: false,
                source: "session",
            },
        ];
        let text = format_tool_rows(&rows);
        assert_eq!(
            text,
            "sh         : enabled  (config )\nweb_search : disabled (session), unavailable here"
        );
        let lines = text.lines().collect::<Vec<_>>();
        let column = |line: &&str, mark: char| line.find(mark).unwrap();
        assert!(
            lines
                .iter()
                .all(|line| column(line, ')') == column(&lines[0], ')'))
        );
    }
}
