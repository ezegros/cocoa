//! Centered local-command dialogs. Secret input never enters the chat composer.

use crate::{
    catalogue::{Catalogue, Efforts, Model, PROVIDERS},
    config::{self, Alias},
    status::{Kind, Status},
};
use cocoa::mcp::presets::{PRESETS, Preset};
use cocoa::theme::Theme;
use cocoa_llm::{ProviderKind, ThinkingLevel};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{
    Frame,
    buffer::CellWidth,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph, Wrap},
};
use secrecy::{ExposeSecret, SecretString};
use std::collections::BTreeMap;

/// Credential-free discovery. Obsolete scans cannot replace newer results.
pub(crate) struct RewindPicker {
    pub points: Vec<cocoa::persistence::RewindPoint>,
    selected: usize,
}

impl RewindPicker {
    pub fn new(points: Vec<cocoa::persistence::RewindPoint>) -> Self {
        Self {
            points,
            selected: 0,
        }
    }

    pub fn input(&mut self, event: Event) -> cocoa::chat::Action {
        use cocoa::chat::Action;
        let Event::Key(key) = event else {
            return Action::None;
        };
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        match key.code {
            KeyCode::Esc => Action::Command("/rewind-close".into()),
            KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => Action::Exit,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                Action::Redraw
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.points.len().saturating_sub(1));
                Action::Redraw
            }
            KeyCode::Enter => self
                .points
                .get(self.selected)
                .map_or(Action::None, |point| {
                    Action::Command(format!("/rewind-before {}", i64::from(point.record)))
                }),
            _ => Action::None,
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>, theme: Theme) {
        let bounds = frame.area();
        frame.render_widget(Clear, bounds);
        let block = Block::bordered()
            .title(" Rewind before prompt ")
            .style(theme.style());
        let inner = block.inner(bounds);
        frame.render_widget(block, bounds);
        let visible = usize::from(inner.height.saturating_sub(4));
        let first = self.selected.saturating_sub(visible.saturating_sub(1));
        let mut lines = Vec::new();
        for (index, point) in self.points.iter().enumerate().skip(first).take(visible) {
            let label: String = point.input.text().chars()
                .filter(|character| !character.is_control() && !matches!(*character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')).collect();
            lines.push(Line::styled(
                fit_session_label(
                    &format!(
                        "{} {} [{} images]",
                        i64::from(point.record),
                        label,
                        point.input.images().len()
                    ),
                    inner.width,
                ),
                if index == self.selected {
                    theme.selection_style()
                } else {
                    Style::default()
                },
            ));
        }
        lines.push(Line::default());
        lines.push(Line::from(
            "Files unchanged. JavaScript and handles reset. Original preserved.",
        ));
        lines.push(Line::from("↑↓ select · Enter fork · Esc cancel"));
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

pub(crate) struct SessionPicker {
    database: Option<cocoa::persistence::SessionDatabase>,
    directory: std::path::PathBuf,
    query: String,
    selected: usize,
    matches: Vec<cocoa::persistence::SearchMatch>,
    deadline: Option<tokio::time::Instant>,
    task: Option<
        tokio::task::JoinHandle<
            Result<Vec<cocoa::persistence::SearchMatch>, cocoa::persistence::PersistenceError>,
        >,
    >,
    status: String,
}

impl SessionPicker {
    pub fn new(
        database: Option<cocoa::persistence::SessionDatabase>,
        directory: std::path::PathBuf,
        query: String,
    ) -> Self {
        Self {
            database,
            directory,
            query,
            selected: 0,
            matches: Vec::new(),
            deadline: Some(tokio::time::Instant::now()),
            task: None,
            status: String::new(),
        }
    }

    pub fn input(&mut self, event: Event) -> cocoa::chat::Action {
        if let Event::Paste(text) = event {
            for character in text.chars().filter(|character| !character.is_control()) {
                if self.query.len() + character.len_utf8() > 512 {
                    break;
                }
                self.query.push(character);
            }
            return self.refilter();
        }
        let Event::Key(key) = event else {
            return cocoa::chat::Action::None;
        };
        if key.kind != KeyEventKind::Press {
            return cocoa::chat::Action::None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => return cocoa::chat::Action::Command("/sessions-close".into()),
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return cocoa::chat::Action::Exit,
            (KeyCode::Enter, _) => {
                if self.deadline.is_none()
                    && self.task.is_none()
                    && let Some(matched) = self.matches.get(self.selected)
                {
                    return cocoa::chat::Action::Command(format!(
                        "/resume {}",
                        matched.session.public_id
                    ));
                }
                return cocoa::chat::Action::None;
            }
            (KeyCode::Up, _) => {
                self.selected = self.selected.saturating_sub(1);
                return cocoa::chat::Action::Redraw;
            }
            (KeyCode::Down, _) => {
                self.selected = (self.selected + 1).min(self.matches.len().saturating_sub(1));
                return cocoa::chat::Action::Redraw;
            }
            (KeyCode::Backspace, _) => {
                self.query.pop();
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => self.query.clear(),
            (KeyCode::Char(character), KeyModifiers::NONE | KeyModifiers::SHIFT)
                if !character.is_control() && self.query.len() + character.len_utf8() <= 512 =>
            {
                self.query.push(character)
            }
            _ => return cocoa::chat::Action::None,
        }
        self.refilter()
    }

    pub async fn poll(&mut self) -> bool {
        if self
            .deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            self.deadline = None;
            let Some(database) = self.database.clone() else {
                self.status = "No sessions found.".into();
                return true;
            };
            let directory = self.directory.clone();
            let query = self.query.clone();
            self.task = Some(tokio::spawn(async move {
                if query.trim().is_empty() {
                    Ok(database
                        .list_sessions(Some(&directory), 50, None)
                        .await?
                        .sessions
                        .into_iter()
                        .map(|session| cocoa::persistence::SearchMatch {
                            session,
                            snippet: String::new(),
                            source: cocoa::persistence::SearchSource::Title,
                        })
                        .collect())
                } else {
                    Ok(database
                        .search_sessions(Some(&directory), &query, 50)
                        .await?
                        .sessions)
                }
            }));
            return true;
        }
        if self
            .task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            let Some(task) = self.task.take() else {
                return false;
            };
            match task.await {
                Ok(Ok(matches)) => {
                    self.matches = matches;
                    self.selected = self.selected.min(self.matches.len().saturating_sub(1));
                    self.status = if self.matches.is_empty() {
                        "No sessions found.".into()
                    } else {
                        "Up to 50 results; refine the search for more.".into()
                    };
                }
                Ok(Err(error)) => {
                    self.matches.clear();
                    self.status = error.to_string();
                }
                Err(error) => {
                    self.matches.clear();
                    self.status = format!("Session search failed: {error}");
                }
            }
            return true;
        }
        false
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, theme: Theme) {
        let palette = theme.palette();
        let bounds = frame.area();
        let area = Rect::new(
            bounds.x + 2.min(bounds.width / 2),
            bounds.y + 1.min(bounds.height / 2),
            bounds.width.saturating_sub(4),
            bounds.height.saturating_sub(2),
        );
        frame.render_widget(Clear, bounds);
        frame.render_widget(Block::new().style(theme.style()), bounds);
        frame.render_widget(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(palette.border))
                .title(" Sessions "),
            area,
        );
        let inner = area.inner(ratatui::layout::Margin::new(2, 1));
        let muted = palette.muted;
        let clean = |text: &str| {
            text.chars().filter(|character| !character.is_control() && !matches!(*character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')).collect::<String>()
        };
        let mut lines = vec![Line::from(format!("/ {}", self.query)), Line::default()];
        let visible = usize::from(inner.height.saturating_sub(6));
        let first = self.selected.saturating_sub(visible.saturating_sub(1));
        for (index, matched) in self.matches.iter().enumerate().skip(first).take(visible) {
            let session = &matched.session;
            let style = if self.selected == index {
                theme.selection_style()
            } else {
                Style::default()
            };
            lines.push(Line::styled(
                fit_session_label(
                    &format!(
                        "{} {}",
                        if index == self.selected { "›" } else { " " },
                        clean(session.title.as_deref().unwrap_or("Untitled session"))
                    ),
                    inner.width,
                ),
                style,
            ));
        }
        frame.render_widget(Paragraph::new(lines), inner);
        let status = if self.deadline.is_some() || self.task.is_some() {
            "Searching..."
        } else {
            &self.status
        };
        if inner.height >= 3 {
            frame.render_widget(
                Paragraph::new(status).style(Style::new().fg(muted)),
                Rect::new(inner.x, inner.bottom() - 3, inner.width, 1),
            );
            frame.render_widget(
                Paragraph::new("↑↓ select · Enter resume · Esc cancel")
                    .style(Style::new().fg(muted)),
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            );
        }
        if inner.width > 2 && inner.height > 0 {
            frame.set_cursor_position((
                inner.x + 2 + (Line::from(self.query.as_str()).width() as u16).min(inner.width - 3),
                inner.y,
            ));
        }
    }

    /// Restart the search after the query changed.
    fn refilter(&mut self) -> cocoa::chat::Action {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_millis(200));
        self.selected = 0;
        self.status.clear();
        cocoa::chat::Action::Redraw
    }
}

impl Drop for SessionPicker {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub(crate) enum Action {
    None,
    Close,
    Exit,
    CancelConnection,
    McpAdd {
        name: String,
        url: String,
    },
    McpConnect(String),
    McpAuth(String),
    McpToggle(String),
    McpLogout(String),
    McpRemove(String),
    ChatGptLogin {
        registration: Option<String>,
    },
    SelectAccount(String),
    LogoutAccount(String),
    ConnectProvider {
        provider: ProviderKind,
        key: Option<SecretString>,
    },
    SaveSetup {
        provider: ProviderKind,
        key: Option<SecretString>,
    },
    SaveAliasChoices {
        name: String,
        entries: Vec<Alias>,
        make_default: bool,
    },
    SelectModel {
        model: String,
        thinking: Option<ThinkingLevel>,
    },
    SelectAliasChoice {
        alias: String,
        choice: usize,
    },
    SaveKey {
        provider: ProviderKind,
        key: Option<SecretString>,
    },
}

#[derive(Clone)]
enum ModelTarget {
    Choice,
    Session,
}

struct ModelSelection {
    key: String,
    model: Model,
    thinking: Option<ThinkingLevel>,
}

enum Page {
    Mcp,
    McpServer {
        name: String,
    },
    McpPresets,
    McpName {
        preset: Option<&'static Preset>,
    },
    McpUrl {
        name: String,
        preset: Option<&'static Preset>,
    },
    McpTools {
        name: String,
    },
    McpTool {
        name: String,
        tool: cocoa::mcp::Tool,
        scroll: u16,
    },
    McpWaiting {
        name: String,
    },
    McpRemove {
        name: String,
    },
    Accounts,
    Account {
        registration: String,
    },
    PlanConsent {
        registration: Option<String>,
    },
    OAuthWaiting,
    SetupProviders,
    SetupLoading {
        provider: ProviderKind,
    },
    SetupModels {
        provider: ProviderKind,
    },
    SetupManual {
        provider: ProviderKind,
    },
    AliasEntries,
    AliasEntry,
    Priority {
        entry: Alias,
    },
    ModelPicker,
    Aliases,
    Name,
    Models {
        target: ModelTarget,
    },
    Thinking {
        target: ModelTarget,
        model: Model,
    },
    Manual {
        target: ModelTarget,
        provider: ProviderKind,
    },
    Providers {
        login: bool,
    },
    Login {
        provider: ProviderKind,
    },
    Logout {
        provider: ProviderKind,
    },
}

pub(crate) struct Dialog {
    page: Page,
    onboarding: bool,
    aliases: BTreeMap<String, Vec<Alias>>,
    editing_alias: String,
    editing_entry: Option<usize>,
    models: Vec<Model>,
    connected_providers: Vec<ProviderKind>,
    thinking_choices: BTreeMap<String, Option<ThinkingLevel>>,
    alias_choices: BTreeMap<String, usize>,
    accounts: Vec<cocoa_oauth::Account>,
    servers: Vec<cocoa::mcp::ServerView>,
    query: String,
    query_overflow: bool,
    secret: SecretString,
    secret_overflow: bool,
    selected: usize,
    status: String,
    area: Rect,
    rows_area: Rect,
    first: usize,
}

impl Dialog {
    pub fn onboarding() -> Self {
        let mut dialog = Self::new(Page::SetupProviders);
        dialog.onboarding = true;
        dialog
    }

    pub fn is_onboarding(&self) -> bool {
        self.onboarding
    }

    pub fn setup_catalogue(&mut self, provider: ProviderKind, catalogue: Catalogue) -> Action {
        self.models.retain(|model| model.provider != provider);
        self.models.extend(catalogue.models);
        self.save_setup(provider)
    }

    pub fn setup_unavailable(&mut self, provider: ProviderKind) {
        self.page = Page::SetupModels { provider };
        self.query.clear();
        self.selected = 0;
        self.status = if provider == ProviderKind::Ollama {
            "Start Ollama with a tool-capable model, then retry.".into()
        } else {
            "Catalogue unavailable. Retry or enter a model ID. Key unverified.".into()
        };
    }

    pub fn setup_rejected(&mut self, provider: ProviderKind, message: String) {
        self.secret = "".into();
        self.page = if provider == ProviderKind::Ollama {
            Page::SetupModels { provider }
        } else {
            Page::Login { provider }
        };
        self.selected = 0;
        self.status = message;
    }

    pub fn setup_saved(&mut self, provider: ProviderKind) {
        self.secret = "".into();
        self.setup_unavailable(provider);
        if self.models.iter().any(|model| model.provider == provider) {
            self.status = "Choose a tool-capable model.".into();
        }
    }

    pub fn setup_without_catalogue(&mut self, provider: ProviderKind) -> Action {
        self.save_setup(provider)
    }

    pub fn aliases() -> Self {
        let mut dialog = Self::new(Page::Aliases);
        match config::aliases() {
            Ok(aliases) => dialog.aliases = aliases,
            Err(error) => dialog.status = error.to_string(),
        }
        dialog
    }

    pub fn models() -> Self {
        let mut dialog = Self::aliases();
        dialog.page = Page::Models {
            target: ModelTarget::Session,
        };
        dialog
    }

    pub fn model_picker() -> Self {
        let mut dialog = Self::aliases();
        dialog.page = if dialog.aliases.is_empty() {
            Page::Models {
                target: ModelTarget::Session,
            }
        } else {
            Page::ModelPicker
        };
        dialog
    }

    pub fn alias_choices_saved(&mut self) {
        let selected = if matches!(self.page, Page::AliasEntries) {
            self.selected
        } else {
            self.editing_entry.unwrap_or(usize::MAX)
        };
        match config::aliases() {
            Ok(aliases) => self.aliases = aliases,
            Err(error) => {
                self.error(error.to_string());
                return;
            }
        }
        self.page = if self.aliases.contains_key(&self.editing_alias) {
            Page::AliasEntries
        } else {
            Page::Aliases
        };
        self.query.clear();
        self.selected = selected.min(
            self.aliases
                .get(&self.editing_alias)
                .map_or(0, |entries| entries.len().saturating_sub(1)),
        );
        self.status = "✓ Alias saved. Live sessions unchanged.".into();
    }

    pub fn mcp(servers: Vec<cocoa::mcp::ServerView>) -> Self {
        let mut dialog = Self::new(Page::Mcp);
        dialog.servers = servers;
        dialog
    }

    pub fn mcp_updated(&mut self, servers: Vec<cocoa::mcp::ServerView>, error: Option<String>) {
        self.servers = servers;
        if let Page::McpWaiting { name } = &self.page {
            self.page = Page::McpServer { name: name.clone() };
            self.selected = 0;
        }
        if matches!(&self.page, Page::McpServer { name } if !self.servers.iter().any(|server| &server.name == name))
        {
            self.page = Page::Mcp;
        }
        self.status = error.unwrap_or_default();
    }

    pub fn auth(login: bool, provider: Option<ProviderKind>) -> Self {
        let page = match provider {
            Some(provider) if login => Page::Login { provider },
            Some(provider) => Page::Logout { provider },
            None => Page::Providers { login },
        };
        let mut dialog = Self::new(page);
        if provider == Some(ProviderKind::ChatGpt) {
            dialog.chatgpt_accounts();
        }
        dialog
    }

    pub fn subscription_ready(&mut self) -> Action {
        self.chatgpt_accounts();
        if self.onboarding {
            self.connect_provider(ProviderKind::ChatGpt)
        } else {
            Action::Close
        }
    }

    pub fn subscription_error(&mut self, message: String) {
        self.chatgpt_accounts();
        self.status = message;
    }

    pub fn program_status(&self) -> Status {
        match self.page {
            Page::OAuthWaiting => Status::Working("Waiting for browser sign-in"),
            Page::McpWaiting { .. } => Status::Working("Connecting MCP server"),
            Page::SetupLoading { .. } => Status::Working("Connecting provider"),
            Page::Login { .. } => Status::Blocked {
                kind: Kind::Auth,
                message: "Enter a provider API key",
            },
            Page::Logout { .. } => Status::Blocked {
                kind: Kind::Permission,
                message: "Confirm removing the saved API key",
            },
            _ => Status::Blocked {
                kind: Kind::Question,
                message: "Choose provider or alias settings",
            },
        }
    }

    pub fn catalogue(&mut self, catalogue: Catalogue) {
        let thinking = self.levels().get(self.selected).copied();
        self.models = catalogue.models;
        self.connected_providers = catalogue.connected;
        if let Page::Thinking { model, .. } = &mut self.page
            && let Some(discovered) = self.models.iter().find(|discovered| {
                discovered.provider == model.provider && discovered.id == model.id
            })
        {
            *model = discovered.clone();
            self.selected = thinking
                .and_then(|thinking| self.levels().iter().position(|level| *level == thinking))
                .unwrap_or(0);
        }
        self.status = if catalogue.unavailable.is_empty() {
            String::new()
        } else {
            "Some models unavailable. Manual entry available.".into()
        };
    }

    pub fn error(&mut self, message: String) {
        if let Page::SetupLoading { provider } = self.page {
            self.setup_unavailable(provider);
        }
        self.status = message;
    }

    pub fn input(&mut self, event: Event) -> Action {
        let key = match event {
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.selected = self.selected.saturating_sub(1),
                    MouseEventKind::ScrollDown => {
                        self.selected = (self.selected + 1).min(self.rows().len().saturating_sub(1))
                    }
                    MouseEventKind::Moved
                        if matches!(self.page, Page::ModelPicker | Page::Models { .. })
                            && self.rows_area.contains((mouse.column, mouse.row).into()) =>
                    {
                        let offset = usize::from(mouse.row - self.rows_area.y);
                        if self.first + offset < self.rows().len() {
                            self.selected = self.first + offset;
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left)
                        if self.rows_area.contains((mouse.column, mouse.row).into()) =>
                    {
                        let offset = usize::from(mouse.row - self.rows_area.y);
                        if self.first + offset < self.rows().len() {
                            self.selected = self.first + offset;
                            return self.choose();
                        }
                    }
                    _ => {}
                }
                return Action::None;
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => key,
            Event::Paste(text) => {
                if !self.waiting() {
                    for character in text.chars().filter(|character| !character.is_control()) {
                        self.insert_character(character);
                    }
                }
                return Action::None;
            }
            _ => return Action::None,
        };
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.secret = "".into();
            self.secret_overflow = false;
            return if self.onboarding {
                Action::Exit
            } else {
                Action::Close
            };
        }
        if key.code == KeyCode::Esc {
            if let Page::McpWaiting { name } = &self.page {
                self.page = Page::McpServer { name: name.clone() };
                self.status = "Connection cancelled. Saved configuration is unchanged.".into();
                return Action::CancelConnection;
            }
            if matches!(self.page, Page::OAuthWaiting) {
                self.chatgpt_accounts();
                self.status = "Sign-in cancelled. No generation request was made.".into();
                return Action::CancelConnection;
            }
            if matches!(self.page, Page::SetupLoading { .. }) {
                self.secret = "".into();
                self.secret_overflow = false;
                self.page = Page::SetupProviders;
                self.status.clear();
                self.selected = 0;
                return Action::CancelConnection;
            }
            if !matches!(self.page, Page::SetupManual { .. }) {
                self.secret = "".into();
                self.secret_overflow = false;
            }
            let query = match &self.page {
                Page::McpUrl { name, .. } => name.clone(),
                _ => String::new(),
            };
            self.page = match &self.page {
                Page::Mcp => return Action::Close,
                Page::McpServer { .. } | Page::McpPresets => Page::Mcp,
                Page::McpName { .. } => Page::McpPresets,
                Page::McpUrl { preset, .. } => Page::McpName { preset: *preset },
                Page::McpTools { name } | Page::McpRemove { name } | Page::McpWaiting { name } => {
                    Page::McpServer { name: name.clone() }
                }
                Page::McpTool { name, .. } => Page::McpTools { name: name.clone() },
                Page::Accounts | Page::PlanConsent { .. } if self.onboarding => {
                    Page::SetupProviders
                }
                Page::Accounts | Page::PlanConsent { .. } => return Action::Close,
                Page::Account { .. } | Page::OAuthWaiting => Page::Accounts,
                Page::SetupProviders => return Action::Exit,
                Page::SetupModels { .. } => Page::SetupProviders,
                Page::SetupManual { provider } => Page::SetupModels {
                    provider: *provider,
                },
                Page::SetupLoading { .. } => Page::SetupProviders,
                Page::Thinking { target, .. } => Page::Models {
                    target: target.clone(),
                },
                Page::Manual { target, .. } => Page::Models {
                    target: target.clone(),
                },
                Page::Models {
                    target: ModelTarget::Session,
                } => return Action::Close,
                Page::Models {
                    target: ModelTarget::Choice,
                } => Page::AliasEntries,
                Page::ModelPicker => return Action::Close,
                Page::Name | Page::AliasEntries => {
                    if self
                        .aliases
                        .get(&self.editing_alias)
                        .is_some_and(Vec::is_empty)
                    {
                        self.aliases.remove(&self.editing_alias);
                    }
                    Page::Aliases
                }
                Page::AliasEntry | Page::Priority { .. } => Page::AliasEntries,
                Page::Aliases | Page::Providers { .. } => {
                    return Action::Close;
                }
                Page::Login { .. } if self.onboarding => Page::SetupProviders,
                Page::Login { .. } => Page::Providers { login: true },
                Page::Logout { .. } => Page::Providers { login: false },
            };
            self.query = query;
            self.query_overflow = false;
            self.selected = 0;
            self.thinking_choices.clear();
            self.status.clear();
            return Action::None;
        }
        if self.waiting() {
            return Action::None;
        }
        let code = match (key.code, key.modifiers) {
            (KeyCode::Char('n'), KeyModifiers::CONTROL) => KeyCode::Down,
            (KeyCode::Char('p'), KeyModifiers::CONTROL) => KeyCode::Up,
            _ => key.code,
        };
        if let Page::McpTool { scroll, .. } = &mut self.page {
            match code {
                KeyCode::Up => *scroll = scroll.saturating_sub(1),
                KeyCode::Down => *scroll = scroll.saturating_add(1),
                KeyCode::Home => *scroll = 0,
                KeyCode::End => *scroll = u16::MAX,
                _ => {}
            }
            return Action::None;
        }
        match code {
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.rows().len().saturating_sub(1),
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.rows().len().saturating_sub(1))
            }
            KeyCode::Left | KeyCode::Right => {
                if let Some((alias, choice)) = self.alias_selection() {
                    let choices = config::regular_choices(&self.aliases[&alias]);
                    let selected = choices
                        .iter()
                        .position(|index| *index == choice)
                        .unwrap_or(0);
                    let selected = if code == KeyCode::Left {
                        selected.saturating_sub(1)
                    } else {
                        (selected + 1).min(choices.len() - 1)
                    };
                    self.alias_choices.insert(alias, choices[selected]);
                } else if let Some(selection) = self.model_selection() {
                    let levels = thinking_levels(&selection.model);
                    let selected = levels
                        .iter()
                        .position(|level| *level == selection.thinking)
                        .unwrap_or(0);
                    let selected = if code == KeyCode::Left {
                        selected.saturating_sub(1)
                    } else {
                        (selected + 1).min(levels.len().saturating_sub(1))
                    };
                    self.thinking_choices
                        .insert(selection.key, levels[selected]);
                }
            }
            KeyCode::Enter => return self.choose(),
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                self.query.clear();
                self.query_overflow = false;
                self.secret = "".into();
                self.secret_overflow = false;
                self.status.clear();
            }
            KeyCode::Backspace => {
                if matches!(self.page, Page::Login { .. }) {
                    let mut secret = self.secret.expose_secret().to_owned();
                    secret.pop();
                    self.secret = secret.into();
                } else {
                    self.query.pop();
                }
                self.selected = 0;
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) && !character.is_control() =>
            {
                self.insert_character(character);
            }
            _ => {}
        }
        Action::None
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, theme: Theme) {
        let palette = theme.palette();
        let bounds = frame.area();
        let width = bounds.width.saturating_sub(4).min(76);
        let height = bounds.height.saturating_sub(2).min(20);
        let spacious = height >= 20;
        self.area = Rect::new(
            bounds.x + (bounds.width - width) / 2,
            bounds.y + (bounds.height - height) / 2,
            width,
            height,
        );
        let title = match &self.page {
            Page::Mcp => "MCP servers".into(),
            Page::McpServer { name } => format!("MCP › {name}"),
            Page::McpPresets => format!("Add server › {} presets", PRESETS.len()),
            Page::McpName { .. } => "Add server › Name".into(),
            Page::McpUrl { name, .. } => format!("{name} › HTTP URL"),
            Page::McpTools { name } => format!("{name} › Tools"),
            Page::McpTool { name, tool, .. } => format!("{name} › {}", tool.name),
            Page::McpWaiting { name } => format!("Connect › {name}"),
            Page::McpRemove { name } => format!("Remove › {name}"),
            Page::Accounts => "Subscriptions".into(),
            Page::Account { registration } => self
                .accounts
                .iter()
                .find(|account| &account.registration == registration)
                .map(|account| format!("ChatGPT › {}", account.label))
                .unwrap_or_else(|| "ChatGPT account".into()),
            Page::PlanConsent { .. } => "Use your ChatGPT plan?".into(),
            Page::OAuthWaiting => "Sign in with ChatGPT".into(),
            Page::SetupProviders => "Choose a provider".into(),
            Page::SetupLoading { provider } => format!("Connect › {provider}"),
            Page::SetupModels { provider } => format!("{provider} › Choose a model"),
            Page::SetupManual { provider } => format!("{provider} › Model ID"),
            Page::AliasEntries => format!("{} › Model choices", self.editing_alias),
            Page::AliasEntry => format!("{} › Edit model", self.editing_alias),
            Page::Priority { .. } => format!("{} › Priority", self.editing_alias),
            Page::ModelPicker => "Models · Aliases".into(),
            Page::Aliases => "Aliases".into(),
            Page::Name => "Create an alias".into(),
            Page::Models {
                target: ModelTarget::Session,
            } => "Select model".into(),
            Page::Models {
                target: ModelTarget::Choice,
            } => format!("{} › Model", self.editing_alias),
            Page::Thinking {
                target: ModelTarget::Choice,
                ..
            } => format!("{} › Thinking", self.editing_alias),
            Page::Manual {
                target: ModelTarget::Choice,
                provider,
            } => format!("{} › Model ID · {provider}", self.editing_alias),
            Page::Thinking {
                target: ModelTarget::Session,
                model,
            } => format!("{} › Thinking", model.id),
            Page::Manual {
                target: ModelTarget::Session,
                provider,
            } => format!("Model ID · {provider}"),
            Page::Providers { login } => {
                if *login {
                    "Login › Provider".into()
                } else {
                    "Logout › Provider".into()
                }
            }
            Page::Login { provider } => format!("Login › {provider}"),
            Page::Logout { provider } => format!("Logout › {provider}"),
        };
        frame.render_widget(Clear, bounds);
        frame.render_widget(Block::new().style(theme.style()), bounds);
        let muted = palette.muted;
        let mut border = Block::bordered().border_style(Style::default().fg(palette.border));
        if self.onboarding {
            border = border.title_top(Line::styled(" cocoa ", Style::new().fg(palette.accent)));
        }
        frame.render_widget(border.border_type(BorderType::Rounded), self.area);
        let inner = self.area.inner(ratatui::layout::Margin {
            horizontal: 2,
            vertical: if spacious { 2 } else { 1 },
        });
        let subtitle = match &self.page {
            Page::Mcp => "HTTP servers available to the REPL.",
            Page::McpPresets if inner.width < 56 => "Editable defaults; setup varies.",
            Page::McpPresets => "Name/URL defaults. Setup varies; connections are unverified.",
            Page::McpServer { name } => self
                .servers
                .iter()
                .find(|server| &server.name == name)
                .map(|server| server.status.as_str())
                .unwrap_or("Not connected"),
            Page::McpName { .. } => "Edit the server name. Nothing is saved yet.",
            Page::McpUrl { .. } if inner.width < 56 => "HTTPS endpoint or loopback HTTP.",
            Page::McpUrl { .. } => "HTTPS endpoint, or loopback HTTP. Enter saves and connects.",
            Page::McpTools { .. } => "Choose a tool to inspect its callable path and schema.",
            Page::McpTool { .. } => "Input schema. Complete JSON: cocoa mcp tools NAME",
            Page::McpWaiting { .. } => "Connecting or waiting for browser sign-in. Esc cancels.",
            Page::McpRemove { .. } => "Revokes local tool access. Remote consent is unchanged.",
            Page::Accounts | Page::Account { .. } => {
                "Registrations stay separate, even for the same email."
            }
            Page::PlanConsent { .. } => "Uses ChatGPT plan/credits. No API fallback.",
            Page::OAuthWaiting => "No generation request. Esc cancels sign-in.",
            Page::SetupProviders if inner.width < 44 => "Connect a provider to begin.",
            Page::SetupProviders => "Connect a provider. We will suggest aliases for you.",
            Page::SetupLoading { .. } => "No generation request. Esc back · Ctrl+C quit",
            Page::SetupModels { .. } => "Choose a model, or retry discovery.",
            Page::SetupManual { .. } => "Exact model ID. Key and inference are unverified.",
            Page::AliasEntries => {
                "Ctrl+T cycles regular choices by priority; ties keep list order."
            }
            Page::AliasEntry => "Changes apply to new sessions, not live sessions.",
            Page::Priority { .. } => "0..99, lowest first. Fallback only is a separate setting.",
            Page::ModelPicker if inner.width < 36 => "Saved effort is fixed.",
            Page::ModelPicker if inner.width < 56 => "Saved effort · session only",
            Page::ModelPicker => "Session only · saved effort levels stay unchanged.",
            Page::Thinking { model, .. }
                if matches!(model.efforts, Some(Efforts::ModelsDev(_))) =>
            {
                "Efforts from models.dev."
            }
            Page::Thinking { model, .. } if model.efforts.is_none() => {
                "Provider levels; model-specific support is unverified."
            }
            Page::Thinking {
                target: ModelTarget::Session,
                ..
            } => "Choose thinking for this session.",
            Page::Thinking { .. } => "Choose a thinking level, then return to aliases.",
            Page::Name => "Nothing is created until you save a model and thinking level.",
            Page::Manual { .. } => "Enter the exact model ID from your provider.",
            Page::Login { .. } if self.onboarding => {
                "API key · hidden · Enter reuses saved credentials"
            }
            Page::Login { .. } => "API key · hidden · separate from chat",
            Page::Logout { .. } => {
                "Enter removes the saved key. Environment and live session unchanged."
            }
            _ => "↑↓ select · Enter choose · Esc back",
        };
        let text = if matches!(self.page, Page::Login { .. }) {
            "•".repeat(self.secret.expose_secret().len().min(32))
        } else {
            self.query.clone()
        };
        let inline_selection =
            height >= 9 && matches!(self.page, Page::ModelPicker | Page::Models { .. });
        let compact_models = inline_selection && height < 14;
        let lines = {
            let input = match &self.page {
                Page::AliasEntries | Page::ModelPicker if text.is_empty() => "/ Filter".into(),
                Page::AliasEntries | Page::ModelPicker => {
                    format!("/ {text}")
                }
                Page::AliasEntry => String::new(),
                Page::Accounts
                | Page::Aliases
                | Page::Mcp
                | Page::McpPresets
                | Page::McpTools { .. }
                    if text.is_empty() =>
                {
                    "/ Filter".into()
                }
                Page::Accounts
                | Page::Aliases
                | Page::Mcp
                | Page::McpPresets
                | Page::McpTools { .. } => {
                    format!("/ {text}")
                }
                Page::McpWaiting { .. } => "Connecting...".into(),
                Page::McpServer { .. } | Page::McpRemove { .. } | Page::McpTool { .. } => {
                    String::new()
                }
                Page::Account { .. } => "ChatGPT account".into(),
                Page::PlanConsent { .. } => {
                    "Choose subscription use or separate API billing".into()
                }
                Page::OAuthWaiting => "Browser sign-in · Esc cancels".into(),
                Page::SetupLoading { .. } => "Connecting...".into(),
                Page::Thinking { model, .. } => format!("{} · {}", model.id, model.provider),
                Page::Models { .. } if text.is_empty() && inner.width < 44 => {
                    "/ Search models".into()
                }
                Page::Models { .. } if text.is_empty() => "/ Search models or providers".into(),
                Page::Providers { .. } | Page::SetupProviders if text.is_empty() => {
                    "/ Filter providers".into()
                }
                Page::SetupModels { .. } if text.is_empty() => "/ Search models".into(),
                Page::Models { .. }
                | Page::Providers { .. }
                | Page::SetupProviders
                | Page::SetupModels { .. } => {
                    format!("/ {text}")
                }
                _ => format!("› {text}"),
            };
            let context = match &self.page {
                Page::AliasEntries if inner.width < 56 => self
                    .rows()
                    .get(self.selected)
                    .map(|(_, detail)| detail.clone())
                    .unwrap_or_default(),
                Page::Models {
                    target: ModelTarget::Session,
                } if self.model_selection().is_some_and(|selection| {
                    selection.model.efforts.is_none() && thinking_levels(&selection.model).len() > 1
                }) =>
                {
                    "Session only · model-specific effort support is unverified.".into()
                }
                Page::Models {
                    target: ModelTarget::Session,
                } => "Applies to this session only.".into(),
                Page::Models {
                    target: ModelTarget::Choice,
                } => self.current_entry().map_or_else(
                    || "No saved model. Choose one below.".into(),
                    |saved| {
                        if inner.width < 44 {
                            format!(
                                "{} · {}",
                                saved.provider,
                                thinking_label(saved.thinking_level)
                            )
                        } else {
                            format!(
                                "Saved {} · {} · {}",
                                saved.model,
                                saved.provider,
                                thinking_label(saved.thinking_level)
                            )
                        }
                    },
                ),
                Page::Aliases => String::new(),
                _ => subtitle.into(),
            };
            let mut lines = vec![Line::from(Span::styled(
                title,
                Style::default().add_modifier(Modifier::BOLD),
            ))];
            if height >= 10 && !compact_models {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(input, Style::default().fg(muted))));
            if height >= 7 && !compact_models {
                lines.push(Line::default());
            }
            if !compact_models {
                lines.push(Line::from(Span::styled(
                    context,
                    Style::default().fg(muted),
                )));
            }
            if spacious {
                lines.push(Line::default());
            }
            lines
        };
        let rows = self.rows();
        let saved = self.saved_selection();
        self.selected = self.selected.min(rows.len().saturating_sub(1));
        let rows_y = inner.y + lines.len() as u16;
        self.rows_area = Rect::new(
            inner.x,
            rows_y,
            inner.width,
            self.area
                .bottom()
                .saturating_sub(if inline_selection { 5 } else { 4 })
                .saturating_sub(rows_y)
                .min(7),
        );
        let visible = usize::from(self.rows_area.height);
        self.first = self.selected.saturating_sub(visible.saturating_sub(1));
        frame.render_widget(Paragraph::new(lines), inner);
        for (index, (label, detail)) in rows.iter().enumerate().skip(self.first).take(visible) {
            let selected = index == self.selected;
            let style = if selected {
                theme.selection_style()
            } else {
                Style::default()
            };
            let row = Rect::new(
                self.rows_area.x,
                self.rows_area.y + (index - self.first) as u16,
                self.rows_area.width,
                1,
            );
            frame.render_widget(Block::default().style(style), row);
            let marker = if saved == Some(index) {
                if row.width >= 36 {
                    " ✓ saved"
                } else {
                    " ✓"
                }
            } else {
                ""
            };
            let detail = if matches!(self.page, Page::AliasEntries) && inner.width < 56 {
                detail
                    .rsplit_once(" · ")
                    .map_or("", |(_, thinking)| thinking)
            } else {
                detail.as_str()
            };
            let marker_width = (Line::from(marker).width() as u16).min(row.width);
            let content_width = row.width.saturating_sub(marker_width);
            let detail_limit =
                content_width.saturating_sub(Line::from(label.as_str()).width() as u16 + 4);
            let detail_width = if content_width >= 16 {
                (Line::from(detail).width() as u16).min(detail_limit)
            } else {
                0
            };
            let label_area = Rect::new(
                row.x,
                row.y,
                content_width.saturating_sub(detail_width + u16::from(detail_width > 0)),
                1,
            );
            frame.render_widget(
                Paragraph::new(format!("{} {label}", if selected { "›" } else { " " }))
                    .style(style),
                label_area,
            );
            if detail_width > 0 {
                frame.render_widget(
                    Paragraph::new(detail)
                        .right_aligned()
                        .style(style.fg(muted)),
                    Rect::new(row.x + content_width - detail_width, row.y, detail_width, 1),
                );
            }
            if marker_width > 0 {
                frame.render_widget(
                    Paragraph::new(marker).style(style.fg(palette.success)),
                    Rect::new(row.right() - marker_width, row.y, marker_width, 1),
                );
            }
        }
        if rows.is_empty() && matches!(self.page, Page::McpTools { .. }) {
            frame.render_widget(
                Paragraph::new(
                    "No available tools match. Connect or authenticate from the server menu.",
                )
                .style(Style::default().fg(muted)),
                self.rows_area,
            );
        }
        if let Page::McpName {
            preset: Some(preset),
        }
        | Page::McpUrl {
            preset: Some(preset),
            ..
        } = &self.page
        {
            let mut details_area = self.rows_area;
            if self.status.is_empty() {
                details_area.height = self
                    .area
                    .bottom()
                    .saturating_sub(2)
                    .saturating_sub(details_area.y);
            }
            frame.render_widget(
                Paragraph::new(format!(
                    "{}{}{}",
                    preset.setup,
                    if spacious { "\n\nDocs: " } else { "\n" },
                    preset.documentation
                ))
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(muted)),
                details_area,
            );
        }
        if let Page::McpTool { name, tool, scroll } = &mut self.page {
            let schema = serde_json::to_string_pretty(&tool.input_schema).unwrap_or_default();
            let description: String = tool
                .description
                .chars()
                .filter(|character| !character.is_control())
                .collect();
            let paragraph = Paragraph::new(format!(
                "{}(arguments)\n{description}\n{schema}",
                cocoa::mcp::callable(name, &tool.name)
            ))
            .wrap(Wrap { trim: false });
            let last = paragraph
                .line_count(self.rows_area.width)
                .saturating_sub(usize::from(self.rows_area.height));
            *scroll = (*scroll).min(last.min(usize::from(u16::MAX)) as u16);
            frame.render_widget(paragraph.scroll((*scroll, 0)), self.rows_area);
        }
        let input_offset = if height >= 10 && !compact_models {
            2
        } else {
            1
        };
        if inner.width > 2
            && inner.height > input_offset
            && !matches!(
                self.page,
                Page::Thinking { .. }
                    | Page::Account { .. }
                    | Page::McpServer { .. }
                    | Page::McpWaiting { .. }
                    | Page::McpRemove { .. }
                    | Page::McpTool { .. }
                    | Page::PlanConsent { .. }
                    | Page::OAuthWaiting
                    | Page::Logout { .. }
                    | Page::SetupLoading { .. }
            )
        {
            let cursor = (Line::from(text.as_str()).width() as u16).min(inner.width - 3);
            frame.set_cursor_position((inner.x + 2 + cursor, inner.y + input_offset));
        }
        if inline_selection {
            frame.render_widget(
                Paragraph::new("─".repeat(usize::from(inner.width)))
                    .style(Style::new().fg(palette.border)),
                Rect::new(inner.x, self.area.bottom() - 5, inner.width, 1),
            );
            if matches!(self.page, Page::ModelPicker) {
                let row = Rect::new(inner.x, self.area.bottom() - 4, inner.width, 1);
                if let Some((alias, choice)) = self.alias_selection() {
                    let entries = &self.aliases[&alias];
                    let choices = config::regular_choices(entries);
                    let selected = choices
                        .iter()
                        .position(|index| *index == choice)
                        .unwrap_or(0);
                    let entry = &entries[choice];
                    let prefix = format!(
                        "{}{}/{} {} ",
                        if inner.width < 56 { "" } else { "Model " },
                        selected + 1,
                        choices.len(),
                        if selected > 0 { "‹" } else { " " }
                    );
                    let prefix_width = (Line::from(prefix.as_str()).width() as u16).min(row.width);
                    frame.render_widget(Paragraph::new(prefix).style(Style::new().fg(muted)), row);
                    frame.render_widget(
                        Paragraph::new(format!(
                            "{} · {}",
                            if inner.width < 56 {
                                entry.model.clone()
                            } else {
                                format!("{}/{}", entry.provider, entry.model)
                            },
                            thinking_label(entry.thinking_level)
                        ))
                        .style(theme.selection_style().add_modifier(Modifier::BOLD)),
                        Rect::new(
                            row.x + prefix_width,
                            row.y,
                            row.width.saturating_sub(prefix_width + 2),
                            1,
                        ),
                    );
                    if selected + 1 < choices.len() {
                        frame.render_widget(
                            Paragraph::new(" ›").style(Style::new().fg(muted)),
                            Rect::new(row.right().saturating_sub(2), row.y, row.width.min(2), 1),
                        );
                    }
                } else {
                    frame.render_widget(
                        Paragraph::new("Browse provider models").style(Style::new().fg(muted)),
                        row,
                    );
                }
            } else {
                let mut spans = vec![Span::styled("Effort  ", Style::new().fg(muted))];
                if let Some(selection) = self.model_selection() {
                    let levels = thinking_levels(&selection.model);
                    let selected = levels
                        .iter()
                        .position(|level| *level == selection.thinking)
                        .unwrap_or(0);
                    let labels: Vec<_> = levels
                        .iter()
                        .map(|level| format!(" {} ", thinking_label(*level)))
                        .collect();
                    let available = usize::from(inner.width.saturating_sub(12));
                    let mut first = 0;
                    while first < selected
                        && labels[first..=selected]
                            .iter()
                            .map(String::len)
                            .sum::<usize>()
                            > available
                    {
                        first += 1;
                    }
                    spans.push(Span::styled(
                        if first > 0 { "‹ " } else { "  " },
                        Style::new().fg(muted),
                    ));
                    let mut used = 0;
                    for (index, label) in labels.iter().enumerate().skip(first) {
                        if used + label.len() > available {
                            spans.push(Span::styled(" ›", Style::new().fg(muted)));
                            break;
                        }
                        used += label.len();
                        spans.push(Span::styled(
                            label.clone(),
                            if index == selected {
                                theme.selection_style().add_modifier(Modifier::BOLD)
                            } else {
                                Style::new().fg(muted)
                            },
                        ));
                    }
                } else {
                    spans.push(Span::styled("Select a model", Style::new().fg(muted)));
                }
                frame.render_widget(
                    Paragraph::new(Line::from(spans)),
                    Rect::new(inner.x, self.area.bottom() - 4, inner.width, 1),
                );
            }
        }
        if height >= 9 && !self.status.is_empty() {
            let status = self.status.as_str();
            frame.render_widget(
                Paragraph::new(status)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(if status.starts_with('✓') {
                        palette.success
                    } else {
                        muted
                    })),
                Rect::new(
                    inner.x,
                    self.area.bottom() - if inline_selection { 3 } else { 4 },
                    inner.width,
                    if inline_selection { 1 } else { 2 },
                ),
            );
        }
        if height >= 6 {
            let hint = match self.page {
                Page::ModelPicker if inner.width < 36 => "↑↓ alias · ←→ model",
                Page::ModelPicker if inner.width < 56 => "↑↓ alias · ←→ model · Enter · Esc",
                Page::ModelPicker => "↑↓ alias · ←→ model · Enter select · Esc back",
                Page::Models { .. } if inner.width < 36 => "↑↓ model · ←→ effort",
                Page::Models { .. } if inner.width < 56 => "↑↓ model · ←→ effort · Enter · Esc",
                Page::Models { .. } => "↑↓ model · ←→ effort · Enter select · Esc back",
                Page::AliasEntry | Page::AliasEntries => "↑↓ select · Enter choose · Esc back",
                Page::Priority { .. } => "Enter save · Ctrl+U clear · Esc back",
                Page::Accounts
                | Page::Aliases
                | Page::Mcp
                | Page::McpPresets
                | Page::McpTools { .. } => {
                    if inner.width < 44 {
                        "↑↓ · Enter choose · Esc back"
                    } else {
                        "↑↓ select · Enter choose · Esc back"
                    }
                }
                Page::McpName { .. } => "Enter next · Ctrl+U clear · Esc back",
                Page::McpUrl { .. } => "Enter save/connect · Esc back",
                Page::McpWaiting { .. } => "Esc / Ctrl+C cancel",
                Page::McpTool { .. } => "↑↓ scroll · Esc back",
                Page::McpServer { .. } | Page::McpRemove { .. } => "Enter choose · Esc back",
                Page::Account { .. } | Page::PlanConsent { .. } => "Enter choose · Esc back",
                Page::OAuthWaiting => "Esc / Ctrl+C cancel sign-in",
                Page::SetupProviders if !self.aliases.is_empty() => {
                    "Enter choose · Esc back · Ctrl+C quit"
                }
                Page::SetupProviders => "Enter choose · Esc / Ctrl+C quit",
                Page::SetupLoading { .. } => "Esc cancel · Ctrl+C quit",
                Page::SetupModels { .. } => "Enter choose · Esc back · Ctrl+C quit",
                Page::SetupManual { .. } => "Enter save · Esc back",
                Page::Login { .. } if self.onboarding => "Enter connect · Esc back",
                Page::Thinking {
                    target: ModelTarget::Session,
                    ..
                } => "Enter use · Esc back",
                Page::Thinking { .. } | Page::Login { .. } => "Enter save · Esc back",
                Page::Name | Page::Manual { .. } => "Enter next · Esc back",
                Page::Logout { .. } => "Enter remove · Esc back",
                Page::Providers { .. } => "Enter choose · Esc back",
            };
            frame.render_widget(
                Paragraph::new(hint).style(Style::default().fg(muted)),
                Rect::new(inner.x, self.area.bottom() - 2, inner.width, 1),
            );
        }
    }

    fn chatgpt_accounts(&mut self) {
        self.secret = "".into();
        self.query.clear();
        self.selected = 0;
        self.page = Page::Accounts;
        match cocoa::chatgpt::client(None).and_then(|client| client.accounts()) {
            Ok(accounts) => {
                self.page = if accounts.is_empty() {
                    Page::PlanConsent { registration: None }
                } else {
                    Page::Accounts
                };
                self.accounts = accounts;
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    fn new(page: Page) -> Self {
        Self {
            page,
            onboarding: false,
            aliases: BTreeMap::new(),
            editing_alias: String::new(),
            editing_entry: None,
            models: Vec::new(),
            connected_providers: Vec::new(),
            thinking_choices: BTreeMap::new(),
            alias_choices: BTreeMap::new(),
            accounts: Vec::new(),
            servers: Vec::new(),
            query: String::new(),
            query_overflow: false,
            secret: "".into(),
            secret_overflow: false,
            selected: 0,
            status: String::new(),
            area: Rect::default(),
            rows_area: Rect::default(),
            first: 0,
        }
    }

    fn waiting(&self) -> bool {
        matches!(
            self.page,
            Page::SetupLoading { .. } | Page::OAuthWaiting | Page::McpWaiting { .. }
        )
    }

    fn insert_character(&mut self, character: char) {
        if matches!(self.page, Page::Login { .. }) {
            if self.secret.expose_secret().len() + character.len_utf8() <= 8192 {
                let mut secret = self.secret.expose_secret().to_owned();
                secret.push(character);
                self.secret = secret.into();
            } else {
                self.secret_overflow = true;
                self.status = "API key exceeds 8192 bytes. Ctrl+U clears it; Esc cancels.".into();
            }
        } else if self.query.len() + character.len_utf8()
            <= if matches!(self.page, Page::McpUrl { .. }) {
                2048
            } else {
                256
            }
        {
            self.query.push(character);
        } else {
            self.query_overflow = true;
            self.status = "Input exceeds 256 bytes. Ctrl+U clears it; Esc cancels.".into();
        }
        self.selected = 0;
    }

    fn choose(&mut self) -> Action {
        if self.query_overflow {
            self.status = "Input exceeds 256 bytes. Ctrl+U clears it; Esc cancels.".into();
            return Action::None;
        }
        let rows = self.rows();
        let row = rows.get(self.selected).map(|(label, _)| label.clone());
        match &self.page {
            Page::Mcp => {
                self.page = match row {
                    Some(name) if name != "+ Add server" => Page::McpServer { name },
                    _ => Page::McpPresets,
                };
            }
            Page::McpPresets => {
                let preset = PRESETS
                    .iter()
                    .find(|preset| Some(preset.title) == row.as_deref());
                self.page = Page::McpName { preset };
                self.query = preset.map_or_else(String::new, |preset| preset.name.into());
                self.selected = 0;
                self.status.clear();
                return Action::None;
            }
            Page::McpName { preset } => {
                if self.query.is_empty()
                    || self.query.len() > 64
                    || !self
                        .query
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
                {
                    self.status = "Use 1–64 letters, digits, underscores or hyphens.".into();
                    return Action::None;
                }
                let preset = *preset;
                self.page = Page::McpUrl {
                    name: self.query.clone(),
                    preset,
                };
                self.query = preset.map_or_else(String::new, |preset| preset.url.into());
                self.selected = 0;
                self.status.clear();
                return Action::None;
            }
            Page::McpUrl { name, .. } => {
                let name = name.clone();
                let url = self.query.clone();
                if let Err(error) = cocoa::mcp::Server::new(&url, None) {
                    self.status = error.to_string();
                    return Action::None;
                }
                self.page = Page::McpWaiting { name: name.clone() };
                self.query.clear();
                return Action::McpAdd { name, url };
            }
            Page::McpServer { name } => {
                let name = name.clone();
                match row.as_deref() {
                    Some("Available tools") => self.page = Page::McpTools { name },
                    Some("Connect / refresh") => {
                        self.page = Page::McpWaiting { name: name.clone() };
                        return Action::McpConnect(name);
                    }
                    Some("Authenticate") => {
                        self.page = Page::McpWaiting { name: name.clone() };
                        return Action::McpAuth(name);
                    }
                    Some("Enable" | "Disable") => {
                        self.page = Page::McpWaiting { name: name.clone() };
                        return Action::McpToggle(name);
                    }
                    Some("Forget credentials") => {
                        self.page = Page::McpWaiting { name: name.clone() };
                        return Action::McpLogout(name);
                    }
                    Some("Remove server") => self.page = Page::McpRemove { name },
                    _ => {}
                }
            }
            Page::McpTools { name } => {
                if let Some(tool) = self
                    .servers
                    .iter()
                    .find(|server| &server.name == name)
                    .and_then(|server| {
                        server
                            .tools
                            .iter()
                            .find(|tool| Some(&tool.name) == row.as_ref())
                    })
                {
                    self.page = Page::McpTool {
                        name: name.clone(),
                        tool: tool.clone(),
                        scroll: 0,
                    };
                }
            }
            Page::McpRemove { name } => {
                if self.selected == 0 {
                    let name = name.clone();
                    self.page = Page::McpWaiting { name: name.clone() };
                    return Action::McpRemove(name);
                }
                self.page = Page::McpServer { name: name.clone() };
            }
            Page::McpWaiting { .. } | Page::McpTool { .. } => {}
            Page::Accounts => {
                if let Some(account) = self
                    .accounts
                    .iter()
                    .filter(|account| {
                        account
                            .label
                            .to_lowercase()
                            .contains(&self.query.to_lowercase())
                    })
                    .nth(self.selected)
                {
                    self.page = Page::Account {
                        registration: account.registration.clone(),
                    };
                } else {
                    self.page = Page::PlanConsent { registration: None };
                }
            }
            Page::Account { registration } => {
                let registration = registration.clone();
                match self.selected {
                    0 => return Action::SelectAccount(registration),
                    1 => {
                        self.page = Page::PlanConsent {
                            registration: Some(registration),
                        }
                    }
                    _ => return Action::LogoutAccount(registration),
                }
            }
            Page::PlanConsent { registration } => {
                if self.selected == 0 {
                    let registration = registration.clone();
                    self.page = Page::OAuthWaiting;
                    self.status = "Complete sign-in in your browser. Esc cancels.".into();
                    return Action::ChatGptLogin { registration };
                }
                self.page = Page::Login {
                    provider: ProviderKind::OpenAi,
                };
            }
            Page::OAuthWaiting => {}
            Page::SetupProviders => {
                if let Some(provider) = row.and_then(|name| name.parse::<ProviderKind>().ok()) {
                    if provider == ProviderKind::ChatGpt {
                        self.chatgpt_accounts();
                        return Action::None;
                    }
                    if provider == ProviderKind::Ollama {
                        return self.connect_provider(provider);
                    }
                    self.page = Page::Login { provider };
                }
            }
            Page::SetupLoading { .. } => {}
            Page::AliasEntries => {
                let choices = self.alias_entry_choices();
                if let Some(index) = choices.get(self.selected) {
                    self.editing_entry = Some(*index);
                    self.page = Page::AliasEntry;
                } else {
                    match row.as_deref() {
                        Some("+ Add model") => {
                            if self
                                .aliases
                                .get(&self.editing_alias)
                                .is_some_and(|entries| entries.len() >= 32)
                            {
                                self.status =
                                    "At most 32 model choices are allowed in an alias.".into();
                                return Action::None;
                            }
                            self.editing_entry = None;
                            self.page = Page::Models {
                                target: ModelTarget::Choice,
                            };
                        }
                        Some("Use as default alias") => {
                            return self.save_alias_entries(None, true);
                        }
                        _ => {}
                    }
                }
            }
            Page::AliasEntry => match self.selected {
                0 => {
                    self.page = Page::Models {
                        target: ModelTarget::Choice,
                    }
                }
                1 => {
                    if let Some(entry) = self.current_entry().cloned() {
                        self.query = entry.priority.to_string();
                        self.page = Page::Priority { entry };
                        self.selected = 0;
                        self.status.clear();
                        return Action::None;
                    }
                }
                2 => {
                    if let Some(mut entry) = self.current_entry().cloned() {
                        entry.fallback_only = !entry.fallback_only;
                        return self.save_alias_entries(Some(entry), false);
                    }
                }
                _ => {
                    if let Some(index) = self.editing_entry {
                        let mut entries = self
                            .aliases
                            .get(&self.editing_alias)
                            .cloned()
                            .unwrap_or_default();
                        if index < entries.len() {
                            entries.remove(index);
                        }
                        return Action::SaveAliasChoices {
                            name: self.editing_alias.clone(),
                            entries,
                            make_default: false,
                        };
                    }
                }
            },
            Page::Priority { entry } => {
                let Ok(priority) = self.query.parse::<u8>() else {
                    self.status = "Enter a priority from 0 to 99.".into();
                    return Action::None;
                };
                if priority > 99 {
                    self.status = "Enter a priority from 0 to 99.".into();
                    return Action::None;
                }
                let mut entry = entry.clone();
                entry.priority = priority;
                return self.save_alias_entries(Some(entry), false);
            }
            Page::ModelPicker => {
                if let Some((alias, choice)) = self.alias_selection() {
                    return Action::SelectAliasChoice { alias, choice };
                }
                self.page = Page::Models {
                    target: ModelTarget::Session,
                };
            }
            Page::SetupModels { provider } => {
                let provider = *provider;
                let choices = self.setup_models(provider);
                if let Some(model) = choices.get(self.selected) {
                    return Action::SelectModel {
                        model: format!("{provider}/{}", model.id),
                        thinking: None,
                    };
                }
                match row.as_deref() {
                    Some("Enter model ID") => self.page = Page::SetupManual { provider },
                    Some("Retry discovery") => return self.connect_provider(provider),
                    _ => self.page = Page::SetupProviders,
                }
            }
            Page::SetupManual { provider } => {
                if self.query.trim().is_empty() {
                    self.status = "Enter the exact tool-capable model ID.".into();
                    return Action::None;
                }
                return Action::SelectModel {
                    model: format!("{provider}/{}", self.query.trim()),
                    thinking: None,
                };
            }
            Page::Aliases => {
                if let Some(name) = row {
                    self.page = if name == "+ Add alias" {
                        Page::Name
                    } else {
                        self.editing_alias = name;
                        Page::AliasEntries
                    };
                }
            }
            Page::Name => {
                if self.query.is_empty()
                    || self.query.len() > 64
                    || !self
                        .query
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                {
                    self.status = "Use 1–64 ASCII letters, digits, underscores or hyphens.".into();
                    return Action::None;
                }
                if self.aliases.contains_key(&self.query) {
                    self.status = "That alias already exists. Choose another name.".into();
                    return Action::None;
                }
                if self.aliases.len() >= 32 {
                    self.status = "At most 32 aliases are allowed.".into();
                    return Action::None;
                }
                self.editing_alias = self.query.clone();
                self.aliases.insert(self.editing_alias.clone(), Vec::new());
                self.editing_entry = None;
                self.page = Page::Models {
                    target: ModelTarget::Choice,
                };
            }
            Page::Models { target } => {
                let selected = self.selected;
                let choices = self.model_choices();
                if let Some(selection) = self.model_selection() {
                    return self.select_model(target.clone(), selection.model, selection.thinking);
                } else if let Some(provider) = self
                    .connected_providers
                    .get(selected.saturating_sub(choices.len()))
                {
                    self.page = Page::Manual {
                        target: target.clone(),
                        provider: *provider,
                    };
                }
            }
            Page::Manual { target, provider } => {
                if self.query.trim().is_empty() {
                    self.status = "Enter the exact model ID.".into();
                    return Action::None;
                }
                self.page = Page::Thinking {
                    target: target.clone(),
                    model: Model {
                        provider: *provider,
                        id: self.query.clone(),
                        display_name: None,
                        efforts: None,
                        none_allowed: true,
                        limits: Default::default(),
                    },
                };
            }
            Page::Thinking { target, model } => {
                let Some(thinking) = self.levels().get(self.selected).copied() else {
                    return Action::None;
                };
                return self.select_model(target.clone(), model.clone(), thinking);
            }
            Page::Providers { login } => {
                if let Some(provider) = row.and_then(|name| name.parse::<ProviderKind>().ok()) {
                    if provider == ProviderKind::ChatGpt {
                        self.chatgpt_accounts();
                        return Action::None;
                    }
                    self.page = if *login {
                        Page::Login { provider }
                    } else {
                        Page::Logout { provider }
                    };
                }
            }
            Page::Login { provider } => {
                if *provider == ProviderKind::ChatGpt {
                    self.chatgpt_accounts();
                    return Action::None;
                }
                if self.onboarding
                    && self.secret.expose_secret().is_empty()
                    && !self.secret_overflow
                {
                    return self.connect_provider(*provider);
                }
                if *provider == ProviderKind::Ollama {
                    self.status = "Local Ollama does not accept credentials.".into();
                    return Action::None;
                }
                if self.secret_overflow
                    || !crate::auth::valid_credential(self.secret.expose_secret())
                {
                    self.status = "Use an API key or !command, up to 8192 bytes.".into();
                    return Action::None;
                }
                if self.onboarding {
                    return self.connect_provider(*provider);
                }
                return Action::SaveKey {
                    provider: *provider,
                    key: Some(std::mem::replace(&mut self.secret, "".into())),
                };
            }
            Page::Logout { provider } => {
                return Action::SaveKey {
                    provider: *provider,
                    key: None,
                };
            }
        }
        self.query.clear();
        self.thinking_choices.clear();
        self.selected = self.saved_selection().unwrap_or(0);
        self.status.clear();
        Action::None
    }

    fn select_model(
        &mut self,
        target: ModelTarget,
        model: Model,
        thinking: Option<ThinkingLevel>,
    ) -> Action {
        match target {
            ModelTarget::Session => Action::SelectModel {
                model: format!("{}/{}", model.provider, model.id),
                thinking,
            },
            ModelTarget::Choice => {
                let mut entry = self.current_entry().cloned().unwrap_or_else(|| {
                    Alias::new(model.provider.to_string(), model.id.clone(), thinking, 0)
                });
                entry.provider = model.provider.to_string();
                entry.model = model.id;
                entry.thinking_level = thinking;
                self.query = entry.priority.to_string();
                self.page = Page::Priority { entry };
                self.selected = 0;
                self.status.clear();
                Action::None
            }
        }
    }

    fn save_alias_entries(&self, entry: Option<Alias>, make_default: bool) -> Action {
        let mut entries = self
            .aliases
            .get(&self.editing_alias)
            .cloned()
            .unwrap_or_default();
        if let Some(entry) = entry {
            if let Some(index) = self.editing_entry.filter(|index| *index < entries.len()) {
                entries[index] = entry;
            } else {
                entries.push(entry);
            }
        }
        Action::SaveAliasChoices {
            name: self.editing_alias.clone(),
            entries,
            make_default,
        }
    }

    fn current_entry(&self) -> Option<&Alias> {
        self.editing_entry
            .and_then(|index| self.aliases.get(&self.editing_alias)?.get(index))
    }

    fn alias_entry_choices(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.aliases
            .get(&self.editing_alias)
            .into_iter()
            .flatten()
            .enumerate()
            .filter(|(_, entry)| {
                format!(
                    "{} {} {} {}",
                    entry.provider,
                    entry.model,
                    entry.priority,
                    thinking_label(entry.thinking_level)
                )
                .to_lowercase()
                .contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn saved_alias_names(&self) -> Vec<String> {
        let query = self.query.to_lowercase();
        config::alias_names(&self.aliases)
            .into_iter()
            .filter(|key| {
                key.to_lowercase().contains(&query)
                    || self.aliases[key].iter().any(|entry| {
                        !entry.fallback_only
                            && format!(
                                "{}/{} {}",
                                entry.provider,
                                entry.model,
                                thinking_label(entry.thinking_level)
                            )
                            .to_lowercase()
                            .contains(&query)
                    })
            })
            .collect()
    }

    fn alias_selection(&self) -> Option<(String, usize)> {
        if !matches!(self.page, Page::ModelPicker) {
            return None;
        }
        let names = self.saved_alias_names();
        let name = names.get(self.selected)?;
        let choices = config::regular_choices(self.aliases.get(name)?);
        let choice = self
            .alias_choices
            .get(name)
            .filter(|choice| choices.contains(choice))
            .or_else(|| choices.first())?;
        Some((name.clone(), *choice))
    }

    fn connect_provider(&mut self, provider: ProviderKind) -> Action {
        self.page = Page::SetupLoading { provider };
        self.status = "Checking provider and loading models...".into();
        Action::ConnectProvider {
            provider,
            key: (!self.secret.expose_secret().is_empty()).then(|| self.secret.clone()),
        }
    }

    fn save_setup(&mut self, provider: ProviderKind) -> Action {
        self.page = Page::SetupLoading { provider };
        self.status = "Saving provider settings...".into();
        Action::SaveSetup {
            provider,
            key: (!self.secret.expose_secret().is_empty()).then(|| self.secret.clone()),
        }
    }

    fn setup_models(&self, provider: ProviderKind) -> Vec<Model> {
        let query = self.query.to_lowercase();
        self.models
            .iter()
            .filter(|model| model.provider == provider && model.id.to_lowercase().contains(&query))
            .cloned()
            .collect()
    }

    fn saved_selection(&self) -> Option<usize> {
        match &self.page {
            Page::Models {
                target: ModelTarget::Choice,
            } => {
                let saved = self.current_entry()?;
                self.model_choices().iter().position(|model| {
                    model.id == saved.model && model.provider.to_string() == saved.provider
                })
            }
            Page::Thinking {
                target: ModelTarget::Choice,
                model,
            } => {
                let saved = self.current_entry()?;
                if saved.model != model.id || saved.provider != model.provider.to_string() {
                    return None;
                }
                self.levels()
                    .iter()
                    .position(|level| *level == saved.thinking_level)
            }
            _ => None,
        }
    }

    fn model_choices(&self) -> Vec<Model> {
        let mut choices = Vec::new();
        for alias in self.aliases.values().flatten() {
            if let Ok(provider) = alias.provider.parse()
                && !choices
                    .iter()
                    .any(|model: &Model| model.provider == provider && model.id == alias.model)
            {
                let metadata = self
                    .models
                    .iter()
                    .find(|model| model.provider == provider && model.id == alias.model);
                choices.push(Model {
                    provider,
                    id: alias.model.clone(),
                    display_name: metadata.and_then(|model| model.display_name.clone()),
                    efforts: metadata.and_then(|model| model.efforts.clone()),
                    none_allowed: metadata.is_none_or(|model| model.none_allowed),
                    limits: metadata.map(|model| model.limits).unwrap_or_default(),
                });
            }
        }
        for model in &self.models {
            if !choices
                .iter()
                .any(|choice| choice.provider == model.provider && choice.id == model.id)
            {
                choices.push(model.clone());
            }
        }
        let query = self.query.to_lowercase();
        choices.retain(|model| {
            format!("{} {}", model.id, model.provider)
                .to_lowercase()
                .contains(&query)
        });
        choices
    }

    fn levels(&self) -> Vec<Option<ThinkingLevel>> {
        if let Page::Thinking { model, .. } = &self.page {
            thinking_levels(model)
        } else {
            vec![None]
        }
    }

    fn model_selection(&self) -> Option<ModelSelection> {
        let mut selection = match &self.page {
            Page::Models { target } => {
                let model = self.model_choices().get(self.selected)?.clone();
                let saved = match target {
                    ModelTarget::Choice => self.current_entry(),
                    ModelTarget::Session => None,
                };
                let thinking = saved
                    .filter(|saved| {
                        saved.provider == model.provider.to_string() && saved.model == model.id
                    })
                    .and_then(|saved| saved.thinking_level);
                ModelSelection {
                    key: format!("{}/{}", model.provider, model.id),
                    model,
                    thinking,
                }
            }
            _ => return None,
        };
        let levels = thinking_levels(&selection.model);
        if let Some(thinking) = self.thinking_choices.get(&selection.key) {
            selection.thinking = *thinking;
        }
        if !levels.contains(&selection.thinking) {
            selection.thinking = None;
        }
        Some(selection)
    }

    fn rows(&self) -> Vec<(String, String)> {
        let query = self.query.to_lowercase();
        match &self.page {
            Page::Mcp => {
                let mut rows: Vec<_> = self
                    .servers
                    .iter()
                    .filter(|server| server.name.to_lowercase().contains(&query))
                    .map(|server| (server.name.clone(), server.status.clone()))
                    .collect();
                rows.push(("+ Add server".into(), String::new()));
                rows
            }
            Page::McpPresets => {
                let mut rows: Vec<_> = PRESETS
                    .iter()
                    .filter(|preset| {
                        preset.title.to_lowercase().contains(&query) || preset.name.contains(&query)
                    })
                    .map(|preset| (preset.title.into(), preset.authentication.into()))
                    .collect();
                if "custom server".contains(&query) || rows.is_empty() {
                    rows.insert(0, ("Custom server".into(), "Enter name and URL".into()));
                }
                rows
            }
            Page::McpServer { name } => {
                let enabled = self
                    .servers
                    .iter()
                    .find(|server| &server.name == name)
                    .is_some_and(|server| server.enabled);
                vec![
                    ("Available tools".into(), String::new()),
                    ("Connect / refresh".into(), String::new()),
                    ("Authenticate".into(), "Browser OAuth".into()),
                    (
                        if enabled { "Disable" } else { "Enable" }.into(),
                        "New calls".into(),
                    ),
                    ("Forget credentials".into(), "Local only".into()),
                    ("Remove server".into(), "Confirm next".into()),
                ]
            }
            Page::McpTools { name } => self
                .servers
                .iter()
                .find(|server| &server.name == name)
                .map(|server| {
                    server
                        .tools
                        .iter()
                        .filter(|tool| {
                            format!("{} {}", tool.name, tool.description)
                                .to_lowercase()
                                .contains(&query)
                        })
                        .map(|tool| {
                            (
                                tool.name.clone(),
                                tool.description
                                    .chars()
                                    .filter(|character| !character.is_control())
                                    .collect(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
            Page::McpRemove { .. } => vec![
                ("Remove server".into(), "Existing effects remain".into()),
                ("Cancel".into(), String::new()),
            ],
            Page::Accounts => {
                let mut rows: Vec<_> = self
                    .accounts
                    .iter()
                    .filter(|account| account.label.to_lowercase().contains(&query))
                    .map(|account| {
                        (
                            account.label.clone(),
                            if account.active && account.inference_enabled {
                                "Active"
                            } else if account.inference_enabled {
                                "Connected"
                            } else if account.connected {
                                "Identity only"
                            } else {
                                "Reconnect required"
                            }
                            .into(),
                        )
                    })
                    .collect();
                rows.push(("+ Add subscription".into(), String::new()));
                rows
            }
            Page::Account { .. } => vec![
                ("Use this account".into(), "New sessions only".into()),
                ("Reconnect".into(), "Browser sign-in".into()),
                ("Sign out".into(), "Revoke and remove local tokens".into()),
            ],
            Page::PlanConsent { .. } => vec![
                (
                    "Continue with ChatGPT".into(),
                    "Plan allowance / credits".into(),
                ),
                (
                    "Use OpenAI API key instead".into(),
                    "Separate API billing".into(),
                ),
            ],
            Page::SetupProviders => PROVIDERS
                .into_iter()
                .filter(|provider| provider.to_string().contains(&query))
                .map(|provider| {
                    (
                        provider.to_string(),
                        match provider {
                            ProviderKind::Ollama => "Local models · no key",
                            ProviderKind::OpenAi => "API key · not ChatGPT",
                            ProviderKind::ChatGpt => "ChatGPT plan · browser sign-in",
                            ProviderKind::OpenCodeGo => "Go subscription Console key",
                            ProviderKind::OpenCodeZen => "Zen Console key · API billing",
                            ProviderKind::KimiCode => "Kimi Code subscription key",
                            ProviderKind::Meta => "API key · not Muse subscription",
                            ProviderKind::XAi => "API key · not Grok subscription",
                            _ => "API key",
                        }
                        .into(),
                    )
                })
                .collect(),
            Page::SetupModels { provider } => {
                let mut rows: Vec<_> = self
                    .setup_models(*provider)
                    .into_iter()
                    .map(|model| (model.display_name.unwrap_or(model.id.clone()), model.id))
                    .collect();
                if *provider != ProviderKind::Ollama {
                    rows.push(("Enter model ID".into(), String::new()));
                }
                rows.push(("Retry discovery".into(), String::new()));
                rows.push(("Choose another provider".into(), String::new()));
                rows
            }
            Page::AliasEntries => {
                let mut rows: Vec<_> = self
                    .alias_entry_choices()
                    .into_iter()
                    .filter_map(|index| self.aliases.get(&self.editing_alias)?.get(index))
                    .map(|entry| {
                        (
                            format!("{}/{}", entry.provider, entry.model),
                            format!(
                                "{} {} · {}",
                                if entry.fallback_only {
                                    "fallback"
                                } else {
                                    "priority"
                                },
                                entry.priority,
                                thinking_label(entry.thinking_level)
                            ),
                        )
                    })
                    .collect();
                rows.push(("+ Add model".into(), String::new()));
                rows.push(("Use as default alias".into(), "New sessions".into()));
                rows
            }
            Page::AliasEntry => vec![
                ("Change model and effort".into(), String::new()),
                (
                    "Edit priority".into(),
                    self.current_entry()
                        .map(|entry| entry.priority.to_string())
                        .unwrap_or_default(),
                ),
                (
                    "Fallback only".into(),
                    if self
                        .current_entry()
                        .is_some_and(|entry| entry.fallback_only)
                    {
                        "Yes"
                    } else {
                        "No"
                    }
                    .into(),
                ),
                (
                    "Remove model choice".into(),
                    "Last choice removes alias".into(),
                ),
            ],
            Page::ModelPicker => {
                let mut rows: Vec<_> = self
                    .saved_alias_names()
                    .into_iter()
                    .map(|key| (key, String::new()))
                    .collect();
                rows.push((
                    if self.area.width < 44 {
                        "+ Browse models"
                    } else {
                        "+ Browse provider models"
                    }
                    .into(),
                    if self.area.width < 56 {
                        ""
                    } else {
                        "Discover or enter a model ID"
                    }
                    .into(),
                ));
                rows
            }
            Page::Aliases => {
                let mut rows: Vec<_> = config::alias_names(&self.aliases)
                    .into_iter()
                    .filter(|name| name.to_lowercase().contains(&query))
                    .filter_map(|name| {
                        self.aliases.get(&name).map(|entries| {
                            let suffix = if entries.len() == 1 { "" } else { "s" };
                            (name, format!("{} model choice{suffix}", entries.len()))
                        })
                    })
                    .collect();
                rows.push(("+ Add alias".into(), String::new()));
                rows
            }
            Page::Models { .. } => {
                let mut rows: Vec<_> = self
                    .model_choices()
                    .into_iter()
                    .map(|model| {
                        (
                            model.display_name.unwrap_or(model.id.clone()),
                            format!("{} · {}", model.provider, model.id),
                        )
                    })
                    .collect();
                rows.extend(
                    self.connected_providers
                        .iter()
                        .map(|provider| ("+ Enter model ID".into(), provider.to_string())),
                );
                rows
            }
            Page::Thinking { .. } => self
                .levels()
                .into_iter()
                .map(|level| match level {
                    None => ("default".into(), "Use model default".into()),
                    Some(level) => (thinking_label(Some(level)), String::new()),
                })
                .collect(),
            Page::Providers { login } => PROVIDERS
                .into_iter()
                .filter(|provider| !*login || *provider != ProviderKind::Ollama)
                .filter(|provider| provider.to_string().contains(&query))
                .map(|provider| (provider.to_string(), String::new()))
                .collect(),
            _ => Vec::new(),
        }
    }
}

pub(crate) fn thinking_label(level: Option<ThinkingLevel>) -> String {
    match level {
        None => "default",
        Some(ThinkingLevel::None) => "none",
        Some(ThinkingLevel::Minimal) => "minimal",
        Some(ThinkingLevel::Low) => "low",
        Some(ThinkingLevel::Medium) => "medium",
        Some(ThinkingLevel::High) => "high",
        Some(ThinkingLevel::XHigh) => "xhigh",
        Some(ThinkingLevel::Max) => "max",
    }
    .into()
}

fn thinking_levels(model: &Model) -> Vec<Option<ThinkingLevel>> {
    let mut levels = vec![None];
    let allowed: &[ThinkingLevel] = match model.provider {
        ProviderKind::Ollama => &[],
        ProviderKind::OpenCodeGo if model.efforts.is_none() => &[],
        ProviderKind::Baseten => &[
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ],
        _ => &[
            ThinkingLevel::None,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
            ThinkingLevel::Max,
        ],
    };
    for level in allowed {
        if *level == ThinkingLevel::None && !model.none_allowed {
            continue;
        }
        if model
            .efforts
            .as_ref()
            .is_none_or(|efforts| efforts.supports(*level))
        {
            levels.push(Some(*level));
        }
    }
    levels
}

fn fit_session_label(text: &str, width: u16) -> String {
    if Line::from(text).width() <= usize::from(width) {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    let mut label = String::new();
    let mut used = 0;
    for grapheme in Span::raw(text).styled_graphemes(Style::default()) {
        let length = grapheme.symbol.cell_width();
        if used + length >= width {
            break;
        }
        label.push_str(grapheme.symbol);
        used += length;
    }
    label.push('…');
    label
}
