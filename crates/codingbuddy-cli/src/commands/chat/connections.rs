use super::*;
use codingbuddy_core::{LlmConfig, Session, SessionBudgets, SessionState};
use codingbuddy_ui::{TerminalMenu, TuiStreamEvent};
use std::sync::{Arc, Mutex};

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
    configured: bool,
    pending: Option<LlmConfig>,
    connections: std::collections::HashMap<String, LlmConfig>,
    history_choice: Option<LlmConfig>,
    history_preview: Option<(Uuid, Vec<codingbuddy_core::ChatMessage>)>,
    generation: u64,
}

impl Connection {
    pub fn new(llm: LlmConfig, ready: bool, configured: bool) -> Self {
        let connections = [(llm.provider.clone(), llm.clone())].into();
        Self {
            llm,
            ready,
            configured,
            pending: None,
            connections,
            history_choice: None,
            history_preview: None,
            generation: 0,
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
    pub tx: codingbuddy_ui::StreamSender,
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
                let _ = self.tx.send(TuiStreamEvent::Menu(None));
                state.pending = None;
                state.history_choice = None;
                state.history_preview = None;
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
                    ("remove-key", "/logout    Remove default saved key"),
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
                *self
                    .chat_mode
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = mode;
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
                    (
                        "remove-key",
                        "Remove the active provider’s default saved key",
                    ),
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
                        account_id: None,
                        cloud: None,
                        gateway: None,
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
                llm.providers
                    .get_mut(&llm.provider)
                    .ok_or_else(|| anyhow!("Provider `{}` is not configured", llm.provider))?
                    .base_url = url;
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
                    let provider_id = llm.provider.clone();
                    let profile = llm
                        .providers
                        .get_mut(&provider_id)
                        .ok_or_else(|| anyhow!("Provider `{provider_id}` is not configured"))?;
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
                let llm = state.llm.clone();
                if state.ready {
                    self.discover(&mut state, llm);
                } else {
                    drop(state);
                    return self.handle("connect", String::new());
                }
            }
            "paste-key" => self.prompt_key(),
            "key-entered" | "key-entered-save" => {
                anyhow::ensure!(!value.trim().is_empty(), "The API key is empty");
                let llm = state
                    .pending
                    .as_mut()
                    .ok_or_else(|| anyhow!("Choose a provider first"))?;
                llm.api_key = Some(value.trim().into());
                if id == "key-entered-save" {
                    self.prompt_key_name("save-key-and-connect");
                } else {
                    let llm = llm.clone();
                    self.connect_with_key(&mut state, llm);
                }
            }
            "save-key" => self.prompt_key_name("save-key-only"),
            "save-key-only" | "save-key-and-connect" => {
                let llm = state
                    .pending
                    .clone()
                    .ok_or_else(|| anyhow!("Choose a provider first"))?;
                let key = codingbuddy_llm::resolve_key_from_config(&llm)
                    .ok_or_else(|| anyhow!("Enter an API key first"))?;
                let name = value.trim();
                // Make a new custom endpoint discoverable on the next launch.
                if !AppConfig::load(&self.cwd)?
                    .llm
                    .providers
                    .contains_key(&llm.provider)
                {
                    let mut profile = llm.active_provider();
                    profile.headers.clear();
                    update_settings(|settings| {
                        settings["llm"]["providers"][&llm.provider] = json!(profile);
                    })?;
                }
                let result = if name.is_empty() {
                    codingbuddy_llm::save_provider_key(&llm, &key)
                } else {
                    codingbuddy_llm::save_named_provider_key(&llm, name, &key)
                };
                if let Err(error) = result {
                    self.notify(format!("Could not save the API key. {error}"), true);
                    self.prompt_key_name(id);
                } else {
                    self.notify("API key saved.".into(), false);
                    if id == "save-key-and-connect" {
                        self.connect_with_key(&mut state, llm);
                    } else {
                        self.menu(TerminalMenu::actions(
                            "API key saved",
                            "Retry when the provider is ready.",
                            choices(&[
                                ("retry-connection", "Retry connection"),
                                ("retry-models", "Choose another model"),
                                ("cancel", "Return to chat"),
                            ]),
                        ));
                    }
                }
            }
            id if id.starts_with("named-key:") => {
                let mut llm = state
                    .pending
                    .clone()
                    .ok_or_else(|| anyhow!("Choose a provider first"))?;
                llm.api_key = Some(codingbuddy_llm::load_named_provider_key(
                    &llm,
                    &id["named-key:".len()..],
                )?);
                self.connect_with_key(&mut state, llm);
            }
            "retry-connection" => {
                let llm = state
                    .pending
                    .clone()
                    .ok_or_else(|| anyhow!("Choose a provider first"))?;
                self.verify(&mut state, llm)?;
            }
            "key-use" | "retry-models" => {
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
                        "Default saved key removed. Another credential source is available."
                    } else {
                        "Default saved key removed."
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
            "history-preview" => {
                let source = (*self
                    .session
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner))
                .ok_or_else(|| anyhow!("No source session"))?;
                let store = Store::new(&self.cwd)?;
                let _lease = store.session_lease(source)?;
                let history =
                    crate::commands::session::portable_history(&self.cwd, &store, source)?;
                for message in &history {
                    match message {
                        codingbuddy_core::ChatMessage::User { content } => {
                            self.notify(format!("User — {content}"), false)
                        }
                        codingbuddy_core::ChatMessage::Assistant {
                            content: Some(content),
                            ..
                        } => self.notify(format!("Assistant — {content}"), false),
                        _ => {}
                    }
                }
                state.history_preview = Some((source, history));
                self.menu(TerminalMenu::choices(
                    "Confirm redacted fork",
                    "Review the history in the transcript before continuing.",
                    choices(&[
                        ("history-share", "Create linked fork"),
                        ("cancel-close", "Cancel"),
                    ]),
                ));
            }
            "history-new" | "history-share" => {
                let llm = state
                    .history_choice
                    .clone()
                    .ok_or_else(|| anyhow!("Choose a model first"))?;
                let selected = if id == "history-new" {
                    self.create_session()?
                } else {
                    let (source, preview) = state
                        .history_preview
                        .take()
                        .ok_or_else(|| anyhow!("Preview the redacted history first"))?;
                    let store = Store::new(&self.cwd)?;
                    let _lease = store.session_lease(source)?;
                    let history =
                        crate::commands::session::portable_history(&self.cwd, &store, source)?;
                    anyhow::ensure!(
                        serde_json::to_value(&preview)? == serde_json::to_value(&history)?,
                        "History changed. Preview the redacted history again."
                    );
                    store
                        .fork_session_with_messages(source, &history)?
                        .session_id
                };
                *self
                    .session
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(selected);
                state.history_choice = None;
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

    fn prompt_key(&self) {
        self.menu(
            TerminalMenu::field(
                "API key",
                "Paste your key. Input is hidden.",
                "key-entered",
                true,
            )
            .with_save_action("key-entered-save"),
        );
    }

    fn begin(&self, state: &mut Connection, llm: LlmConfig) {
        if codingbuddy_llm::provider_requires_key(&llm) {
            let has_key = codingbuddy_llm::resolve_key_from_config(&llm).is_some();
            let mut entries = Vec::new();
            if has_key {
                entries.extend(choices(&[("key-use", "Use available credential")]));
            }
            match codingbuddy_llm::provider_key_names(&llm) {
                Ok(names) => entries.extend(
                    names
                        .into_iter()
                        .map(|name| (format!("named-key:{name}"), name)),
                ),
                Err(error) => self.notify(format!("Could not list named API keys. {error}"), true),
            }
            state.pending = Some(llm);
            if entries.is_empty() {
                self.prompt_key();
            } else {
                entries.extend(choices(&[("paste-key", "Enter a different API key")]));
                self.menu(TerminalMenu::choices(
                    "Credential options",
                    "Choose a saved key or enter another key.",
                    entries,
                ));
            }
        } else {
            self.discover(state, llm);
        }
    }

    fn prompt_key_name(&self, action: &str) {
        self.menu(TerminalMenu::field(
            "Name this API key",
            "Use a unique name, or leave blank to replace the default saved key.",
            action,
            false,
        ));
    }

    fn connect_with_key(&self, state: &mut Connection, llm: LlmConfig) {
        if llm.active_base_model().trim().is_empty() {
            self.discover(state, llm);
        } else if let Err(error) = self.verify(state, llm.clone()) {
            self.notify(error.to_string(), true);
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
            && *self
                .chat_mode
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                == ChatMode::Code;
        state.pending = Some(llm.clone());
        let menu = models_menu(&cwd, &llm, &[], code, "Discovering available models…");
        let _ = self.tx.send(TuiStreamEvent::Menu(Some(menu)));
        codingbuddy_core::runtime::runtime().spawn(async move {
            let ids = match codingbuddy_llm::ApiClient::new(llm.clone()) {
                Ok(client) => codingbuddy_llm::LlmClient::list_models(&client).await,
                Err(_) => Vec::new(),
            };
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
            && *self
                .chat_mode
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                == ChatMode::Code;
        if let Err(error) = self.check_model(&llm, code) {
            self.show_connection_failure(state, llm, &error);
            return Ok(());
        }
        if state.ready
            && state.llm.connection_id() == llm.connection_id()
            && state.llm.active_base_model() == llm.active_base_model()
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
            &format!("Verifying {}/{}…", llm.provider, llm.active_base_model()),
            Vec::new(),
        ));
        codingbuddy_core::runtime::runtime().spawn(async move {
            let result = codingbuddy_llm::probe_provider(&llm).await;
            if result.is_ok() {
                let _ = codingbuddy_store::Store::new(&controller.cwd)
                    .and_then(|store| store.route_key())
                    .and_then(|key| {
                        codingbuddy_llm::certification::record_connectivity(
                            &llm,
                            &controller.cwd,
                            &key,
                        )
                    });
            }
            if let Ok(mut state) = controller.state.lock()
                && state.generation == generation
            {
                if let Err(error) = result {
                    controller.show_connection_failure(&mut state, llm, &error);
                } else if let Err(error) = controller.select(&mut state, llm) {
                    controller.notify(error.to_string(), true);
                    let _ = controller.tx.send(TuiStreamEvent::Menu(None));
                }
            }
        });
        Ok(())
    }

    fn show_connection_failure(
        &self,
        state: &mut Connection,
        llm: LlmConfig,
        error: &anyhow::Error,
    ) {
        let can_save = codingbuddy_llm::provider_requires_key(&llm)
            && codingbuddy_llm::resolve_key_from_config(&llm).is_some();
        let menu = connection_failure_menu(&llm, error, can_save);
        self.notify(error.to_string(), true);
        state.pending = Some(llm);
        self.menu(menu);
    }

    fn select(&self, state: &mut Connection, llm: LlmConfig) -> Result<()> {
        let session = *self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let needs_choice = if let Some(session) = session {
            let history = Store::new(&self.cwd)?.rebuild_from_events(session)?;
            !history.chat_messages.is_empty()
                && history.provider_selection.as_ref().map_or_else(
                    || state.llm.connection_id() != llm.connection_id(),
                    |(provider, model, destination)| {
                        provider != &llm.provider
                            || model != &llm.active_base_model()
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
            "Choose a fresh session or preview a linked fork.",
            choices(&[
                ("history-new", "Start fresh"),
                ("history-preview", "Preview redacted history"),
                ("cancel-close", "Cancel"),
            ]),
        ));
    }

    pub fn ensure_session(&self) -> Result<Uuid> {
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(session) = *self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            let projection = Store::new(&self.cwd)?.rebuild_from_events(session)?;
            if !projection.chat_messages.is_empty()
                && projection.provider_selection.as_ref().is_some_and(
                    |(provider, model, destination)| {
                        provider != &state.llm.provider
                            || model != &state.llm.active_base_model()
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
        let verification = codingbuddy_store::Store::new(&self.cwd)
            .and_then(|store| store.route_key())
            .map(|key| {
                codingbuddy_llm::certification::live_status(
                    &state.llm,
                    &state.llm.active_base_model(),
                    &self.cwd,
                    &key,
                )
            })
            .unwrap_or_else(|_| {
                codingbuddy_llm::certification::status(&state.llm, &state.llm.active_base_model())
            });
        self.notify(
            format!(
                "Provider verification — fixtures {}, live {}",
                verification["fixture_verified"], verification["live_verified"]
            ),
            false,
        );
        self.notify(
            format!(
                "Connected to {}/{}.",
                state.llm.provider,
                state.llm.active_base_model()
            ),
            false,
        );
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
        let profile = llm
            .providers
            .get_mut(provider)
            .ok_or_else(|| anyhow!("Provider `{provider}` is not configured"))?;
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

fn connection_failure_menu(llm: &LlmConfig, error: &anyhow::Error, can_save: bool) -> TerminalMenu {
    use codingbuddy_llm::{ConnectionError, error::ProviderError};
    let status = error
        .downcast_ref::<ProviderError>()
        .and_then(|error| error.http_status);
    let choose_model = ("retry-models", "Choose another model");
    let retry = ("retry-connection", "Retry connection");
    let paste_key = ("paste-key", "Enter a different API key");
    let switch_provider = ("connect", "Switch provider");
    let (message, mut actions) = match (status, error.downcast_ref::<ConnectionError>()) {
        (Some(402), _) | (_, Some(ConnectionError::BillingRequired)) => (
            "Insufficient balance. Top up your provider account.",
            vec![retry, switch_provider],
        ),
        (Some(401), _) | (_, Some(ConnectionError::RejectedCredential)) => (
            "The provider rejected the API key.",
            vec![paste_key, switch_provider],
        ),
        (Some(404), _) => (
            "The provider could not find the selected model (HTTP 404).",
            vec![choose_model, switch_provider],
        ),
        (Some(503), _) => (
            "The selected model is temporarily unavailable (HTTP 503).",
            vec![retry, choose_model],
        ),
        _ => {
            let mut actions = vec![choose_model];
            if codingbuddy_llm::provider_requires_key(llm) {
                actions.push(paste_key);
            }
            (
                "Could not verify the model. Details are in the chat.",
                actions,
            )
        }
    };
    if can_save {
        actions.push(("save-key", "Save API key"));
    }
    actions.push(("cancel", "Return to chat"));
    TerminalMenu::actions("Connection failed", message, choices(&actions))
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

#[cfg(test)]
mod tests {
    use super::*;
    use codingbuddy_llm::{error::ProviderError, protocol::ChatProtocol};

    #[test]
    fn gemini_model_errors_offer_recovery_without_replacing_the_key() {
        let mut llm = LlmConfig::default();
        llm.select_provider("google");
        for (status, expected_actions) in [
            (404, ["retry-models", "connect", "cancel"]),
            (503, ["retry-connection", "retry-models", "cancel"]),
        ] {
            let error = ProviderError::http(ChatProtocol::GeminiGenerateContent, status, "{}");
            assert!(error.to_string().contains(&format!("HTTP {status}")));
            let menu = connection_failure_menu(&llm, &error.into(), false);
            assert!(menu.message.contains(&format!("HTTP {status}")));
            assert_eq!(
                menu.choices.iter().map(|(id, _)| id).collect::<Vec<_>>(),
                expected_actions
            );
        }
    }
    #[test]
    fn save_key_is_available_after_billing_and_unclassified_failures() {
        let llm = LlmConfig::default();
        for error in [
            ProviderError::http(ChatProtocol::OpenAiChat, 402, "{}").into(),
            anyhow!("The connection timed out"),
        ] {
            let menu = connection_failure_menu(&llm, &error, true);
            assert!(menu.choices.iter().any(|(id, _)| id == "save-key"));
        }
    }
}
