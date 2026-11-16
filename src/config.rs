//! Small, versioned configuration for the provider and runtime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const DEFAULT_CONTEXT_TOKENS: u64 = 128_000;
/// The smallest window Gears accepts, from the configuration or /context set.
pub const MIN_CONTEXT_TOKENS: u64 = 4096;
/// Model round trips one turn of an unattended run may use; attended sessions
/// have no cap.
const DEFAULT_TOOL_ROUNDS: usize = 256;
const MAX_TOOL_ROUNDS: usize = 4096;
const DEFAULT_OUTPUT_RESERVE: u64 = 16_384;
const DEFAULT_RECENT_TAIL: u64 = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextConfig {
    pub window_tokens: u64,
    pub output_reserve_tokens: u64,
    pub recent_tail_tokens: u64,
}

impl ContextConfig {
    /// The bounds a window must satisfy so compaction still leaves room for a
    /// summary: the window covers the output reserve and the recent tail.
    pub fn validate(&self) -> Result<(), String> {
        if self.window_tokens < MIN_CONTEXT_TOKENS {
            return Err(format!(
                "context.window_tokens must be at least {MIN_CONTEXT_TOKENS}"
            ));
        }
        if self.output_reserve_tokens >= self.window_tokens {
            return Err("context.output_reserve_tokens must be below window_tokens".to_string());
        }
        if self.recent_tail_tokens
            >= self
                .window_tokens
                .saturating_sub(self.output_reserve_tokens)
        {
            return Err("context.recent_tail_tokens leaves no room for a summary".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NamedProvider {
    pub base_url: String,
    pub plain_http: bool,
    pub model: Option<String>,
    /// Model ids offered when choosing a model; any id may still be used.
    pub models: Vec<String>,
    /// Without a key file the provider is sent no `Authorization` header.
    pub key_file: Option<PathBuf>,
    pub ca_cert: Option<PathBuf>,
    pub reasoning_echo: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub egress_allowlist: Vec<String>,
    pub plain_http_allowlist: Vec<String>,
    pub models: Vec<String>,
    /// Every provider, `[provider]` among them as "default".
    pub providers: BTreeMap<String, NamedProvider>,

    pub default_provider: String,
    pub default_model: Option<String>,
    pub log_file: Option<PathBuf>,
    pub log_level: crate::trace::Level,
    pub max_tool_rounds: usize,
    pub context: ContextConfig,
    /// The system prompt file this build reads, named by the configuration and
    /// found beside it. It is never a path.
    pub system_prompt_file: String,
    /// That file's text, read when the configuration is loaded from a file. A
    /// configuration that was only parsed has none.
    pub system_prompt: String,
    /// The file this configuration was read from. It seeds a missing user
    /// gears.toml when interactive defaults are saved for the first time.
    pub source_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            egress_allowlist: Vec::new(),
            plain_http_allowlist: Vec::new(),
            models: Vec::new(),
            providers: BTreeMap::new(),

            default_provider: "default".to_string(),
            default_model: None,
            log_file: None,
            log_level: crate::trace::Level::Info,
            max_tool_rounds: DEFAULT_TOOL_ROUNDS,
            context: ContextConfig {
                window_tokens: DEFAULT_CONTEXT_TOKENS,
                output_reserve_tokens: DEFAULT_OUTPUT_RESERVE,
                recent_tail_tokens: DEFAULT_RECENT_TAIL,
            },
            system_prompt_file: String::new(),
            system_prompt: String::new(),
            source_path: None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    version: u32,
    default_provider: Option<String>,
    default_model: Option<String>,
    /// The system prompt file for each platform. Both are required: Gears sends
    /// the one for the platform it was built for.
    system_prompt_linux: String,
    system_prompt_motor_os: String,
    #[serde(default)]
    net: RawNet,
    provider: Option<RawProvider>,
    #[serde(default)]
    providers: BTreeMap<String, RawProvider>,
    #[serde(default)]
    models: RawModels,
    #[serde(default)]
    trace: RawTrace,
    #[serde(default)]
    runtime: RawRuntime,
    #[serde(default)]
    context: RawContext,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNet {
    egress_allowlist: Option<Vec<String>>,
    plain_http_allowlist: Option<Vec<String>>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProvider {
    base_url: Option<String>,
    model: Option<String>,
    api_key_file: Option<PathBuf>,
    ca_cert: Option<PathBuf>,
    reasoning_echo: Option<bool>,
    models: Option<Vec<String>>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModels {
    used: Option<Vec<String>>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTrace {
    file: Option<PathBuf>,
    level: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRuntime {
    max_tool_rounds: Option<usize>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawContext {
    window_tokens: Option<u64>,
    output_reserve_tokens: Option<u64>,
    recent_tail_tokens: Option<u64>,
}

impl Config {
    pub fn provider(&self, name: &str) -> Option<NamedProvider> {
        self.providers.get(name).cloned()
    }

    pub fn default_path() -> Option<PathBuf> {
        crate::directory::default_root()
            .ok()
            .map(|root| root.join("gears.toml"))
    }

    pub fn load_user(explicit: Option<&Path>) -> Result<Self, String> {
        if let Some(path) = explicit {
            return Self::read_config(path)?
                .ok_or_else(|| format!("{}: not found", path.display()));
        }
        Self::load_found(&Found::detect(), false)
    }

    /// Load what `detect` found; with both, `source_tree` picks the shipped one.
    pub fn load_found(found: &Found, source_tree: bool) -> Result<Self, String> {
        match (found, source_tree) {
            (Found::User(user), _) | (Found::Both { user, .. }, false) => {
                Self::read_config(user)?.ok_or_else(|| format!("{}: not found", user.display()))
            }
            (Found::SourceTree(package), _) | (Found::Both { package, .. }, true) => {
                let path = package.join("config/gears.toml");
                Self::read_config(&path)?.ok_or_else(|| format!("{}: not found", path.display()))
            }
            (Found::Neither(directory), _) => Err(format!(
                "no gears.toml in {}: copy config/gears.toml and config/SYSTEM.md from the Gears source there",
                directory.display()
            )),
        }
    }

    /// Use the addresses the user chose for providers. A host the user typed is
    /// allowed for egress, and for plain HTTP when its URL uses `http`.
    /// Addresses for providers this file does not define are left for others.
    pub fn apply_urls(&mut self, urls: &BTreeMap<String, String>) -> Result<(), String> {
        for (name, url) in urls {
            let Some(provider) = self.providers.get_mut(name) else {
                continue;
            };
            let endpoint = crate::provider::Endpoint::new(url)
                .map_err(|error| format!("saved address for provider {name:?}: {error}"))?;
            let host = validate_hosts(vec![endpoint.url().host().to_string()])?.remove(0);
            provider.plain_http = endpoint.url().scheme() == crate::net::Scheme::Http;
            if !self.egress_allowlist.contains(&host) {
                self.egress_allowlist.push(host.clone());
            }
            if provider.plain_http && !self.plain_http_allowlist.contains(&host) {
                self.plain_http_allowlist.push(host);
            }
            provider.base_url = url.clone();
        }
        Ok(())
    }

    /// Save a provider and model choice as defaults in the user's gears.toml,
    /// keeping the rest of the file as written.
    pub fn save_model_defaults(
        &self,
        directory: &Path,
        provider: &str,
        model: &str,
    ) -> Result<(), String> {
        self.edit_user_config(directory, |config| {
            config["default_provider"] = toml_edit::value(provider);
            config["default_model"] = toml_edit::value(model);
        })
    }

    /// Save the context window as the default in the user's gears.toml.
    pub fn save_context_default(&self, directory: &Path, window_tokens: u64) -> Result<(), String> {
        let window_tokens = i64::try_from(window_tokens)
            .map_err(|_| format!("context window {window_tokens} is too large"))?;
        self.edit_user_config(directory, |config| {
            let context = &mut config["context"];
            if context.is_none() {
                *context = toml_edit::Item::Table(toml_edit::Table::new());
            }
            context["window_tokens"] = toml_edit::value(window_tokens);
        })
    }

    /// Edit the user's gears.toml in place, preserving comments and layout. A
    /// missing file is seeded from the configuration this run loaded. The
    /// edited file must still load, so a saved default never breaks a start.
    fn edit_user_config(
        &self,
        directory: &Path,
        edit: impl FnOnce(&mut toml_edit::DocumentMut),
    ) -> Result<(), String> {
        let state = crate::state::StateRoot::new(directory.to_path_buf())?;
        let path = state.file(Path::new("gears.toml"))?;
        let (text, seeded) = match std::fs::read_to_string(&path) {
            Ok(text) => (text, false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let source = self.source_path.as_deref().ok_or_else(|| {
                    format!(
                        "{}: not found; copy config/gears.toml there first",
                        path.display()
                    )
                })?;
                let text = std::fs::read_to_string(source)
                    .map_err(|error| format!("{}: {error}", source.display()))?;
                (text, true)
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let mut document: toml_edit::DocumentMut = text
            .parse()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        edit(&mut document);
        let text = document.to_string();
        Self::parse_in(&text, directory).map_err(|error| format!("{}: {error}", path.display()))?;
        if seeded {
            // The prompt is read beside gears.toml, so a seeded file needs its
            // prompt written first: the next start reads it there.
            self.seed_system_prompt(&state)?;
        }
        state.replace_file(Path::new("gears.toml"), text.as_bytes())
    }

    /// Write this run's system prompt beside a gears.toml Gears just seeded,
    /// unless the directory already has one.
    fn seed_system_prompt(&self, state: &crate::state::StateRoot) -> Result<(), String> {
        if self.system_prompt.is_empty() {
            return Ok(());
        }
        let name = Path::new(&self.system_prompt_file);
        if state.existing_file(name)?.is_some() {
            return Ok(());
        }
        state.replace_file(name, self.system_prompt.as_bytes())
    }

    fn read_config(path: &Path) -> Result<Option<Self>, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                // Key and certificate files live in the Gears directory, whichever
                // gears.toml names them.
                let directory = crate::directory::default_root().ok();
                let mut config = Self::parse_with_base(&text, directory.as_deref())
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                config.source_path = Some(path.to_path_buf());
                config.read_system_prompt()?;
                Ok(Some(config))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    /// Read this build's system prompt from beside the file this configuration
    /// came from. Only a loaded configuration knows where that is.
    fn read_system_prompt(&mut self) -> Result<(), String> {
        let source = self
            .source_path
            .as_deref()
            .ok_or("the system prompt needs the file this configuration came from")?;
        let directory = source
            .parent()
            .ok_or_else(|| format!("{}: no directory", source.display()))?;
        let path = directory.join(&self.system_prompt_file);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "{}: not found; copy SYSTEM.md beside {}",
                    path.display(),
                    source.display()
                ));
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        if text.trim().is_empty() {
            return Err(format!("{}: the system prompt is empty", path.display()));
        }
        self.system_prompt = text;
        Ok(())
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        Self::parse_with_base(text, None)
    }

    /// Parse with relative key and certificate paths in `directory`.
    pub fn parse_in(text: &str, directory: &Path) -> Result<Self, String> {
        Self::parse_with_base(text, Some(directory))
    }

    fn parse_with_base(text: &str, base: Option<&Path>) -> Result<Self, String> {
        let raw: RawConfig = toml::from_str(text).map_err(|error| error.to_string())?;
        if raw.version != 1 {
            return Err(format!(
                "unsupported config version {} (expected 1)",
                raw.version
            ));
        }
        prompt_file_name("system_prompt_linux", &raw.system_prompt_linux)?;
        prompt_file_name("system_prompt_motor_os", &raw.system_prompt_motor_os)?;
        let system_prompt_file = platform_prompt_file(&raw).to_string();
        let egress_allowlist = validate_hosts(raw.net.egress_allowlist.unwrap_or_default())?;
        let plain_http_allowlist =
            validate_hosts(raw.net.plain_http_allowlist.unwrap_or_default())?;
        if let Some(host) = plain_http_allowlist
            .iter()
            .find(|host| !egress_allowlist.contains(host))
        {
            return Err(format!(
                "net.plain_http_allowlist host {host:?} must also be in net.egress_allowlist"
            ));
        }
        // `[provider]` is the provider named "default"; nothing is built in.
        let mut providers = BTreeMap::new();
        if let Some(provider) = raw.provider {
            let provider = named_provider(
                "provider",
                provider,
                &egress_allowlist,
                &plain_http_allowlist,
                base,
            )?;
            providers.insert("default".to_string(), provider);
        }
        for (name, provider) in raw.providers {
            if name.is_empty() || !valid_name(&name) || name == "default" {
                return Err(format!("invalid provider name {name:?}"));
            }
            let label = format!("providers.{name}");
            let provider = named_provider(
                &label,
                provider,
                &egress_allowlist,
                &plain_http_allowlist,
                base,
            )?;
            providers.insert(name, provider);
        }
        let default_provider = match raw.default_provider {
            Some(name) if providers.contains_key(&name) => name,
            Some(name) => return Err(format!("unknown default_provider {name:?}")),
            None if providers.len() == 1 => providers.keys().next().unwrap().clone(),
            None if providers.len() > 1 => {
                return Err("default_provider is required with multiple providers".to_string());
            }
            None => "default".to_string(),
        };
        let log_level = match raw.trace.level {
            Some(level) => crate::trace::Level::parse(&level)
                .ok_or_else(|| format!("unknown trace level {level:?}"))?,
            None => crate::trace::Level::Info,
        };
        let max_tool_rounds = raw.runtime.max_tool_rounds.unwrap_or(DEFAULT_TOOL_ROUNDS);
        if !(1..=MAX_TOOL_ROUNDS).contains(&max_tool_rounds) {
            return Err(format!(
                "runtime.max_tool_rounds must be between 1 and {MAX_TOOL_ROUNDS}"
            ));
        }
        let context = ContextConfig {
            window_tokens: raw.context.window_tokens.unwrap_or(DEFAULT_CONTEXT_TOKENS),
            output_reserve_tokens: raw
                .context
                .output_reserve_tokens
                .unwrap_or(DEFAULT_OUTPUT_RESERVE),
            recent_tail_tokens: raw
                .context
                .recent_tail_tokens
                .unwrap_or(DEFAULT_RECENT_TAIL),
        };
        let mut models = raw.models.used.unwrap_or_default();
        context.validate()?;
        models.retain(|model| !model.trim().is_empty());
        let mut seen_models = std::collections::HashSet::new();
        models.retain(|model| seen_models.insert(model.clone()));
        Ok(Self {
            egress_allowlist,
            plain_http_allowlist,
            models,
            providers,
            default_provider,
            default_model: raw.default_model.filter(|model| !model.trim().is_empty()),
            log_file: raw.trace.file,
            log_level,
            max_tool_rounds,
            context,
            system_prompt_file,
            system_prompt: String::new(),
            source_path: None,
        })
    }
}

/// The `gears.toml` files available when `--config` is not given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// `gears.toml` in the Gears directory.
    User(PathBuf),
    /// The Gears package Gears was built in, whose `config/gears.toml` ships.
    SourceTree(PathBuf),
    Both {
        user: PathBuf,
        package: PathBuf,
    },
    /// Neither; holds the Gears directory.
    Neither(PathBuf),
}

impl Found {
    pub fn detect() -> Self {
        let directory = crate::directory::default_root().unwrap_or_default();
        let user = directory.join("gears.toml");
        let package = std::env::current_exe()
            .ok()
            .and_then(|executable| source_tree_of(&executable));
        match (user.is_file(), package) {
            (true, Some(package)) => Self::Both { user, package },
            (true, None) => Self::User(user),
            (false, Some(package)) => Self::SourceTree(package),
            (false, None) => Self::Neither(directory),
        }
    }
}

/// The package directory when `executable` lies in its `target/` tree, as after
/// `lorry run`; only the Gears package with its shipped
/// `config/gears.toml` counts.
fn source_tree_of(executable: &Path) -> Option<PathBuf> {
    let target = executable
        .ancestors()
        .find(|path| path.file_name() == Some("target".as_ref()))?;
    let package = target.parent()?;
    let manifest: toml::Table =
        toml::from_str(&std::fs::read_to_string(package.join("Cargo.toml")).ok()?).ok()?;
    let gears = manifest.get("package")?.get("name")?.as_str()? == "gears";
    (gears && package.join("config/gears.toml").is_file()).then(|| package.to_path_buf())
}

fn named_provider(
    label: &str,
    raw: RawProvider,
    egress_allowlist: &[String],
    plain_http_allowlist: &[String],
    base: Option<&Path>,
) -> Result<NamedProvider, String> {
    let base_url = raw
        .base_url
        .ok_or_else(|| format!("{label} needs base_url"))?;
    let endpoint = crate::provider::Endpoint::new(&base_url)
        .map_err(|error| format!("bad {label}.base_url: {error}"))?;
    let plain_http = endpoint.url().scheme() == crate::net::Scheme::Http;
    if plain_http {
        // Refuse at load time, so the user sees this rather than a key error
        // or a per-request refusal.
        crate::net::EgressPolicy::new(egress_allowlist)
            .with_plain_http_allowlist(plain_http_allowlist)
            .check(endpoint.url())
            .map_err(|error| {
                format!(
                    "{label}.base_url: {error}; list the host in both \
                     net.egress_allowlist and net.plain_http_allowlist"
                )
            })?;
    }
    let mut models = raw.models.unwrap_or_default();
    models.retain(|model| !model.trim().is_empty());
    Ok(NamedProvider {
        base_url,
        plain_http,
        model: raw.model.filter(|model| !model.trim().is_empty()),
        models,
        key_file: provider_path(raw.api_key_file, base)?,
        ca_cert: provider_path(raw.ca_cert, base)?,
        reasoning_echo: raw.reasoning_echo,
    })
}

fn provider_path(path: Option<PathBuf>, base: Option<&Path>) -> Result<Option<PathBuf>, String> {
    path.map(|path| {
        if path.is_absolute() {
            Ok(path)
        } else {
            base.map(|base| base.join(&path)).ok_or_else(|| {
                format!("provider path {} needs the Gears directory", path.display())
            })
        }
    })
    .transpose()
}

fn validate_hosts(hosts: Vec<String>) -> Result<Vec<String>, String> {
    hosts.iter().map(|host| validate_host(host)).collect()
}

fn validate_host(host: &str) -> Result<String, String> {
    let host = host.to_ascii_lowercase();
    if host.is_empty()
        || host.contains(['/', ':', '@'])
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return Err(format!("bad egress host {host:?}"));
    }
    Ok(host)
}

fn valid_name(name: &str) -> bool {
    name.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// The system prompt file this build sends. The platform is chosen when Gears
/// is built, not when it runs.
#[cfg(target_os = "motor")]
fn platform_prompt_file(raw: &RawConfig) -> &str {
    &raw.system_prompt_motor_os
}

#[cfg(not(target_os = "motor"))]
fn platform_prompt_file(raw: &RawConfig) -> &str {
    &raw.system_prompt_linux
}

/// A system prompt lives beside the configuration that names it, so the
/// configuration gives a file name and nothing else.
fn prompt_file_name(label: &str, name: &str) -> Result<(), String> {
    let mut components = Path::new(name).components();
    let bare = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
        && !name.contains(['/', '\\']);
    if !bare {
        return Err(format!(
            "{label} must be a file name beside gears.toml, not a path: {name:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The prompt settings every gears.toml carries: a file name in the
    /// configuration's own directory.
    const PROMPTS: &str =
        "system_prompt_linux = 'SYSTEM.md'\nsystem_prompt_motor_os = 'SYSTEM.md'\n";

    /// Configuration text for the tests, with the mandatory prompt settings.
    fn configured(body: &str) -> String {
        format!("version = 1\n{PROMPTS}{body}")
    }

    #[test]
    fn plaintext_hosts_require_explicit_egress_and_use_the_same_matching() {
        assert!(
            Config::parse(&configured(""))
                .unwrap()
                .plain_http_allowlist
                .is_empty()
        );
        let config = Config::parse(&configured(
            r#"[net]
egress_allowlist = ["192.168.4.1", "Host.Test"]
plain_http_allowlist = ["192.168.4.1", "host.test"]
"#,
        ))
        .unwrap();
        assert_eq!(config.plain_http_allowlist, config.egress_allowlist);
        for host in ["192.168.4.1", "api.host.test"] {
            let error = Config::parse(&configured(&format!(
                r#"[net]
egress_allowlist = ["host.test"]
plain_http_allowlist = ["{host}"]
"#
            )))
            .unwrap_err();
            assert!(error.contains("net.plain_http_allowlist"));
            assert!(error.contains("net.egress_allowlist"));
        }
        assert!(
            Config::parse(&configured(
                r#"[net]
plain_http_allowlist = ["http://host.test"]
"#
            ))
            .is_err()
        );
    }

    #[test]
    fn a_plaintext_endpoint_is_checked_when_the_config_loads() {
        let text = |net: &str| {
            configured(&format!(
                r#"[provider]
base_url = "http://192.168.4.1:8080/v1"
[net]
{net}
"#
            ))
        };
        for net in [
            "",
            r#"egress_allowlist = ["192.168.4.1"]"#,
            r#"egress_allowlist = ["192.168.4.1", "host.test"]
plain_http_allowlist = ["host.test"]"#,
        ] {
            let error = Config::parse(&text(net)).unwrap_err();
            assert!(
                error.contains("plain HTTP is not allowed"),
                "{net}: {error}"
            );
            assert!(error.contains("net.plain_http_allowlist"), "{net}: {error}");
        }
        let config = Config::parse(&text(
            r#"egress_allowlist = ["192.168.4.1"]
plain_http_allowlist = ["192.168.4.1"]"#,
        ))
        .unwrap();
        assert!(config.providers["default"].plain_http);
    }

    #[test]
    fn minimal_config_has_redesign_defaults() {
        let config = Config::parse(&configured("")).unwrap();
        assert_eq!(config.max_tool_rounds, 256);
        assert_eq!(config.context.output_reserve_tokens, 16_384);
        // No provider or host is built in; they come from the shipped gears.toml.
        assert!(config.provider("default").is_none());
        assert!(config.egress_allowlist.is_empty());
    }

    #[test]
    fn relative_key_files_live_in_the_gears_directory() {
        let directory = Path::new("/home/user/.gears");
        let config = Config::parse_in(
            &configured("[provider]\nbase_url = 'https://one.test/v1'\nmodel = 'current'\napi_key_file = 'secrets/key'\nca_cert = 'ca.pem'\nmodels = ['a', '', 'b']\n"),
            directory,
        )
        .unwrap();
        let provider = config.provider("default").unwrap();
        assert_eq!(provider.model.as_deref(), Some("current"));
        assert_eq!(provider.models, ["a", "b"]);
        assert_eq!(provider.key_file, Some(directory.join("secrets/key")));
        assert_eq!(provider.ca_cert, Some(directory.join("ca.pem")));
        assert!(
            Config::parse(&configured("[provider]\nmodel = 'm'")).is_err(),
            "a provider without base_url is refused"
        );
    }

    #[test]
    fn saved_addresses_replace_provider_urls_and_allow_their_hosts() {
        // This test names its own providers rather than reading `config/`.
        let mut config = Config::parse_in(
            &configured(
                "default_provider = 'cloud'\n[providers.cloud]\nbase_url = 'https://cloud.test/v1'\n[providers.local]\nbase_url = 'http://127.0.0.1:8080/v1'\nmodels = ['local']\n[net]\negress_allowlist = ['127.0.0.1']\nplain_http_allowlist = ['127.0.0.1']\n",
            ),
            Path::new("/user/cfg/gears"),
        )
        .unwrap();
        let urls = BTreeMap::from([
            (
                "local".to_string(),
                "http://192.168.4.1:8080/v1".to_string(),
            ),
            ("elsewhere".to_string(), "https://other.test/v1".to_string()),
        ]);
        config.apply_urls(&urls).unwrap();
        assert_eq!(
            config.providers["local"].base_url,
            "http://192.168.4.1:8080/v1"
        );
        assert!(config.providers["local"].plain_http);
        assert!(config.egress_allowlist.contains(&"192.168.4.1".to_string()));
        assert!(
            config
                .plain_http_allowlist
                .contains(&"192.168.4.1".to_string())
        );
        assert!(!config.egress_allowlist.contains(&"other.test".to_string()));
        let bad = BTreeMap::from([("local".to_string(), "not a url".to_string())]);
        assert!(config.apply_urls(&bad).is_err());
    }

    #[test]
    fn the_shipped_config_is_found_from_an_executable_in_the_package_target() {
        let root = std::env::temp_dir().join(format!("gears-source-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let package = root.join("gears");
        std::fs::create_dir_all(package.join("config")).unwrap();
        std::fs::create_dir_all(package.join("target/release")).unwrap();
        std::fs::write(package.join("Cargo.toml"), "[package]\nname = \"gears\"\n").unwrap();
        let executable = package.join("target/release/gears");
        assert_eq!(source_tree_of(&executable), None);
        std::fs::write(package.join("config/gears.toml"), "version = 1\n").unwrap();
        assert_eq!(source_tree_of(&executable), Some(package.clone()));
        assert_eq!(
            source_tree_of(&package.join("target/lorry/debug/gears")),
            Some(package.clone())
        );
        assert_eq!(source_tree_of(&root.join("bin/gears")), None);
        std::fs::write(package.join("Cargo.toml"), "[package]\nname = \"other\"\n").unwrap();
        assert_eq!(source_tree_of(&executable), None);
        let error = Config::load_found(&Found::Neither(root.clone()), false).unwrap_err();
        assert!(error.contains("no gears.toml"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unknown_keys_are_refused_in_every_section() {
        for body in [
            "unknown = 1",
            "[after_turn]\ncommand = ['/review']",
            "[provider]\nkey_file = '/secret/key'",
            "[providers.local]\nbase_url = 'https://local.test/v1'\nmodle = 'm'",
            "[runtime]\nsh_timeout_seconds = 5",
            "[net]\negress = ['a.test']",
            "[[tools]]\ndirectory = 'tools/sh'\nenabled = true\nname = 'sh'",
        ] {
            let text = configured(body);
            let error = Config::parse(&text).unwrap_err();
            assert!(error.contains("unknown field"), "{text}: {error}");
        }
    }

    #[test]
    fn the_prompt_settings_are_mandatory_file_names() {
        for (body, field) in [
            (
                "system_prompt_motor_os = 'SYSTEM.md'\n",
                "system_prompt_linux",
            ),
            (
                "system_prompt_linux = 'SYSTEM.md'\n",
                "system_prompt_motor_os",
            ),
        ] {
            let error = Config::parse(&format!("version = 1\n{body}")).unwrap_err();
            assert!(error.contains(field), "{body}: {error}");
        }
        for name in [
            "/etc/prompts.md",
            "prompts/SYSTEM.md",
            "./SYSTEM.md",
            "..",
            "",
        ] {
            let error = Config::parse(&format!(
                "version = 1\nsystem_prompt_linux = '{name}'\nsystem_prompt_motor_os = 'SYSTEM.md'\n"
            ))
            .unwrap_err();
            assert!(error.contains("beside gears.toml"), "{name}: {error}");
            let error = Config::parse(&format!(
                "version = 1\nsystem_prompt_linux = 'SYSTEM.md'\nsystem_prompt_motor_os = '{name}'\n"
            ))
            .unwrap_err();
            assert!(error.contains("beside gears.toml"), "{name}: {error}");
        }
    }

    #[test]
    fn the_build_chooses_the_prompt_for_its_platform() {
        let config = Config::parse(&format!(
            "version = 1\nsystem_prompt_linux = 'LINUX.md'\nsystem_prompt_motor_os = 'MOTOR.md'\n"
        ))
        .unwrap();
        let expected = if cfg!(target_os = "motor") {
            "MOTOR.md"
        } else {
            "LINUX.md"
        };
        assert_eq!(config.system_prompt_file, expected);
        // Nothing is read until the configuration is loaded from a file.
        assert!(config.system_prompt.is_empty());
    }

    #[test]
    fn the_prompt_is_read_from_beside_the_loaded_configuration() {
        let root = std::env::temp_dir().join(format!("gears-prompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("gears.toml");
        std::fs::write(
            &path,
            "version = 1\nsystem_prompt_linux = 'PROMPT.md'\nsystem_prompt_motor_os = 'PROMPT.md'\n",
        )
        .unwrap();
        let error = Config::read_config(&path).unwrap_err();
        assert!(error.contains("PROMPT.md"), "{error}");
        std::fs::write(root.join("PROMPT.md"), " \n").unwrap();
        let error = Config::read_config(&path).unwrap_err();
        assert!(error.contains("is empty"), "{error}");
        std::fs::write(root.join("PROMPT.md"), "Work carefully.\n").unwrap();
        let config = Config::read_config(&path).unwrap().unwrap();
        assert_eq!(config.system_prompt_file, "PROMPT.md");
        assert_eq!(config.system_prompt, "Work carefully.\n");
        assert_eq!(config.source_path.as_deref(), Some(path.as_path()));
        // A prompt elsewhere in the tree is not read.
        std::fs::create_dir_all(root.join("prompts")).unwrap();
        std::fs::write(root.join("prompts/PROMPT.md"), "Elsewhere.\n").unwrap();
        assert_eq!(
            Config::read_config(&path).unwrap().unwrap().system_prompt,
            "Work carefully.\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn named_providers_are_validated_and_have_a_selected_default() {
        let path = Path::new("/tmp/gears-test-config");
        let config = Config::parse_in(
            &configured(
                r#"default_provider = 'local'
default_model = 'small'
[net]
egress_allowlist = ['127.0.0.1', 'openrouter.ai']
plain_http_allowlist = ['127.0.0.1']
[providers.local]
base_url = 'http://127.0.0.1:9090/v1'
api_key_file = 'secrets/local.key'
model = 'local/model'
reasoning_echo = true
[providers.remote]
base_url = 'https://openrouter.ai/api/v1'
reasoning_echo = false
"#,
            ),
            path,
        )
        .unwrap();
        assert_eq!(config.default_provider, "local");
        assert_eq!(config.default_model.as_deref(), Some("small"));
        assert_eq!(config.providers.len(), 2);
        assert!(config.providers["local"].plain_http);
        assert_eq!(config.providers["local"].reasoning_echo, Some(true));
        assert_eq!(config.providers["remote"].reasoning_echo, Some(false));
        assert_eq!(
            config.providers["local"].key_file,
            Some(PathBuf::from("/tmp/gears-test-config/secrets/local.key"))
        );
        assert!(!config.providers["remote"].plain_http);
        assert_eq!(
            config.provider("local").unwrap().model.as_deref(),
            Some("local/model")
        );
        assert!(config.provider("missing").is_none());
        assert!(
            Config::parse(&configured(
                "[providers.one]\nbase_url = 'https://one.test/v1'\n[providers.two]\nbase_url = 'https://two.test/v1'\n"
            ))
            .is_err()
        );
        assert!(Config::parse(&configured("default_provider = 'missing'\n")).is_err());
    }

    #[test]
    fn context_bounds_are_checked() {
        assert!(
            Config::parse(&configured(
                "[context]\nwindow_tokens = 10000\noutput_reserve_tokens = 10000"
            ))
            .is_err()
        );
        assert!(
            Config::parse(&configured("[context]\nwindow_tokens = 4095")).is_err(),
            "a window below the minimum is refused"
        );
        let context = ContextConfig {
            window_tokens: 4096,
            output_reserve_tokens: 1024,
            recent_tail_tokens: 1024,
        };
        assert_eq!(context.validate(), Ok(()));
        assert!(
            ContextConfig {
                window_tokens: 4096,
                output_reserve_tokens: 2048,
                recent_tail_tokens: 2048,
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn interactive_defaults_are_saved_without_losing_the_rest_of_the_file() {
        let root = std::env::temp_dir().join(format!("gears-defaults-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // Settings of this test's own rather than `config/`, whose contents
        // change by hand.
        let shipped = configured(
            "# This comment must survive a saved default.\n[providers.openrouter]\nbase_url = 'https://openrouter.ai/api/v1'\nmodels = ['vendor/model']\n[net]\negress_allowlist = ['openrouter.ai']\n",
        );
        std::fs::write(root.join("gears.toml"), &shipped).unwrap();
        let config = Config::parse_in(&shipped, &root).unwrap();
        config
            .save_model_defaults(&root, "openrouter", "qwen/qwen4-max")
            .unwrap();
        config.save_context_default(&root, 200_000).unwrap();
        let text = std::fs::read_to_string(root.join("gears.toml")).unwrap();
        assert!(
            text.contains("# This comment must survive a saved default."),
            "{text}"
        );
        let saved = Config::parse_in(&text, &root).unwrap();
        assert_eq!(saved.default_provider, "openrouter");
        assert_eq!(saved.default_model.as_deref(), Some("qwen/qwen4-max"));
        assert_eq!(saved.context.window_tokens, 200_000);
        assert_eq!(saved.providers.len(), config.providers.len());
        assert_eq!(saved.egress_allowlist, config.egress_allowlist);
        // A default the file cannot load is refused, leaving the file alone.
        assert!(config.save_model_defaults(&root, "missing", "m").is_err());
        assert!(config.save_context_default(&root, 100).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("gears.toml")).unwrap(),
            text
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_missing_user_config_is_seeded_from_the_loaded_file() {
        let root = std::env::temp_dir().join(format!("gears-defaults-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let text = configured("[provider]\nbase_url = 'https://one.test/v1'\n");
        let source = root.join("loaded.toml");
        std::fs::write(&source, &text).unwrap();
        let mut config = Config::parse(&text).unwrap();
        let directory = root.join("fresh");
        let error = config.save_context_default(&directory, 64_000).unwrap_err();
        assert!(error.contains("not found"), "{error}");
        config.source_path = Some(source);
        // A seeded gears.toml needs the prompt it names beside it, so this run's
        // prompt is written first.
        config.system_prompt = "You are a test agent.\n".to_string();
        config.save_context_default(&directory, 64_000).unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.join("SYSTEM.md")).unwrap(),
            "You are a test agent.\n"
        );
        let saved = Config::parse_in(
            &std::fs::read_to_string(directory.join("gears.toml")).unwrap(),
            &directory,
        )
        .unwrap();
        assert_eq!(saved.context.window_tokens, 64_000);
        assert!(saved.provider("default").is_some());
        std::fs::remove_dir_all(root).unwrap();
    }
}
