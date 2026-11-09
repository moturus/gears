mod history;
pub mod jsonl;
pub mod line;
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
