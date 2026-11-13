use super::*;
use codingbuddy_core::{LlmConfig, Session, SessionBudgets, SessionState};
use codingbuddy_ui::{TerminalMenu, TuiStreamEvent};
use std::sync::{Arc, Mutex, mpsc};

const PROVIDERS: &[(&str, &str)] = &[
    ("openai-compatible", "OpenAI"),
    ("anthropic", "Anthropic"),
    ("google", "Google Gemini"),
    ("deepseek", "DeepSeek"),
    ("openrouter", "OpenRouter"),
    ("ollama", "Ollama"),
];

pub(super) fn detect(llm: &LlmConfig) -> Vec<String> {
    PROVIDERS
        .iter()
        .filter_map(|(id, _)| {
            let mut candidate = llm.clone();
            candidate.select_provider(id);
            (codingbuddy_llm::provider_requires_key(&candidate)
                && codingbuddy_llm::check_provider(&candidate).is_ok())
            .then(|| (*id).to_owned())
        })
        .collect()
}

pub(super) struct Connection {
    pub llm: LlmConfig,
    pub ready: bool,
    pub allow_history_transfer: bool,
    configured: bool,
    pending: Option<LlmConfig>,
    connections: std::collections::HashMap<String, LlmConfig>,
    history_choice: Option<LlmConfig>,
    generation: u64,
    save_key: bool,
}

impl Connection {
    pub fn new(llm: LlmConfig, ready: bool, configured: bool) -> Self {
        let connections = [(llm.provider.clone(), llm.clone())].into();
        Self {
            llm,
            ready,
            configured,
            allow_history_transfer: false,
            pending: None,
            connections,
            history_choice: None,
            generation: 0,
            save_key: false,
        }
    }
    fn candidate(&self, provider: &str) -> Result<LlmConfig> {
        if let Some(connection) = self.connections.get(provider) {
            let mut connection = connection.clone();
            connection.providers.extend(
                self.llm
                    .providers
                    .iter()
                    .filter(|(id, _)| id.as_str() != provider)
                    .map(|(id, profile)| (id.clone(), profile.clone())),
            );
            return Ok(connection);
        }
        candidate(&self.llm, provider)
    }
}

pub(super) struct Menus {
    pub cwd: PathBuf,
    pub state: Arc<Mutex<Connection>>,
    pub session: Arc<Mutex<Option<Uuid>>>,
    pub chat_mode: Arc<Mutex<ChatMode>>,
    pub read_only: Arc<std::sync::atomic::AtomicBool>,
    pub allow_tools: bool,
    pub tx: mpsc::Sender<TuiStreamEvent>,
}

impl Menus {
    fn menu(&self, menu: TerminalMenu) {
        let _ = self.tx.send(TuiStreamEvent::Menu(Some(menu)));
    }

    fn notify(&self, message: String, error: bool) {
        let _ = self.tx.send(TuiStreamEvent::SystemNotice {
            line: message,
            error,
        });
    }

    pub fn action(&self, id: &str, value: String) {
        if let Err(error) = self.handle(id, value) {
            self.notify(error.to_string(), true);
            let _ = self.tx.send(TuiStreamEvent::Menu(None));
        }
    }

    fn handle(&self, id: &str, value: String) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("Connection state unavailable"))?;
        state.generation += 1;
        match id {
            "cancel" | "cancel-close" => {
                state.allow_history_transfer = false;
                let _ = self.tx.send(TuiStreamEvent::Menu(None));
                state.pending = None;
                state.history_choice = None;
                state.save_key = false;
            }
            "commands" => self.menu(TerminalMenu::choices(
                "Commands",
                "Type to find a command",
                choices(&[
                    ("connect", "/connect   Manage providers"),
                    ("models", "/models    Choose a model"),
                    ("settings", "/settings  Defaults and theme"),
                    ("ask", "/ask       Read-only questions"),
                    ("code", "/code      Coding tools"),
                    ("remove-key", "/logout    Remove saved key"),
                ]),
            )),
            "ask" | "code" => {
                let mode = if id == "ask" {
                    ChatMode::Ask
                } else {
                    ChatMode::Code
                };
                if id == "code" {
                    self.check_model(&state.llm, true)?;
                }
                *self.chat_mode.lock().unwrap() = mode;
                let _ = self.tx.send(TuiStreamEvent::Menu(None));
                self.notify(format!("Mode set to {id}"), false);
            }
            "connect" => {
                let mut entries: Vec<_> = PROVIDERS
                    .iter()
                    .map(|(id, name)| {
                        let active = if state.ready && state.llm.provider == *id {
                            "  • active"
                        } else {
                            ""
                        };
                        (format!("provider:{id}"), format!("{name}{active}"))
                    })
                    .collect();
                let mut custom: Vec<_> = state
                    .llm
                    .providers
                    .iter()
                    .filter(|(id, provider)| {
                        !PROVIDERS.iter().any(|(known, _)| known == id)
                            && matches!(
                                provider.kind.as_str(),
                                "openai-compatible" | "openai-compat"
                            )
                    })
                    .map(|(id, _)| (format!("provider:{id}"), id.clone()))
                    .collect();
                custom.sort();
                entries.extend(custom);
                entries.extend(choices(&[
                    ("custom-id", "Add an OpenAI-compatible endpoint"),
                    ("remove-key", "Remove the active provider’s saved key"),
                ]));
                self.menu(TerminalMenu::choices(
                    "Connect a provider",
                    "Select a provider or add an endpoint",
                    entries,
                ));
            }
            "custom-id" => self.menu(TerminalMenu::field(
                "Custom endpoint",
                "Enter a new profile name",
                "custom-url",
                false,
            )),
            "custom-url" => {
                let name = value.trim();
                anyhow::ensure!(
                    !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')),
                    "Use letters, digits, hyphens, or underscores for the profile name"
                );
                anyhow::ensure!(
                    !state.llm.providers.contains_key(name),
                    "A profile with this name already exists"
                );
                let mut llm = state.llm.clone();
                llm.select_provider(name);
                llm.providers.insert(
                    name.into(),
                    codingbuddy_core::ProviderConfig {
                        kind: "openai-compatible".into(),
                        base_url: String::new(),
                        api_key_env: format!(
                            "CODINGBUDDY_{}_API_KEY",
                            name.replace('-', "_").to_ascii_uppercase()
                        ),
                        openai_compat_prefix: false,
                        chat_protocol: None,
                        auth_strategy: None,
                        headers: Default::default(),
                        discovery: Default::default(),
                        payload_options: Default::default(),
                        models: codingbuddy_core::ProviderModels {
                            chat: String::new(),
                            reasoner: None,
                        },
                    },
                );
                state.pending = Some(llm);
                self.menu(TerminalMenu::field(
                    "Custom endpoint",
                    "Enter API base URL, including /v1 if required",
                    "custom-auth",
                    false,
                ));
            }
            "custom-auth" => {
                let url = codingbuddy_llm::validate_base_url(value.trim())?;
                let llm = state
                    .pending
                    .as_mut()
                    .ok_or_else(|| anyhow!("Choose a provider first"))?;
                llm.providers.get_mut(&llm.provider).unwrap().base_url = url;
                self.menu(TerminalMenu::choices(
                    "Authentication",
                    "Choose how the endpoint authenticates",
                    choices(&[
                        ("custom-key", "API key (Bearer)"),
                        ("custom-none", "No authentication"),
                    ]),
                ));
            }
            "custom-key" | "custom-none" => {
                let mut llm = state
                    .pending
                    .take()
                    .ok_or_else(|| anyhow!("Choose a provider first"))?;
                if id == "custom-none" {
                    let profile = llm.providers.get_mut(&llm.provider).unwrap();
                    profile.api_key_env.clear();
                    profile.auth_strategy = Some("none".into());
                }
                self.begin(&mut state, llm);
            }
            "provider-id" => {
                let llm = state.candidate(&value)?;
                self.begin(&mut state, llm);
            }
            "models" => {
                state.save_key = false;
                let llm = state.llm.clone();
                if state.ready {
                    self.discover(&mut state, llm);
                } else {
                    drop(state);
                    return self.handle("connect", String::new());
                }
            }
            "paste-key" => self.menu(TerminalMenu::field(
                "API key",
                "Paste your key. Input is hidden.",
                "key-entered",
                true,
            )),
            "key-entered" => {
                anyhow::ensure!(!value.trim().is_empty(), "The API key is empty");
                let llm = state
                    .pending
                    .as_mut()
                    .ok_or_else(|| anyhow!("Choose a provider first"))?;
                llm.api_key = Some(value.trim().into());
                self.menu(TerminalMenu::choices(
                    "Save API key?",
                    "Use your system credential store",
                    choices(&[
                        ("key-save", "Save securely"),
                        ("key-once", "Use for this session"),
                    ]),
                ));
            }
            "key-save" | "key-once" => {
                let llm = state
                    .pending
                    .take()
                    .ok_or_else(|| anyhow!("Choose a provider first"))?;
                state.save_key = id == "key-save";
                self.discover(&mut state, llm);
            }
            "key-use" => {
                let llm = state
                    .pending
                    .take()
                    .ok_or_else(|| anyhow!("Choose a provider first"))?;
                self.discover(&mut state, llm);
            }
            "remove-key" => {
                codingbuddy_llm::remove_provider_key(&state.llm)?;
                state.llm.api_key = None;
                let active = state.llm.clone();
                state.connections.insert(active.provider.clone(), active);
                state.ready = codingbuddy_llm::check_provider(&state.llm).is_ok();
                self.changed(&state, None);
                self.notify(
                    if state.ready {
                        "Saved key removed. Another credential source is available."
                    } else {
                        "Saved key removed."
                    }
                    .into(),
                    false,
                );
            }
            "model-manual" => self.menu(TerminalMenu::field(
                "Model ID",
                "Enter the model ID used by this provider",
                "model-entered",
                false,
            )),
            "model-entered" => {
                anyhow::ensure!(!value.trim().is_empty(), "The model ID is empty");
                let mut llm = state
                    .pending
                    .clone()
                    .ok_or_else(|| anyhow!("Choose a provider first"))?;
                set_model(&mut llm, value.trim());
                self.verify(&mut state, llm)?;
            }
            "model-spec" => {
                if is_max_think_selection(&value) {
                    let mut llm = state.llm.clone();
                    let model = llm.active_reasoner_model();
                    set_model(&mut llm, &model);
                    self.verify(&mut state, llm)?;
                    return Ok(());
                }
                anyhow::ensure!(!value.trim().is_empty(), "The model ID is empty");
                let mut llm = if let Some((provider, _)) = value.split_once('/')
                    && state.llm.providers.contains_key(provider)
                {
                    state.candidate(provider)?
                } else {
                    state.llm.clone()
                };
                llm.apply_model_spec(&value);
                self.verify(&mut state, llm)?;
            }
            "settings" => self.menu(TerminalMenu::choices(
                "Settings",
                "Changes apply to future launches",
                choices(&[
                    ("default", "Use active provider and model by default"),
                    ("theme:auto", "Theme · System"),
                    ("theme:dark", "Theme · Dark"),
                    ("theme:light", "Theme · Light"),
                ]),
            )),
            "default" => {
                anyhow::ensure!(state.ready, "Connect a provider first");
                save_default(&state.llm)?;
                state.configured = true;
                self.changed(&state, None);
                self.notify("Default provider and model saved.".into(), false);
            }
            "history-new" | "history-share" => {
                let llm = state
                    .history_choice
                    .take()
                    .ok_or_else(|| anyhow!("Choose a model first"))?;
                if id == "history-new" {
                    *self.session.lock().unwrap() = Some(self.create_session()?);
                } else {
                    // The agent records the chosen destination when the next turn starts.
                    state.allow_history_transfer = true;
                }
                self.activate(&mut state, llm)?;
            }
            id if id.starts_with("theme:") => {
                let theme = &id["theme:".len()..];
                update_settings(|settings| settings["theme"]["mode"] = json!(theme))?;
                self.changed(&state, Some(theme.into()));
            }
            id if id.starts_with("provider:") => {
                let llm = state.candidate(&id["provider:".len()..])?;
                self.begin(&mut state, llm);
            }
            id if id.starts_with("model:") => {
                let mut llm = state
                    .pending
                    .clone()
                    .ok_or_else(|| anyhow!("Choose a provider first"))?;
                set_model(&mut llm, &id["model:".len()..]);
                self.verify(&mut state, llm)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn check_model(&self, llm: &LlmConfig, code: bool) -> Result<()> {
        if code
            && let Some(message) = model_switch_safety_message(
                &self.cwd,
                &AppConfig {
                    llm: llm.clone(),
                    ..Default::default()
                },
                &format!("{}/{}", llm.provider, llm.active_base_model()),
                true,
                false,
                ChatMode::Code,
            )
        {
            return Err(anyhow!(message));
        }
        Ok(())
    }

    fn begin(&self, state: &mut Connection, llm: LlmConfig) {
        state.save_key = false;
        if codingbuddy_llm::provider_requires_key(&llm) {
            let mut entries = Vec::new();
            if codingbuddy_llm::resolve_key_from_config(&llm).is_some() {
                entries.extend(choices(&[("key-use", "Use available credential")]));
            }
            entries.extend(choices(&[("paste-key", "Paste an API key")]));
            let hint = format!("Credential: {}", llm.active_provider().api_key_env);
            state.pending = Some(llm);
            self.menu(TerminalMenu::choices("API key", &hint, entries));
        } else {
            self.discover(state, llm);
        }
    }

    fn discover(&self, state: &mut Connection, llm: LlmConfig) {
        let generation = state.generation;
        let shared = Arc::clone(&self.state);
        let tx = self.tx.clone();
        let cwd = self.cwd.clone();
        let code = self.allow_tools
            && !self.read_only.load(std::sync::atomic::Ordering::Relaxed)
            && *self.chat_mode.lock().unwrap() == ChatMode::Code;
        state.pending = Some(llm.clone());
        let menu = models_menu(&cwd, &llm, &[], code, "Discovering available models…");
        let _ = self.tx.send(TuiStreamEvent::Menu(Some(menu)));
        thread::spawn(move || {
            let ids = codingbuddy_llm::ApiClient::new(llm.clone())
                .map(|client| codingbuddy_llm::LlmClient::list_models(&client))
                .unwrap_or_default();
            let menu = models_menu(
                &cwd,
                &llm,
                &ids,
                code,
                if ids.is_empty() {
                    "Using cached and configured choices"
                } else {
                    "Select a model or enter its ID"
                },
            );
            if let Ok(state) = shared.lock()
                && state.generation == generation
            {
                let _ = tx.send(TuiStreamEvent::Menu(Some(menu)));
            }
        });
    }

    fn verify(&self, state: &mut Connection, llm: LlmConfig) -> Result<()> {
        let code = self.allow_tools
            && !self.read_only.load(std::sync::atomic::Ordering::Relaxed)
            && *self.chat_mode.lock().unwrap() == ChatMode::Code;
        self.check_model(&llm, code)?;
        if state.ready
            && state.llm.connection_id() == llm.connection_id()
            && state.llm.active_base_model() == llm.active_base_model()
            && !state.save_key
            && state.llm.api_key == llm.api_key
        {
            return self.select(state, llm);
        }
        let generation = state.generation;
        let controller = Self {
            cwd: self.cwd.clone(),
            state: Arc::clone(&self.state),
            session: Arc::clone(&self.session),
            chat_mode: Arc::clone(&self.chat_mode),
            read_only: Arc::clone(&self.read_only),
            allow_tools: self.allow_tools,
            tx: self.tx.clone(),
        };
        self.menu(TerminalMenu::choices(
            "Checking connection",
            "Verifying model access…",
            Vec::new(),
        ));
        thread::spawn(move || {
            let result = codingbuddy_llm::probe_provider(&llm);
            if let Ok(mut state) = controller.state.lock()
                && state.generation == generation
            {
                let result = result.and_then(|()| controller.select(&mut state, llm));
                if let Err(error) = result {
                    controller.notify(error.to_string(), true);
                    let _ = controller.tx.send(TuiStreamEvent::Menu(None));
                }
            }
        });
        Ok(())
    }

    fn select(&self, state: &mut Connection, llm: LlmConfig) -> Result<()> {
        let session = *self.session.lock().unwrap();
        let needs_choice = if let Some(session) = session {
            let history = Store::new(&self.cwd)?.rebuild_from_events(session)?;
            !history.chat_messages.is_empty()
                && history.provider_selection.as_ref().map_or_else(
                    || state.llm.connection_id() != llm.connection_id(),
                    |(provider, _, destination)| {
                        provider != &llm.provider
                            || destination
                                .as_ref()
                                .is_some_and(|id| id != &llm.connection_id())
                    },
                )
        } else {
            false
        };
        if needs_choice {
            state.history_choice = Some(llm);
            self.history_menu();
        } else {
            self.activate(state, llm)?;
        }
        Ok(())
    }

    pub fn history_menu(&self) {
        self.menu(TerminalMenu::choices(
            "Change conversation provider",
            "Continue sends previous messages to this provider.",
            choices(&[
                ("history-new", "Start fresh"),
                ("history-share", "Continue and share history"),
                ("cancel-close", "Cancel"),
            ]),
        ));
    }

    pub fn ensure_session(&self) -> Result<Uuid> {
        let mut session = self.session.lock().unwrap();
        if let Some(id) = *session {
            return Ok(id);
        }
        let id = self.create_session()?;
        *session = Some(id);
        Ok(id)
    }

    fn create_session(&self) -> Result<Uuid> {
        let cfg = AppConfig::load(&self.cwd)?;
        let session = Session {
            session_id: Uuid::now_v7(),
            workspace_root: self.cwd.display().to_string(),
            baseline_commit: None,
            status: SessionState::Idle,
            active_plan_id: None,
            budgets: SessionBudgets {
                per_turn_seconds: cfg.budgets.max_turn_duration_secs,
                max_think_tokens: cfg.budgets.max_reasoner_tokens_per_session as u32,
            },
        };
        Store::new(&self.cwd)?.save_session(&session)?;
        Ok(session.session_id)
    }

    pub fn review_resume(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state.allow_history_transfer = false;
        if let Some(session) = *self.session.lock().unwrap() {
            let projection = Store::new(&self.cwd)?.rebuild_from_events(session)?;
            if !projection.chat_messages.is_empty()
                && projection.provider_selection.as_ref().is_some_and(
                    |(provider, _, destination)| {
                        provider != &state.llm.provider
                            || destination
                                .as_ref()
                                .is_some_and(|id| id != &state.llm.connection_id())
                    },
                )
            {
                state.history_choice = Some(state.llm.clone());
                self.history_menu();
            }
        }
        Ok(())
    }

    fn activate(&self, state: &mut Connection, llm: LlmConfig) -> Result<()> {
        if std::mem::take(&mut state.save_key)
            && let Some(key) = llm.api_key.as_deref()
            && codingbuddy_llm::save_provider_key(&llm, key).is_err()
        {
            self.notify(
                "Credential store unavailable. Using the key for this session.".into(),
                false,
            );
        }
        if state.configured
            && !AppConfig::load(&self.cwd)?
                .llm
                .providers
                .contains_key(&llm.provider)
        {
            let mut profile = llm.active_provider();
            profile.headers.clear();
            update_settings(|settings| {
                settings["llm"]["providers"][&llm.provider] = json!(profile)
            })?;
        }
        if !state.configured {
            if let Err(error) = save_default(&llm) {
                self.notify(
                    format!("Connected for this session. Could not save the default: {error}"),
                    false,
                );
            } else {
                state.configured = true;
            }
        }
        state.connections.insert(llm.provider.clone(), llm.clone());
        state.llm = llm;
        state.ready = true;
        state.pending = None;
        self.changed(state, None);
        Ok(())
    }

    fn changed(&self, state: &Connection, theme: Option<String>) {
        let _ = self.tx.send(TuiStreamEvent::Menu(None));
        let _ = self.tx.send(TuiStreamEvent::ConnectionChanged {
            ready: state.ready,
            theme,
        });
    }
}

fn choices(entries: &[(&str, &str)]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|(id, label)| ((*id).into(), (*label).into()))
        .collect()
}

fn candidate(active: &LlmConfig, provider: &str) -> Result<LlmConfig> {
    anyhow::ensure!(
        active.providers.contains_key(provider),
        "Unknown provider {provider}"
    );
    let mut llm = active.clone();
    llm.select_provider(provider);
    if provider == "ollama"
        && let Ok(host) = std::env::var("OLLAMA_HOST")
        && !host.trim().is_empty()
    {
        let host = host.trim();
        let url = if host.contains("://") {
            host.to_owned()
        } else {
            format!("http://{host}")
        };
        let url = codingbuddy_llm::validate_base_url(&url)?;
        let profile = llm.providers.get_mut(provider).unwrap();
        if profile.base_url != url {
            profile.base_url = url;
            llm.endpoint.clear();
            llm.api_key = None;
        }
    }
    Ok(llm)
}

fn set_model(llm: &mut LlmConfig, model: &str) {
    llm.base_model = model.into();
    if let Some(provider) = llm.providers.get_mut(&llm.provider) {
        provider.models.chat = model.into();
    }
}

fn models_menu(
    cwd: &Path,
    llm: &LlmConfig,
    ids: &[String],
    code: bool,
    hint: &str,
) -> TerminalMenu {
    let catalog = llm.model_catalog_for_runtime(&runtime_dir(cwd));
    let items = catalog.selector_items(llm);
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in items.iter().filter(|item| item.provider == llm.provider) {
        if (llm.provider != "ollama" || ids.is_empty() || ids.contains(&item.id))
            && (!code || item.capability.tool_call)
        {
            let active = if item.active { " • active" } else { "" };
            entries.push((format!("model:{}", item.id), format!("{}{active}", item.id)));
        }
        seen.insert(item.id.clone());
    }
    for id in ids {
        if seen.insert(id.clone()) {
            entries.push((format!("model:{id}"), id.clone()));
        }
    }
    entries.extend(choices(&[
        ("model-manual", "Enter model ID manually"),
        ("connect", "Choose a different provider"),
    ]));
    TerminalMenu::choices(&format!("Models · {}", llm.provider), hint, entries)
}

fn save_default(llm: &LlmConfig) -> Result<()> {
    let mut profile = llm.active_provider();
    let provider = llm.provider.clone();
    // Top-level endpoint belongs to this profile and contains no credentials.
    let endpoint = llm.endpoint.clone();
    let model = llm.active_base_model();
    profile.models.chat = model;
    profile.headers.clear();
    update_settings(move |settings| {
        let previous = settings
            .pointer("/llm/provider")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("deepseek");
        let previous_endpoint = settings
            .pointer("/llm/endpoint")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let previous_base = settings["llm"]["providers"][&provider]["base_url"]
            .as_str()
            .or_else(|| {
                settings
                    .pointer("/llm/base_url")
                    .and_then(serde_json::Value::as_str)
            });
        let destination_changed = previous != provider
            || previous_endpoint != endpoint
            || previous_base.is_some_and(|base| base != profile.base_url);
        if destination_changed
            && let Some(llm) = settings
                .get_mut("llm")
                .and_then(serde_json::Value::as_object_mut)
        {
            llm.remove("api_key");
        }
        settings["llm"]["provider"] = json!(provider);
        settings["llm"]["providers"][&provider] = json!(profile);
        settings["llm"]["endpoint"] = json!(endpoint);
    })
}

fn update_settings(update: impl FnOnce(&mut serde_json::Value)) -> Result<()> {
    use std::io::Write;
    let path =
        AppConfig::user_settings_path().ok_or_else(|| anyhow!("Cannot locate user settings"))?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Invalid settings path"))?;
    std::fs::create_dir_all(parent)?;
    let mut settings = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(settings.is_object(), "User settings must be a JSON object");
    for key in ["llm", "theme"] {
        anyhow::ensure!(
            settings.get(key).is_none_or(serde_json::Value::is_object),
            "Invalid {key} settings"
        );
    }
    anyhow::ensure!(
        settings
            .pointer("/llm/providers")
            .is_none_or(serde_json::Value::is_object),
        "Invalid provider settings"
    );
    update(&mut settings);
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut file, &settings)?;
    file.flush()?;
    file.persist(path)?;
    Ok(())
}
