//! Scrollable agent output above a multiline composer.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use cocoa_core::capability::{ActivityOutputKind, ActivitySummary};
use cocoa_core::engine::{ActivityStatus, EventKind, Observation};
use cocoa_core::model::{Image, ModelAction, UserInput};
use cocoa_core::store::{AgentId, InvocationId, OperationId, RecordKind, StoredRecord};
use ratatui::{
    Frame,
    buffer::{Buffer, CellWidth},
    crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    layout::{Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Widget, Wrap},
};
use ratatui_textarea::{CursorMove, DataCursor, TextArea, WrapMode};

use crate::agents::{AgentUpdate, AgentUpdateKind};
use crate::files::FileCompletion;
use crate::output::{Markdown, activity_diff};
use crate::selection::{Selection, SelectionMap, WrapLayout};
use crate::theme::Theme;

const POD_PRESS: [PodPress; 16] = [
    PodPress::Pod,
    PodPress::Pod,
    PodPress::Pod,
    PodPress::Pod,
    PodPress::Press,
    PodPress::Mold,
    PodPress::Nibs,
    PodPress::Nibs,
    PodPress::Bar,
    PodPress::Bar,
    PodPress::Bar,
    PodPress::Bar,
    PodPress::Nibs,
    PodPress::Nibs,
    PodPress::Mold,
    PodPress::Press,
];

const ACTIVITY_LABELS: [&str; 12] = [
    "Toasting",
    "Blending",
    "Refining",
    "Winnowing",
    "Conching",
    "Setting",
    "Finishing",
    "Tempering",
    "Stirring",
    "Cracking",
    "Melting",
    "Milling",
];
const ACTIVITY_LABEL_DURATION: Duration = Duration::from_millis(5760);

const THEME_COMMANDS: [(&str, Theme); 4] = [
    ("/theme dark-chocolate", Theme::DarkChocolate),
    ("/theme cacao-grove", Theme::CacaoGrove),
    ("/theme milk-chocolate", Theme::MilkChocolate),
    ("/theme terminal-native", Theme::TerminalNative),
];

const SLASH_COMMANDS: [&str; 13] = [
    "/login",
    "/logout",
    "/aliases",
    "/usage",
    "/mcp",
    "/models",
    "/copy",
    "/new",
    "/resume",
    "/rewind",
    "/sessions",
    "/theme",
    "/update",
];

/// Waits observe issued work without producing effects; the transcript shows
/// the awaited commands instead, so wait operations are tracked but hidden.
const HIDDEN_WAIT_NAME: &str = "cocoa.wait";
const HIDDEN_WAIT_TITLE: &str = "Wait commands";
const HIDDEN_WAIT_OPERATIONS: usize = 256;
const LOADER_TICK_DURATION: Duration = Duration::from_millis(120);
const STREAM_RATE_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
enum PodPress {
    Pod,
    Press,
    Mold,
    Nibs,
    Bar,
}

#[derive(PartialEq, Eq)]
enum Phase {
    Idle,
    Running,
    Stopping,
    Stopped,
}

enum Progress {
    Thinking,
    Retrying { status: u16, retry_at: Instant },
    Paused { reason: String, selected: usize },
    RunningTools,
    Streaming,
}

/// Idle submissions start an invocation; steering redirects the running one.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Redraw,
    Submit(UserInput),
    /// Queue user input for the running invocation's next model round.
    Steer(UserInput),
    /// Execute a local slash command without adding it to model history.
    Command(String),
    /// Select a saved regular choice without changing its effort or fallback list.
    SelectAliasChoice {
        alias: String,
        choice: usize,
    },
    /// Request the next regular model choice in the selected alias.
    /// The host rejects changes to an existing session.
    CycleAlias,
    /// Request the previous regular choice; existing sessions cannot change models.
    CycleAliasPrevious,
    /// Write logical transcript text to the terminal host's clipboard.
    Copy(String),
    /// Read an image or text from the local system clipboard.
    Paste,
    /// Cancel the active turn without exiting or discarding the draft.
    Cancel,
    /// Retry the paused model request without resubmitting the user input.
    Retry,
    Exit,
}

/// Invalid composer budget or initial prompt. Budgets must be 1..=8388608 bytes;
/// initial prompts must be nonblank, fit the budget and contain no controls except LF.
#[derive(Debug, thiserror::Error)]
#[error("invalid composer byte budget or initial prompt")]
pub struct ChatError;

/// Paste rejection leaves the draft unchanged.
#[derive(Debug, thiserror::Error)]
pub enum PasteError {
    #[error("Paste exceeds the configured input byte budget.")]
    InputLimit,
    #[error("A message can contain at most four images.")]
    ImageLimit,
    #[error("Clipboard text contains unsupported control characters.")]
    ControlText,
    #[error("The image path is no longer at the draft cursor.")]
    PathChanged,
}

/// An explicit local image path ending at the draft cursor. `source` retains
/// shell quoting and trailing whitespace; `path` is decoded without shell execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImagePath {
    pub source: String,
    pub path: PathBuf,
}

/// Presentation only. Engine results, not live notifications, authorize answers.
/// Retains the full session transcript and received provisional text.
/// PageUp/PageDown and the mouse wheel scroll output, not the composer.
pub struct Chat {
    composer: TextArea<'static>,
    theme: Theme,
    reduced_motion: bool,
    slash_selected: usize,
    slash_dismissed: String,
    aliases: Vec<String>,
    skills: Vec<cocoa_tools::SkillCommand>,
    selection: Selection,
    files: Option<FileCompletion>,
    previous_prompt: String,
    previous_images: Vec<Attachment>,
    images: Vec<Attachment>,
    image_sequence: u64,
    saved_draft: Option<Draft>,
    up_armed: bool,
    input_bytes: usize,
    message_steering: bool,
    phase: Phase,
    progress: Progress,
    provider_attempt: u32,
    history: VecDeque<Message>,
    preview: String,
    preview_markdown: Markdown,
    preview_layout: WrapLayout,
    invocation: Option<InvocationId>,
    hidden_wait_operations: BTreeSet<(Option<usize>, u64, OperationId)>,
    agents: Vec<AgentActivity>,
    activity_focus: Option<ActivityKey>,
    reveal_focus: bool,
    /// Click targets from the last drawn frame. Observations only rewrite the
    /// transcript, so they must not drop rows the screen still shows; a click
    /// during streaming would otherwise land on nothing.
    headers: Vec<(ActivityKey, Rect)>,
    popup: Rect,
    composer_area: Rect,
    composer_text: Rect,
    recovery_rows: [Rect; 2],
    transcript_height: usize,
    scroll: usize,
    directory: String,
    alias: String,
    thinking: Option<String>,
    branch: Option<String>,
    changes: Option<(u64, u64)>,
    cache_hit_rate: Option<f64>,
    cost_usd: Option<f64>,
    turn_elapsed: Option<Duration>,
    stream_characters: Option<u64>,
    stream_elapsed: Duration,
    stream_tokens_per_second: Option<f64>,
    input_notice: String,
    activity_elapsed: Duration,
    escape_deadline: Option<Instant>,
    exit_deadline: Option<Instant>,
    escape_notice_latest: bool,
    copy_deadline: Option<Instant>,
}

impl Chat {
    /// Create an empty composer with undo history disabled.
    ///
    /// # Errors
    /// Reject budgets outside 1..=8388608 bytes.
    pub fn new(input_bytes: usize) -> Result<Self, ChatError> {
        if !(1..=8 * 1024 * 1024).contains(&input_bytes) {
            return Err(ChatError);
        }
        Ok(Self {
            composer: new_composer(),
            theme: Theme::default(),
            reduced_motion: false,
            slash_selected: 0,
            slash_dismissed: String::new(),
            aliases: Vec::new(),
            skills: Vec::new(),
            selection: Selection::default(),
            files: None,
            previous_prompt: String::new(),
            previous_images: Vec::new(),
            images: Vec::new(),
            image_sequence: 0,
            saved_draft: None,
            up_armed: false,
            input_bytes,
            message_steering: true,
            phase: Phase::Idle,
            progress: Progress::Thinking,
            provider_attempt: 0,
            history: VecDeque::new(),
            preview: String::new(),
            preview_markdown: Markdown::default(),
            preview_layout: WrapLayout::default(),
            invocation: None,
            hidden_wait_operations: BTreeSet::new(),
            agents: Vec::new(),
            activity_focus: None,
            reveal_focus: false,
            headers: Vec::new(),
            popup: Rect::default(),
            composer_area: Rect::default(),
            composer_text: Rect::default(),
            recovery_rows: [Rect::default(); 2],
            transcript_height: 0,
            scroll: 0,
            directory: String::from("."),
            alias: String::new(),
            thinking: None,
            branch: None,
            changes: None,
            cache_hit_rate: None,
            cost_usd: None,
            turn_elapsed: None,
            stream_characters: None,
            stream_elapsed: Duration::ZERO,
            stream_tokens_per_second: None,
            input_notice: String::new(),
            activity_elapsed: Duration::ZERO,
            escape_deadline: None,
            exit_deadline: None,
            escape_notice_latest: false,
            copy_deadline: None,
        })
    }

    /// Change presentation without discarding history, draft, selection or activity.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Use a static chocolate bar and busy word, without changing activity status.
    pub fn set_reduced_motion(&mut self, reduced_motion: bool) {
        self.reduced_motion = reduced_motion;
    }

    /// Current presentation theme, independent of the session's execution profile.
    #[must_use]
    pub fn theme(&self) -> Theme {
        self.theme
    }

    /// Update the composer budget after the host selects an execution profile.
    ///
    /// # Errors
    /// Reject budgets outside 1..=8388608 bytes or smaller than the current draft.
    pub fn set_input_bytes(&mut self, input_bytes: usize) -> Result<(), ChatError> {
        if !(1..=8 * 1024 * 1024).contains(&input_bytes) || self.draft_bytes() > input_bytes {
            return Err(ChatError);
        }
        self.input_bytes = input_bytes;
        Ok(())
    }

    /// Set display-only context, removing controls and bounding directory/alias
    /// labels to 4096/64 bytes. A direct `provider/model` selection drops the
    /// provider prefix. This does not change engine configuration.
    pub fn set_context(&mut self, directory: &str, alias: &str) {
        self.directory = display_label(directory, 4096);
        self.alias = display_label(model_label(alias), 64);
    }

    /// Show a direct model's reasoning effort beside the model label, bounded to
    /// 16 bytes. The composer title drops the indicator before it drops the model.
    pub fn set_thinking(&mut self, thinking: Option<&str>) {
        self.thinking = thinking
            .map(|thinking| display_label(thinking, 16))
            .filter(|thinking| !thinking.is_empty());
    }

    /// Set Git labels without doing I/O. Missing changes mean unknown, not clean.
    /// Branch text is sanitized and bounded to 256 bytes.
    pub fn set_workspace(&mut self, branch: Option<&str>, changes: Option<(u64, u64)>) {
        self.branch = branch
            .map(|branch| display_label(branch, 256))
            .filter(|branch| !branch.is_empty());
        self.changes = changes;
    }

    /// Show the latest cache hit percentage and approximate session API cost.
    /// The host must omit subscription prices and unknown counters or rates.
    /// Invalid percentages and nonfinite or negative costs are hidden.
    pub fn set_usage(&mut self, cache_hit_rate: Option<f64>, cost_usd: Option<f64>) {
        self.cache_hit_rate = cache_hit_rate.filter(|rate| (0.0..=100.0).contains(rate));
        self.cost_usd = cost_usd.filter(|cost| cost.is_finite() && *cost >= 0.0);
    }

    /// Set discovered skill commands, retaining at most 64 entries.
    /// Skills use `$name` references; duplicate skill names cannot be invoked.
    pub fn set_skills(&mut self, skills: Vec<cocoa_tools::SkillCommand>) {
        self.skills = skills.into_iter().take(64).collect();
        self.slash_selected = 0;
    }

    /// Set `/model` and `/models` completion candidates in display order, without
    /// selecting an alias. Retain valid names among the first 32 entries: 1–64
    /// ASCII letters, digits, underscores or hyphens. The host validates selection.
    pub fn set_aliases(&mut self, aliases: &[String]) {
        self.aliases = completion_names(aliases);
        self.slash_selected = 0;
    }

    /// Show copy feedback for 1.5 seconds after the terminal driver writes the
    /// clipboard command. This does not acknowledge terminal clipboard acceptance.
    pub fn confirm_copy(&mut self, now: Instant) {
        self.copy_deadline = Some(now + Duration::from_millis(1500));
    }

    /// Insert clipboard text literally. Newlines never submit a message.
    ///
    /// # Errors
    /// Reject controls other than line breaks and tabs, or input beyond the
    /// combined text and image budget. CRLF and CR normalize to LF; tabs to spaces.
    pub fn paste_text(&mut self, text: &str) -> Result<(), PasteError> {
        if text
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        {
            return Err(PasteError::ControlText);
        }
        let text = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ");
        if self.draft_bytes().saturating_add(text.len()) > self.input_bytes {
            return Err(PasteError::InputLimit);
        }
        self.composer.insert_str(text);
        self.prune_images();
        self.saved_draft = None;
        self.selection.invalidate();
        self.reset_gestures();
        Ok(())
    }

    /// Attach a clipboard image and insert its marker at the cursor. Removing
    /// the marker removes the attachment. Images may be drafted while busy.
    ///
    /// # Errors
    /// Reject more than four images or input beyond the combined byte budget.
    pub fn paste_image(&mut self, image: Image) -> Result<(), PasteError> {
        self.insert_image(image, "")
    }

    /// Enable or disable steering while running. Enabled by default; stopping
    /// and stopped views never submit, regardless of this setting.
    pub fn set_message_steering(&mut self, enabled: bool) {
        self.message_steering = enabled;
    }

    /// Whether Enter may submit or steer, including attaching a pending image path.
    #[must_use]
    pub fn can_submit(&self) -> bool {
        !matches!(self.progress, Progress::Paused { .. })
            && (self.phase == Phase::Idle
                || (self.phase == Phase::Running && self.message_steering))
    }

    /// Pause model submission and show Retry above Cancel. The reason must be a
    /// sanitized host-authored message, never a raw provider diagnostic.
    /// Repeating the displayed reason preserves the selected recovery action.
    pub fn provider_paused(&mut self, reason: &str) {
        let reason = display_label(reason, 1024);
        if matches!(&self.progress, Progress::Paused { reason: current, .. } if current == &reason)
        {
            return;
        }
        self.preview.clear();
        self.activity_focus = None;
        self.selection.invalidate();
        self.reset_gestures();
        self.progress = Progress::Paused {
            reason,
            selected: 0,
        };
    }

    /// Remove the recovery choices before requesting another provider response.
    pub fn provider_resumed(&mut self) {
        self.progress = Progress::Thinking;
        self.provider_attempt = 0;
        self.recovery_rows = [Rect::default(); 2];
    }

    /// Restore the most recent steering draft when the host rejects queue admission.
    /// Call immediately after `Action::Steer`, before accepting more editor input.
    pub fn reject_steering(&mut self, diagnostic: &str) {
        self.history.pop_back();
        self.composer.insert_str(&self.previous_prompt);
        self.images.clone_from(&self.previous_images);
        self.input_notice(diagnostic);
    }

    /// Recognize a PNG, JPEG, GIF or WebP path ending at the cursor, without
    /// reading the file. Accept absolute paths, `./`, `../`, `~/` and local file
    /// URLs, including shell quotes and backslash escapes. Ignore `@` mentions.
    /// Paths are limited to 4096 bytes. A stopped view never requests attachments.
    #[must_use]
    pub fn image_path(&self) -> Option<ImagePath> {
        if self.phase == Phase::Stopped || self.composer.selection_range().is_some() {
            return None;
        }
        let DataCursor(row, column) = self.composer.cursor();
        let line = &self.composer.lines()[row];
        let end = line
            .char_indices()
            .nth(column)
            .map_or(line.len(), |(offset, _)| offset);
        image_path(&line[..end])
    }

    /// Replace a recognized path with an attachment, preserving trailing spaces.
    ///
    /// # Errors
    /// Reject a changed cursor/path, more than four images or an oversized input.
    /// Rejection leaves the path and draft unchanged.
    pub fn paste_image_path(&mut self, path: &ImagePath, image: Image) -> Result<(), PasteError> {
        if self.image_path().as_ref() != Some(path) {
            return Err(PasteError::PathChanged);
        }
        self.insert_image(image, &path.source)
    }

    /// Enable `@` completion beneath the launch directory. Selection inserts only
    /// path text; it does not load file contents or change the engine's context.
    /// Discovery skips hidden and ignored entries and does not follow symlinks.
    pub fn set_working_directory(&mut self, directory: &Path) {
        self.reset_gestures();
        self.files = Some(FileCompletion::new(directory));
    }

    /// Advance activity by elapsed monotonic time, retaining subframe precision.
    /// Also advances bounded file discovery while a mention is active. Returns
    /// whether the view changed. Delayed ticks skip loader poses rather than
    /// slowing the animation. Retry waits, pauses and stopping do not animate;
    /// stopped views also stop discovering paths.
    pub fn tick(&mut self, elapsed: Duration) -> bool {
        self.tick_at(elapsed, Instant::now())
    }

    /// Advance presentation using the input owner's monotonic clock.
    pub fn tick_at(&mut self, elapsed: Duration, now: Instant) -> bool {
        let mut expired = self.expire_gestures(now);
        let mut timing_changed = false;
        if matches!(self.phase, Phase::Running | Phase::Stopping) {
            if let Some(turn_elapsed) = &mut self.turn_elapsed {
                let previous = turn_elapsed.as_secs();
                *turn_elapsed = turn_elapsed.saturating_add(elapsed);
                timing_changed = turn_elapsed.as_secs() != previous;
            }
            if let Some(characters) = self.stream_characters {
                let previous = self.stream_elapsed.as_secs() / STREAM_RATE_INTERVAL.as_secs();
                self.stream_elapsed = self.stream_elapsed.saturating_add(elapsed);
                if self.stream_elapsed.as_secs() / STREAM_RATE_INTERVAL.as_secs() != previous {
                    self.stream_tokens_per_second =
                        Some(characters as f64 / 4.0 / self.stream_elapsed.as_secs_f64());
                    timing_changed = true;
                }
            }
        }
        timing_changed &= self.phase == Phase::Running
            && matches!(
                self.progress,
                Progress::Thinking | Progress::Streaming | Progress::RunningTools
            );
        if self.copy_deadline.is_some_and(|deadline| now >= deadline) {
            self.copy_deadline = None;
            expired = true;
        }
        let files_changed = self.phase != Phase::Stopped
            && self.activity_focus.is_none()
            && self
                .files
                .as_mut()
                .is_some_and(|files| files.tick(&self.composer));
        let ongoing = self.history.iter().any(|message| {
            matches!(
                &message.kind,
                MessageKind::Activity(Activity {
                    state: ActivityState::Ongoing,
                    ..
                })
            )
        });
        let retry_changed = match self.progress {
            Progress::Retrying { retry_at, .. } if self.phase == Phase::Running => {
                now.checked_sub(elapsed).is_none_or(|previous| {
                    retry_seconds(retry_at, now) != retry_seconds(retry_at, previous)
                })
            }
            _ => false,
        };
        if self.loader().is_none()
            || self.reduced_motion
            || (self.phase != Phase::Running && !ongoing)
        {
            return files_changed || expired || retry_changed || timing_changed;
        }
        let previous = (self.loader(), self.activity_label());
        let cycle = ACTIVITY_LABEL_DURATION.as_nanos() * ACTIVITY_LABELS.len() as u128;
        let elapsed = (self.activity_elapsed.as_nanos() + elapsed.as_nanos()) % cycle;
        self.activity_elapsed = Duration::from_nanos(elapsed as u64);
        (self.loader(), self.activity_label()) != previous
            || retry_changed
            || files_changed
            || expired
            || timing_changed
    }

    /// Handle keys, mouse-wheel scrolling and resize. Enter submits; Ctrl+Enter
    /// inserts a newline. Ctrl+J is a fallback without modified Enter support.
    /// Up recalls the last submitted prompt from an empty composer. With a draft,
    /// a consecutive Up at the first visual row recalls it; Down restores the saved
    /// draft and cursor. Editing the recalled prompt ends history navigation.
    /// Option+Delete and Ctrl+W delete the previous word using editor boundaries.
    /// Command+Backspace and Ctrl+U clear the entire draft regardless of cursor.
    /// Command+Left/Right move to the start/end of the draft; Option+Left/Right
    /// move backward/forward by one word using the same editor boundaries.
    /// An active `@` picker consumes arrows, Enter/Tab selection and Escape.
    /// Otherwise Tab focuses activity headers, Up/Down selects, Right expands,
    /// Left collapses, Enter/Space toggles and Escape returns to the intact draft.
    /// Ctrl+N/P act as Down/Up in the picker and focused activity list.
    /// Activity navigation remains available after completion or failure.
    /// Left clicks and mouse-wheel scrolling return keyboard input to the draft.
    /// A left click in the composer moves the cursor to the clicked text cell.
    /// Clicking an activity header toggles its details without taking focus.
    /// Enter closes on an exact draft of `exit`, `:q` or `:x`, including while busy.
    /// While running, Enter steers unless disabled; local slash commands run at once.
    /// A paused provider preserves the draft and accepts clicks, R to retry,
    /// C or Escape to cancel, or Up/Down/Tab followed by Enter to choose a row.
    /// Ctrl+T requests model cycling within the selected alias while idle, preserving the draft.
    /// Ctrl+P opens the model picker while idle, unless a list already owns it.
    /// Stopped views reject edits and submissions.
    /// Left-drag selects displayed text; release requests copying without visual
    /// wraps or padding. Double-click copies a word; triple-click copies its
    /// paragraph, bounded by blank lines. A single click does not copy.
    /// View changes clear selection and pending clicks.
    /// Ctrl+V and Command+V request clipboard paste when forwarded by the terminal.
    /// Bracketed paste inserts its text literally, including line breaks, and never
    /// submits. Other control text and undo shortcuts are rejected.
    pub fn input(&mut self, event: Event) -> Action {
        self.input_at(event, Instant::now())
    }

    /// Interpret input at a monotonic instant. Confirmation windows are exclusive
    /// at one second for Esc and three seconds for Ctrl+C. Reported repeats and
    /// releases cannot arm or confirm either gesture. Focus changes and editor
    /// actions reset both windows. Popup Escape takes precedence over cancellation.
    pub fn input_at(&mut self, event: Event, now: Instant) -> Action {
        let mut changed = self.expire_gestures(now);
        let reset = match &event {
            Event::FocusGained | Event::FocusLost => true,
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                !matches!(key.code, KeyCode::Esc | KeyCode::PageUp | KeyCode::PageDown)
                    && !(key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
            }
            _ => false,
        };
        if reset {
            changed |= self.reset_gestures();
            changed |= !self.input_notice.is_empty();
            self.input_notice.clear();
        }
        let action = self.input_event(event, now);
        self.prune_images();
        match action {
            Action::None if changed => Action::Redraw,
            action => action,
        }
    }

    /// Apply preview notifications. Completion previews never authorize
    /// an answer. Older or idle invocations may update existing capability rows,
    /// but cannot add history or replace the current invocation's answer.
    pub fn observe(&mut self, observation: &Observation) {
        let Observation::Event(event) = observation else {
            return;
        };
        let retained_update = match event.kind {
            EventKind::CapabilityOutput { operation, .. }
            | EventKind::CapabilityFinished { operation, .. } => {
                self.history.iter().any(|message| {
                    matches!(
                        &message.kind,
                        MessageKind::Activity(activity) if activity.key == ActivityKey {
                            invocation: event.invocation,
                            agent: None,
                            kind: ActivityKind::Tool { operation },
                        }
                    )
                })
            }
            _ => false,
        };
        if !retained_update {
            if !matches!(self.phase, Phase::Running | Phase::Stopping)
                || self
                    .invocation
                    .is_some_and(|invocation| invocation != event.invocation)
            {
                return;
            }
            self.invocation = Some(event.invocation);
        }
        let key = |kind| ActivityKey {
            invocation: event.invocation,
            agent: None,
            kind,
        };
        self.selection.invalidate();
        let paused = if matches!(self.progress, Progress::Paused { .. }) {
            Some(std::mem::replace(&mut self.progress, Progress::Thinking))
        } else {
            None
        };
        match &event.kind {
            EventKind::SteeringApplied => self.preview.clear(),
            EventKind::ModelStarted { .. } => {
                self.progress = Progress::Thinking;
                self.provider_attempt = 0;
                self.stream_characters = None;
                self.stream_elapsed = Duration::ZERO;
                self.stream_tokens_per_second = None;
            }
            EventKind::ProviderRetryScheduled { status, retry_at } => {
                self.provider_attempt += 1;
                self.progress = Progress::Retrying {
                    status: *status,
                    retry_at: *retry_at,
                };
            }
            EventKind::ProviderRetryStarted => self.progress = Progress::Thinking,
            EventKind::ModelFinished { executing } => {
                self.stream_characters = None;
                self.stream_tokens_per_second = None;
                if *executing {
                    self.progress = Progress::RunningTools;
                    self.preview.clear();
                }
            }
            EventKind::ReasoningStarted { .. } => {
                self.progress = Progress::Thinking;
            }
            EventKind::ReasoningText { text, .. } => {
                self.progress = Progress::Thinking;
                self.stream_characters = Some(
                    self.stream_characters
                        .unwrap_or(0)
                        .saturating_add(text.chars().count() as u64),
                );
            }
            EventKind::AssistantText { text, .. } => {
                self.progress = Progress::Streaming;
                self.stream_characters = Some(
                    self.stream_characters
                        .unwrap_or(0)
                        .saturating_add(text.chars().count() as u64),
                );
                self.preview.extend(sanitized(text));
            }
            EventKind::CapabilityStarted { operation, .. }
            | EventKind::CapabilityInput { operation, .. }
            | EventKind::CapabilityOutput { operation, .. }
            | EventKind::CapabilityFinished { operation, .. } => {
                if matches!(
                    &event.kind,
                    EventKind::CapabilityStarted { .. } | EventKind::CapabilityInput { .. }
                ) {
                    self.progress = Progress::RunningTools;
                }
                self.observe_capability(
                    key(ActivityKind::Tool {
                        operation: *operation,
                    }),
                    &event.kind,
                );
            }
            EventKind::CellStarted { .. } => {
                self.progress = Progress::RunningTools;
            }
            EventKind::AnswerCompleted { .. }
            | EventKind::ReasoningFinished { .. }
            | EventKind::ExecutionState { .. }
            | EventKind::CellFinished { .. }
            | EventKind::Printed { .. } => {}
        }
        if let Some(paused) = paused {
            self.progress = paused;
        }
    }

    /// Apply display-only updates for one session-scoped named agent. Child text
    /// and completion never replace the main answer or change composer state.
    /// Updates continue to reach their original work group after the parent turn.
    pub fn observe_agent(&mut self, update: &AgentUpdate) {
        let agent = match self
            .agents
            .iter()
            .position(|agent| agent.session == update.session && agent.id == update.agent)
        {
            Some(agent) => agent,
            None => {
                let Some(invocation) = self.invocation else {
                    return;
                };
                let parent = ActivityKey {
                    invocation,
                    agent: None,
                    kind: ActivityKind::Work,
                };
                self.insert_activity(parent, None, "Work");
                if !matches!(self.phase, Phase::Running | Phase::Stopping) {
                    self.finish_activity(parent, ActivityStatus::Completed);
                }
                let agent = self.agents.len();
                let key = ActivityKey {
                    invocation,
                    agent: Some(agent),
                    kind: ActivityKind::Agent,
                };
                let mut title = String::with_capacity(update.name.len());
                for word in update.name.split(['-', '_']) {
                    let mut characters = word.chars();
                    if let Some(first) = characters.next() {
                        if !title.is_empty() {
                            title.push(' ');
                        }
                        title.push(first.to_ascii_uppercase());
                        title.extend(characters.map(|character| character.to_ascii_lowercase()));
                    }
                }
                if title.is_empty() {
                    title.push_str(&update.name);
                }
                self.insert_activity(key, Some(parent), &title);
                self.agents.push(AgentActivity {
                    session: update.session,
                    id: update.agent,
                    name: update.name.clone(),
                    key,
                });
                self.finish_activity(key, ActivityStatus::Running);
                self.hide_spawn_activity(key, &update.name);
                agent
            }
        };
        self.selection.invalidate();
        self.headers.clear();
        let group = self.agents[agent].key;
        match &update.kind {
            AgentUpdateKind::Started => {}
            AgentUpdateKind::ProviderPaused { reason } => {
                let index = self.ensure_activity(group, "Agent");
                if let MessageKind::Activity(activity) = &mut self.history[index].kind {
                    activity.provider_notice =
                        Some(format!("Provider paused · {}", display_label(reason, 256)));
                }
            }
            AgentUpdateKind::ProviderResumed => {
                let index = self.ensure_activity(group, "Agent");
                if let MessageKind::Activity(activity) = &mut self.history[index].kind {
                    activity.provider_notice = None;
                    activity.provider_attempt = 0;
                }
            }
            AgentUpdateKind::Finished(status) => {
                let index = self.ensure_activity(group, "Agent");
                if let MessageKind::Activity(activity) = &mut self.history[index].kind {
                    activity.provider_notice = None;
                }
                self.finish_activity(group, *status);
                for message in &mut self.history {
                    if let MessageKind::Activity(activity) = &mut message.kind
                        && activity.key.agent == Some(agent)
                        && activity.state.is_running()
                    {
                        activity.state =
                            ActivityState::Finished(if *status == ActivityStatus::Completed {
                                ActivityStatus::Unknown
                            } else {
                                *status
                            });
                    }
                }
                self.collapse_finished_work();
            }
            AgentUpdateKind::Observation(Observation::Event(event)) => {
                let key = |kind| ActivityKey {
                    invocation: event.invocation,
                    agent: Some(agent),
                    kind,
                };
                match &event.kind {
                    EventKind::ModelStarted { .. } | EventKind::ProviderRetryStarted => {
                        let index = self.ensure_activity(group, "Agent");
                        if let MessageKind::Activity(activity) = &mut self.history[index].kind {
                            activity.provider_notice = None;
                            if matches!(event.kind, EventKind::ModelStarted { .. }) {
                                activity.provider_attempt = 0;
                            }
                        }
                    }
                    EventKind::ProviderRetryScheduled { status, .. } => {
                        let index = self.ensure_activity(group, "Agent");
                        if let MessageKind::Activity(activity) = &mut self.history[index].kind {
                            activity.provider_attempt += 1;
                            activity.provider_notice = Some(format!(
                                "Retrying provider request {}/3 · HTTP {status}",
                                activity.provider_attempt,
                            ));
                        }
                    }
                    EventKind::AnswerCompleted { text, .. } => {
                        let key = key(ActivityKind::AgentAnswer);
                        let index = self.ensure_activity(key, "Agent answer");
                        self.history[index].text.clear();
                        self.activity_text(key, text);
                        self.finish_activity(key, ActivityStatus::Completed);
                    }
                    EventKind::CapabilityStarted { operation, .. }
                    | EventKind::CapabilityInput { operation, .. }
                    | EventKind::CapabilityOutput { operation, .. }
                    | EventKind::CapabilityFinished { operation, .. } => {
                        self.observe_capability(
                            key(ActivityKind::Tool {
                                operation: *operation,
                            }),
                            &event.kind,
                        );
                    }
                    _ => {}
                }
            }
            AgentUpdateKind::Observation(Observation::Lagged { .. }) => {
                let key = ActivityKey {
                    kind: ActivityKind::AgentNotice,
                    ..group
                };
                let index = self.ensure_activity(key, "Activity updates omitted");
                self.history[index].text = "Some live agent activity was omitted.".into();
                self.finish_activity(key, ActivityStatus::Unknown);
            }
            AgentUpdateKind::Observation(Observation::Closed) => {}
        }
    }

    /// Replace all provisional text with an authoritative successful run result.
    /// Preserves the draft and cursor. Calls outside an active submission have
    /// no effect.
    pub fn complete(&mut self, answer: &str) {
        if !matches!(self.phase, Phase::Running | Phase::Stopping) {
            return;
        }
        self.preview.clear();
        if let Some(invocation) = self.invocation {
            self.finish_activity(
                ActivityKey {
                    invocation,
                    agent: None,
                    kind: ActivityKind::Work,
                },
                ActivityStatus::Completed,
            );
        }
        self.finish_activities(ActivityStatus::Unknown);
        self.append_history(MessageKind::Assistant, answer);
        self.phase = Phase::Idle;
        self.escape_deadline = None;
        self.scroll = 0;
    }

    /// Display a local command result without sending it to the engine.
    pub fn notice(&mut self, message: &str) {
        self.append_history(MessageKind::Notice, message);
        self.scroll = 0;
    }

    /// Show a local warning above the composer without adding it to the transcript.
    /// Replaces the previous warning, sanitizes and bounds it to 4096 bytes, and
    /// clears it on the next editor action or focus change.
    pub fn input_notice(&mut self, message: &str) {
        self.selection.invalidate();
        self.input_notice = display_label(message, 4096);
    }

    /// Enable submission and append a setup error before any model call.
    /// Keep the submitted message in history and leave the current draft unchanged.
    pub fn reject(&mut self, diagnostic: &str) {
        self.phase = Phase::Idle;
        self.append_history(MessageKind::Error, diagnostic);
        self.scroll = 0;
    }

    /// Set the initial prompt, bounded by the same composer byte limit.
    ///
    /// # Errors
    /// Reject blank, oversized or terminal-control input.
    pub fn initial_prompt(&mut self, prompt: &str) -> Result<Action, ChatError> {
        if prompt.trim().is_empty()
            || prompt.len() > self.input_bytes
            || prompt
                .chars()
                .any(|character| character.is_control() && character != '\n')
        {
            return Err(ChatError);
        }
        self.composer.insert_str(prompt);
        Ok(self.input(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))))
    }

    /// Acknowledge cancellation immediately; only authoritative settlement may
    /// enable another submission. The draft remains editable while stopping.
    pub fn stopping(&mut self) {
        if self.phase != Phase::Stopped {
            self.phase = Phase::Stopping;
        }
        if matches!(self.progress, Progress::Paused { .. }) {
            self.provider_resumed();
        }
        self.escape_deadline = None;
    }

    /// Finish a successfully settled cancellation without adding a message.
    /// Preserve the draft and enable another submission.
    pub fn cancelled(&mut self) {
        self.preview.clear();
        self.finish_activities(ActivityStatus::Interrupted);
        self.phase = Phase::Idle;
        self.escape_deadline = None;
        self.scroll = 0;
    }

    /// Discard previews and permanently disable submission after a failed run.
    /// Pass a sanitized diagnostic, never a raw provider or transport error.
    pub fn fail(&mut self, diagnostic: &str) {
        self.reset_gestures();
        self.preview.clear();
        self.finish_activities(ActivityStatus::Unknown);
        self.append_history(MessageKind::Error, diagnostic);
        self.phase = Phase::Stopped;
        self.scroll = 0;
    }

    /// Enable submission after the host has closed a failed session successfully.
    /// Preserve the displayed transcript and draft, but report that the next
    /// engine will have no conversation history or retained JavaScript bindings.
    /// Pass a sanitized diagnostic, never a raw provider or transport error.
    pub fn reset_session(&mut self, diagnostic: &str) {
        self.fail(diagnostic);
        self.notice("Session reset. Model conversation and JavaScript bindings cleared. Existing effects retained. Retry or update /aliases and /login.");
        self.phase = Phase::Idle;
    }

    /// Enable submission after a failure that kept its durable session.
    /// The next prompt resumes the same conversation over a fresh JavaScript
    /// runtime, so the transcript and draft stay authoritative.
    /// Pass a sanitized diagnostic, never a raw provider or transport error.
    pub fn retained_session(&mut self, diagnostic: &str) {
        self.fail(diagnostic);
        self.notice("Conversation retained; the JavaScript runtime restarted. Existing effects remain. Read files again before mutating.");
        self.phase = Phase::Idle;
    }

    /// Restore a persisted prompt and its inline images to the editor.
    ///
    /// # Errors
    /// Reject inputs exceeding the editor's byte or attachment limits.
    pub fn restore_prompt(&mut self, input: &UserInput) -> Result<(), PasteError> {
        if input.bytes() > self.input_bytes {
            return Err(PasteError::InputLimit);
        }
        if input.images().len() > 4 {
            return Err(PasteError::ImageLimit);
        }
        self.composer = new_composer();
        self.composer.insert_str(input.text());
        self.images.clear();
        self.saved_draft = None;
        let mut markers = Vec::new();
        for (offset, _) in input.text().match_indices("[Image ") {
            let remainder = &input.text()[offset + 7..];
            if let Some((number, _)) = remainder.split_once(']')
                && let Ok(sequence) = number.parse::<u64>()
            {
                self.image_sequence = self.image_sequence.max(sequence);
                let marker = format!("[Image {sequence}]");
                if !markers.contains(&marker) {
                    markers.push(marker);
                }
            }
        }
        for (index, image) in input.images().iter().enumerate() {
            if let Some(marker) = markers.get(index) {
                self.images.push(Attachment {
                    marker: marker.clone(),
                    image: image.clone(),
                });
            } else {
                self.insert_image(image.clone(), "")?;
            }
        }
        self.reset_gestures();
        Ok(())
    }

    /// Whether switching sessions would discard text or image attachments.
    #[must_use]
    pub fn has_draft(&self) -> bool {
        !self.images.is_empty() || self.composer.lines().iter().any(|line| !line.is_empty())
    }

    /// Whether delegated work still has an unsettled display status.
    #[must_use]
    pub fn has_active_agents(&self) -> bool {
        self.history.iter().any(|message| matches!(&message.kind, MessageKind::Activity(activity) if activity.key.agent.is_some() && activity.state.is_running()))
    }

    /// Replace the visible conversation with canonical root history, never stream previews.
    /// Call only after the host has settled the old runtime and confirmed draft discard.
    /// Operation rows use method names because live display metadata is not retained.
    pub fn restore_transcript(&mut self, records: &[StoredRecord]) {
        self.composer = new_composer();
        self.images.clear();
        self.previous_prompt.clear();
        self.previous_images.clear();
        self.saved_draft = None;
        self.up_armed = false;
        self.turn_elapsed = None;
        self.stream_characters = None;
        self.stream_elapsed = Duration::ZERO;
        self.stream_tokens_per_second = None;
        self.refresh_transcript(records);
    }

    /// Refresh canonical root history after the host settles the old runtime.
    /// Preserve the draft, attachments, cursor and prompt recall state. Discard
    /// previews and live activity metadata, reset scrolling, and enable submission.
    pub fn refresh_transcript(&mut self, records: &[StoredRecord]) {
        self.history.clear();
        self.preview.clear();
        self.agents.clear();
        self.hidden_wait_operations.clear();
        self.activity_focus = None;
        self.reveal_focus = false;
        self.headers.clear();
        self.selection.invalidate();
        self.invocation = None;
        self.phase = Phase::Idle;
        self.progress = Progress::Thinking;
        self.recovery_rows = [Rect::default(); 2];
        self.scroll = 0;
        self.transcript_height = 0;
        self.input_notice.clear();
        self.reset_gestures();
        for stored in records {
            self.invocation = Some(stored.record.invocation);
            match &stored.record.kind {
                RecordKind::InputAccepted { input } => {
                    self.append_history(MessageKind::User, input.text());
                    if !input.images().is_empty() {
                        self.notice(&format!(
                            "{} retained image attachment(s)",
                            input.images().len()
                        ));
                    }
                }
                RecordKind::ModelCompleted { response, .. } => {
                    if let ModelAction::Final { answer } = &response.action {
                        self.append_history(MessageKind::Assistant, answer);
                    }
                }
                RecordKind::OperationStarted {
                    operation, name, ..
                } => {
                    self.ensure_activity(
                        ActivityKey {
                            invocation: stored.record.invocation,
                            agent: None,
                            kind: ActivityKind::Tool {
                                operation: *operation,
                            },
                        },
                        name,
                    );
                }
                RecordKind::OperationFinished { operation, outcome } => {
                    let key = ActivityKey {
                        invocation: stored.record.invocation,
                        agent: None,
                        kind: ActivityKind::Tool {
                            operation: *operation,
                        },
                    };
                    match serde_json::to_string(outcome) {
                        Ok(text) => self.activity_text(key, &display_label(&text, 8192)),
                        Err(error) => self.notice(&format!(
                            "Cannot display retained operation details: {error}"
                        )),
                    }
                    self.finish_activity(key, ActivityStatus::from(outcome));
                }
                RecordKind::CellCompleted { outcome, .. } => {
                    if !matches!(
                        outcome,
                        cocoa_core::execution::CellResponse::Completed { .. }
                    ) {
                        self.notice(&format!(
                            "Retained JavaScript feedback: {}",
                            match outcome {
                                cocoa_core::execution::CellResponse::Interrupted { .. } =>
                                    "interrupted; external effects may remain",
                                cocoa_core::execution::CellResponse::NotStarted =>
                                    "cell did not run",
                                cocoa_core::execution::CellResponse::Failed { .. } =>
                                    "cell failed; earlier effects may remain",
                                cocoa_core::execution::CellResponse::Completed { .. } =>
                                    unreachable!(),
                            }
                        ));
                    }
                }
                RecordKind::InvocationFinished { outcome } => {
                    let status = match outcome {
                        cocoa_core::store::InvocationOutcome::Completed => {
                            ActivityStatus::Completed
                        }
                        cocoa_core::store::InvocationOutcome::Failed { .. } => {
                            ActivityStatus::Failed { exit_code: None }
                        }
                        cocoa_core::store::InvocationOutcome::Cancelled
                        | cocoa_core::store::InvocationOutcome::Interrupted => {
                            ActivityStatus::Interrupted
                        }
                    };
                    self.finish_activity(
                        ActivityKey {
                            invocation: stored.record.invocation,
                            agent: None,
                            kind: ActivityKind::Work,
                        },
                        status,
                    );
                    if *outcome == cocoa_core::store::InvocationOutcome::Interrupted {
                        self.notice("Previous invocation interrupted. No work was replayed.");
                    }
                }
                RecordKind::TurnCancelled { .. } => self.notice("Previous turn cancelled."),
                RecordKind::CellStarted { .. } => {}
            }
        }
        self.finish_activities(ActivityStatus::Unknown);
        self.phase = Phase::Idle;
    }

    /// Render with OSC 8 hyperlinks around visible HTTP(S) transcript URLs.
    /// Destinations remain visible, and selection copies text without escape sequences.
    /// URLs longer than 2048 bytes and composer text are not linked. Use only with
    /// a backend that writes terminal escape sequences, such as Crossterm.
    pub fn render_terminal(&mut self, frame: &mut Frame<'_>) {
        self.render(frame);
        self.selection.render_hyperlinks(frame.buffer_mut());
    }

    /// Render plain output and the editable composer, including narrow terminals.
    pub fn render(&mut self, frame: &mut Frame<'_>) {
        let palette = self.theme.palette();
        frame.render_widget(Block::new().style(self.theme.style()), frame.area());
        self.composer.set_style(self.theme.style());
        if self.phase != Phase::Stopped
            && !matches!(self.progress, Progress::Paused { .. })
            && self.activity_focus.is_none()
            && let Some(files) = &mut self.files
        {
            files.update(&self.composer);
        }
        let area = frame.area();
        let padding_vertical = u16::from(area.height >= 10);
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(palette.border))
            .padding(Padding::new(2, 2, padding_vertical, padding_vertical));
        let composer_height_max = 8.min(area.height / 2 + 1).min(area.height);
        // TextArea invalidates its wrap map when the measured origin changes.
        let measurement = Rect::new(
            area.x,
            area.bottom().saturating_sub(composer_height_max),
            area.width,
            composer_height_max,
        );
        let content = block.inner(measurement);
        self.composer.set_block(block.clone());
        let draft_height = if content.is_empty() {
            1
        } else {
            // Paragraph drops some wrapped spaces; measure the editor's own screen map.
            (&self.composer).render(measurement, &mut Buffer::empty(measurement));
            let last_line = self.composer.lines().len() - 1;
            let end = DataCursor(last_line, self.composer.lines()[last_line].chars().count());
            let end_row = (0..usize::from(content.height))
                .find(|row| self.composer.screen_to_data(*row, usize::MAX) == end);
            if end_row.is_some()
                && self.composer.scroll_offset().0 > 0
                && self.composer.selection_range().is_none()
            {
                // The editor retains its scroll offset after a long path shrinks to a marker.
                let DataCursor(row, column) = self.composer.cursor();
                self.composer.scroll((-i16::MAX, 0));
                let DataCursor(row_after, column_after) = self.composer.cursor();
                debug_assert_eq!(row, row_after, "The complete draft fits the viewport");
                // Scrolling clamps a cursor at the right edge of a wrapped line back one cell.
                for _ in column_after..column {
                    self.composer.move_cursor(CursorMove::Forward);
                }
            }
            end_row.map_or(usize::from(content.height), |row| row + 1)
        };
        let composer_height = u16::try_from(draft_height.saturating_add(2))
            .unwrap_or(u16::MAX)
            .saturating_add(padding_vertical * 2)
            .clamp(3 + padding_vertical * 2, 8)
            .min(composer_height_max);
        let bottom_notice = match (
            self.phase == Phase::Stopping,
            self.exit_deadline.is_some(),
            self.escape_deadline.is_some(),
        ) {
            (true, true, _) => "Stopping... · Ctrl+C to close",
            (true, false, _) => "Stopping...",
            (false, true, true) if self.escape_notice_latest => "Esc again to stop",
            (false, true, _) => "Ctrl+C to close",
            (false, false, true) => "Esc again to stop",
            _ if self.phase == Phase::Stopped => "Ctrl+C to close",
            _ => "",
        };
        let notice_inline = Line::from(bottom_notice).width() + 4 <= usize::from(area.width);
        let notice_height = if bottom_notice.is_empty() || notice_inline {
            0
        } else {
            Paragraph::new(bottom_notice)
                .wrap(Wrap { trim: false })
                .line_count(area.width.max(1))
                .min(usize::from(area.height.saturating_sub(composer_height))) as u16
        };
        let retry_notice = match self.progress {
            Progress::Paused { .. } if self.phase == Phase::Running => "Provider paused",
            Progress::Retrying { status, .. } if self.phase == Phase::Running => {
                if matches!(status, 429 | 503) {
                    "Provider Saturation"
                } else {
                    "Provider Error"
                }
            }
            _ => "",
        };
        let paused_notice = match &self.progress {
            Progress::Paused { reason, .. } if area.height >= 10 => {
                Some(format!("Provider paused\n{reason}"))
            }
            Progress::Paused { .. } => Some(String::from("Provider paused")),
            _ => None,
        };
        let status_notice = if let Some(notice) = &paused_notice {
            notice.as_str()
        } else if retry_notice.is_empty() {
            self.input_notice.as_str()
        } else {
            retry_notice
        };
        let status_color = if status_notice.is_empty() {
            palette.muted
        } else {
            palette.error
        };
        let status_margin_horizontal = if status_notice.is_empty() { 0 } else { 2 };
        let status_content_height = if status_notice.is_empty() {
            1
        } else {
            Paragraph::new(status_notice)
                .wrap(Wrap { trim: false })
                .line_count(
                    area.width
                        .saturating_sub(status_margin_horizontal * 2)
                        .max(1),
                )
                .saturating_add(if paused_notice.is_some() { 3 } else { 0 })
                .min(usize::from(
                    area.height.saturating_sub(composer_height + notice_height),
                )) as u16
        };
        let status_margin_vertical = if status_notice.is_empty() || area.height < 10 {
            0
        } else {
            (area
                .height
                .saturating_sub(composer_height + notice_height + status_content_height)
                / 2)
            .min(1)
        };
        let status_height = status_content_height + status_margin_vertical * 2;
        let output_height = area
            .height
            .saturating_sub(composer_height + status_height + notice_height);
        let output = Rect::new(area.x, area.y, area.width, output_height);
        let composer = Rect::new(
            area.x,
            area.bottom()
                .saturating_sub(composer_height + notice_height),
            area.width,
            composer_height,
        );
        self.composer_area = composer;
        self.popup = Rect::default();
        let mut selection = SelectionMap::default();
        self.render_history(frame, output, &mut selection);
        if self.history.is_empty() && self.preview.is_empty() && self.phase == Phase::Idle {
            crate::brand::welcome(frame, output, self.theme);
        }
        let notice = if !retry_notice.is_empty() {
            retry_notice
        } else if !self.input_notice.is_empty() {
            self.input_notice.as_str()
        } else if self.activity_focus.is_some() {
            "↑/↓ select · → expand · ← collapse · Esc prompt"
        } else {
            ""
        };
        let status_area = Rect::new(
            area.x,
            output.bottom(),
            area.width,
            status_height.min(area.height),
        )
        .inner(Margin::new(
            status_margin_horizontal,
            status_margin_vertical,
        ));
        frame.render_widget(
            Paragraph::new(paused_notice.as_deref().unwrap_or(notice))
                .style(Style::new().fg(status_color))
                .wrap(Wrap { trim: false }),
            status_area,
        );
        self.recovery_rows = [Rect::default(); 2];
        if let Progress::Paused { selected, .. } = self.progress {
            for (index, label, shortcut) in [(0, "Retry", "r"), (1, "Cancel", "c")] {
                if status_area.height < 3 {
                    continue;
                }
                let row = Rect::new(
                    status_area.x,
                    status_area.bottom() - 2 + index as u16,
                    status_area.width.min(28),
                    1,
                );
                self.recovery_rows[index] = row;
                let selected = selected == index;
                let marker = if selected { "▎" } else { " " };
                let spacing = usize::from(row.width).saturating_sub(label.len() + 3);
                let text = format!("{marker} {label}{:spacing$}{shortcut}", "");
                let style = if selected {
                    self.theme.selection_style()
                } else {
                    Style::new().fg(palette.muted)
                };
                frame.render_widget(Paragraph::new(text).style(style), row);
            }
        }
        let mut title = match self.phase {
            Phase::Idle
                if self.alias.len()
                    + self
                        .thinking
                        .as_ref()
                        .map_or(0, |thinking| thinking.len() + 1)
                    + 11
                    <= usize::from(area.width) =>
            {
                Line::from(" Ready ")
            }
            Phase::Idle => Line::default(),
            Phase::Running if paused_notice.is_some() => Line::from(" Paused "),
            Phase::Running => {
                let label = match self.progress {
                    Progress::Retrying { retry_at, .. } => {
                        let seconds = retry_seconds(retry_at, Instant::now());
                        format!("Retrying in {seconds}s · {}/3", self.provider_attempt)
                    }
                    Progress::Paused { .. } => String::from("Paused"),
                    Progress::Thinking | Progress::RunningTools | Progress::Streaming => {
                        self.activity_label().to_owned()
                    }
                };
                let loader = self.loader().filter(|loader| {
                    loader.composer().len() + Line::from(label.as_str()).width() + 5
                        <= usize::from(area.width)
                });
                if let Some(loader) = loader {
                    Line::from(vec![
                        Span::raw(" "),
                        Span::styled(loader.composer(), Style::new().fg(palette.accent)),
                        Span::raw(format!("{label} ")),
                    ])
                } else {
                    Line::from(format!(" {label} "))
                }
            }
            Phase::Stopping => Line::default(),
            Phase::Stopped => Line::from(" Stopped "),
        };
        if let Some(elapsed) = self.turn_elapsed
            && title.width() > 0
            && (self.phase == Phase::Idle
                || (self.phase == Phase::Running
                    && matches!(
                        self.progress,
                        Progress::Thinking | Progress::Streaming | Progress::RunningTools
                    )))
        {
            let timing = format!("· {}s ", elapsed.as_secs());
            if (self.phase != Phase::Running || elapsed.as_secs() > 0)
                && title.width() + timing.len() + self.alias.len() + 6 <= usize::from(area.width)
            {
                if self.phase == Phase::Running
                    && matches!(self.progress, Progress::Streaming | Progress::Thinking)
                    && let Some(tokens_per_second) = self.stream_tokens_per_second
                {
                    let speed = format!("· ~{tokens_per_second:.0}tps ");
                    if title.width() + speed.len() + timing.len() + self.alias.len() + 6
                        <= usize::from(area.width)
                    {
                        title.spans.push(Span::raw(speed));
                    }
                }
                title.spans.push(Span::raw(timing));
            }
        }
        let title_width = title.width();
        block = block.title_top(title.style(Style::new().fg(status_color)));
        if !self.alias.is_empty() {
            let mut label = self.alias.clone();
            if let Some(thinking) = &self.thinking {
                let suffixed = format!("{label} {thinking}");
                if title_width + Line::from(suffixed.as_str()).width() + 4
                    <= usize::from(area.width)
                {
                    label = suffixed;
                }
            }
            for suffix in [
                self.cache_hit_rate.map(|rate| format!(" · C{rate:.0}%")),
                self.cost_usd.map(|cost| format!(" · ~${cost:.2}")),
            ]
            .into_iter()
            .flatten()
            {
                let suffixed = format!("{label}{suffix}");
                if title_width + Line::from(suffixed.as_str()).width() + 4
                    <= usize::from(area.width)
                {
                    label = suffixed;
                }
            }
            let alias =
                Line::styled(format!(" {label} "), Style::new().fg(palette.accent)).right_aligned();
            if title_width + alias.width() + 2 <= usize::from(area.width) {
                block = block.title_top(alias);
            }
        }
        let hint = if notice_inline && !bottom_notice.is_empty() {
            Line::from(format!(" {bottom_notice} "))
        } else {
            Line::default()
        };
        let workspace_width = usize::from(area.width).saturating_sub(hint.width() + 4);
        let directory = self.workspace_title(workspace_width);
        if directory.width() > 0 {
            block = block.title_bottom(
                directory
                    .style(Style::new().fg(palette.muted))
                    .right_aligned(),
            );
        }
        block = block.title_bottom(hint.style(Style::new().fg(palette.muted)));
        let composer_text = block.inner(composer);
        self.composer_text = composer_text;
        self.composer.set_block(block);
        self.composer.set_cursor_style(
            if self.activity_focus.is_some() || paused_notice.is_some() {
                Style::default()
            } else {
                Style::new().add_modifier(Modifier::REVERSED)
            },
        );
        frame.render_widget(&self.composer, composer);
        selection.composer(&self.composer, composer_text, frame.buffer_mut());
        frame.render_widget(
            Paragraph::new(bottom_notice)
                .style(Style::new().fg(palette.muted))
                .wrap(Wrap { trim: false }),
            Rect::new(
                area.x,
                area.bottom().saturating_sub(notice_height),
                area.width,
                notice_height,
            ),
        );
        if self.phase != Phase::Stopped
            && paused_notice.is_none()
            && self.activity_focus.is_none()
            && let Some(files) = &mut self.files
        {
            let height = files.height().min(composer.y.saturating_sub(area.y));
            self.popup = Rect::new(area.x, composer.y - height, area.width, height);
            selection.exclude(Rect::new(area.x, composer.y - height, area.width, height));
            files.render(
                frame,
                Rect::new(
                    area.x + 2.min(area.width),
                    composer.y - height,
                    area.width.saturating_sub(4),
                    height,
                ),
                self.theme,
            );
        }
        self.selection
            .render(selection, frame.buffer_mut(), self.theme.selection_style());
        self.render_slash(frame, composer);
        if self.copy_deadline.is_some() && !area.is_empty() {
            let width = area.width.min(9);
            frame.render_widget(
                Paragraph::new(" Copied! ").style(Style::new().fg(palette.muted)),
                Rect::new(area.right() - width, area.y, width, 1),
            );
        }
    }

    /// Local slash commands run while idle or during a turn, even with steering
    /// disabled, but not while stopping or when the provider is paused.
    fn commands_available(&self) -> bool {
        matches!(self.phase, Phase::Idle | Phase::Running)
            && !matches!(self.progress, Progress::Paused { .. })
    }

    fn workspace_title(&self, width: usize) -> Line<'static> {
        if width < 3 {
            return Line::default();
        }
        let palette = self.theme.palette();
        let mut changes = Line::from(match self.changes {
            Some((additions, deletions)) if additions > 0 || deletions > 0 => vec![
                Span::styled(format!(" +{additions}"), Style::new().fg(palette.success)),
                Span::styled(format!(" -{deletions}"), Style::new().fg(palette.error)),
            ],
            _ => Vec::new(),
        });
        let mut branch = self
            .branch
            .as_ref()
            .map(|branch| fit_label(branch, width.saturating_sub(8).min(width / 2) as u16))
            .filter(|branch| !branch.is_empty());
        let branch_width = branch
            .as_ref()
            .map_or(0, |branch| Line::from(branch.as_str()).width() + 3);
        if branch_width + changes.width() + 6 > width {
            changes.spans.clear();
        }
        if branch_width + 5 > width {
            branch = None;
        }
        let branch_width = branch
            .as_ref()
            .map_or(0, |branch| Line::from(branch.as_str()).width() + 3);
        let directory_width = width.saturating_sub(branch_width + changes.width() + 2);
        let mut spans = vec![Span::raw(format!(
            " {}",
            directory_title(&self.directory, directory_width)
        ))];
        if let Some(branch) = branch {
            spans.push(Span::raw(format!(" · {branch}")));
        }
        spans.extend(changes.spans);
        spans.push(Span::raw(" "));
        Line::from(spans)
    }

    fn input_event(&mut self, event: Event, now: Instant) -> Action {
        if let Progress::Paused { selected, .. } = &mut self.progress {
            match &event {
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(index) = self
                        .recovery_rows
                        .iter()
                        .position(|area| area.contains((mouse.column, mouse.row).into()))
                    {
                        return if index == 0 {
                            Action::Retry
                        } else {
                            Action::Cancel
                        };
                    }
                }
                Event::Key(key) if key.modifiers.is_empty() => {
                    if key.kind != KeyEventKind::Press {
                        return Action::None;
                    }
                    return match key.code {
                        KeyCode::Char('r') => Action::Retry,
                        KeyCode::Char('c') | KeyCode::Esc => Action::Cancel,
                        KeyCode::Enter => {
                            if *selected == 0 {
                                Action::Retry
                            } else {
                                Action::Cancel
                            }
                        }
                        KeyCode::Up => {
                            *selected = 0;
                            Action::Redraw
                        }
                        KeyCode::Down => {
                            *selected = 1;
                            Action::Redraw
                        }
                        KeyCode::Tab => {
                            *selected = 1 - *selected;
                            Action::Redraw
                        }
                        KeyCode::PageUp | KeyCode::PageDown => {
                            self.scroll = if key.code == KeyCode::PageUp {
                                self.scroll.saturating_add(5)
                            } else {
                                self.scroll.saturating_sub(5)
                            };
                            Action::Redraw
                        }
                        _ => Action::None,
                    };
                }
                Event::Key(key)
                    if key.code != KeyCode::Char('c') || key.modifiers != KeyModifiers::CONTROL =>
                {
                    return Action::None;
                }
                _ => {}
            }
        }
        if let Event::Paste(text) = &event {
            return self.paste(text);
        }
        if let Event::Mouse(mouse) = event {
            if matches!(
                mouse.kind,
                MouseEventKind::Down(MouseButton::Left)
                    | MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
            ) && self.activity_focus.take().is_some()
            {
                self.reset_gestures();
            }
            if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                && !self.popup.contains((mouse.column, mouse.row).into())
            {
                if let Some((key, _)) = self
                    .headers
                    .iter()
                    .find(|(_, area)| area.contains((mouse.column, mouse.row).into()))
                {
                    let key = *key;
                    self.toggle_activity(key, None);
                    return Action::Redraw;
                }
                if self
                    .composer_area
                    .contains((mouse.column, mouse.row).into())
                {
                    self.reset_gestures();
                    let (scroll_row, scroll_column) = self.composer.scroll_offset();
                    let row = usize::from(mouse.row.saturating_sub(self.composer_text.y))
                        .min(usize::from(self.composer_text.height.saturating_sub(1)))
                        + usize::from(scroll_row);
                    let column = usize::from(mouse.column.saturating_sub(self.composer_text.x))
                        .min(usize::from(self.composer_text.width.saturating_sub(1)))
                        + usize::from(scroll_column);
                    let DataCursor(row, column) = self.composer.screen_to_data(row, column);
                    self.composer.move_cursor(CursorMove::Jump(
                        row.try_into().unwrap_or(u16::MAX),
                        column.try_into().unwrap_or(u16::MAX),
                    ));
                }
            }
            self.scroll = match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll.saturating_add(3),
                MouseEventKind::ScrollDown => self.scroll.saturating_sub(3),
                _ => return self.selection.mouse(mouse, now),
            };
            self.reveal_focus = false;
            self.selection.invalidate();
            self.headers.clear();
            return Action::Redraw;
        }
        let Event::Key(key) = event else {
            return if matches!(event, Event::Resize(..) | Event::FocusLost) {
                self.selection.invalidate();
                self.headers.clear();
                Action::Redraw
            } else {
                Action::None
            };
        };
        if key.kind == KeyEventKind::Release
            || (key.code == KeyCode::Enter && key.kind == KeyEventKind::Repeat)
            || (key.kind == KeyEventKind::Repeat
                && (key.code == KeyCode::Esc
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL))))
        {
            return Action::None;
        }
        self.selection.invalidate();
        let up_armed = std::mem::take(&mut self.up_armed);
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.phase == Phase::Stopped || self.exit_deadline.is_some() {
                self.reset_gestures();
                self.stopping();
                return Action::Exit;
            }
            self.exit_deadline = Some(now + Duration::from_secs(3));
            self.escape_notice_latest = false;
            if !self.composer.is_empty() {
                self.composer.select_all();
                self.composer.cut();
                self.saved_draft = None;
                return Action::Redraw;
            }
            if self.phase == Phase::Running {
                self.stopping();
                return Action::Cancel;
            }
            return Action::Redraw;
        }
        if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            self.scroll = if key.code == KeyCode::PageUp {
                self.scroll.saturating_add(5)
            } else {
                self.scroll.saturating_sub(5)
            };
            self.reveal_focus = false;
            return Action::Redraw;
        }
        let alias_previous = key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        if (key.code == KeyCode::Char('t') && key.modifiers == KeyModifiers::CONTROL)
            || (matches!(key.code, KeyCode::Char('t' | 'T')) && alias_previous)
        {
            return if self.phase == Phase::Idle && key.kind == KeyEventKind::Press {
                if alias_previous {
                    Action::CycleAliasPrevious
                } else {
                    Action::CycleAlias
                }
            } else {
                Action::None
            };
        }
        let list_key = match (key.code, key.modifiers) {
            (KeyCode::Char('n'), KeyModifiers::CONTROL) => Some(KeyCode::Down),
            (KeyCode::Char('p'), KeyModifiers::CONTROL) => Some(KeyCode::Up),
            _ => None,
        }
        .map_or(key, |code| KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            ..key
        });
        let matches = self.slash_matches();
        if !matches.is_empty() && self.activity_focus.is_none() {
            let handled = match list_key.code {
                KeyCode::Up => {
                    self.slash_selected = self.slash_selected.saturating_sub(1);
                    true
                }
                KeyCode::Down => {
                    self.slash_selected = (self.slash_selected + 1).min(matches.len() - 1);
                    true
                }
                KeyCode::Enter | KeyCode::Tab if list_key.modifiers.is_empty() => {
                    let selected = self.slash_selected.min(matches.len() - 1);
                    let mention = self.skill_mention();
                    let removed = mention.as_ref().map_or_else(
                        || self.composer.lines().iter().map(String::len).sum(),
                        std::iter::ExactSizeIterator::len,
                    );
                    if self.draft_bytes() - removed + matches[selected].len() > self.input_bytes {
                        self.input_notice("Completion exceeds the configured input byte budget.");
                        return Action::Redraw;
                    }
                    if let Some(range) = mention {
                        let DataCursor(row, _) = self.composer.cursor();
                        let line = &self.composer.lines()[row];
                        let start = line[..range.start].chars().count();
                        let length = line[range].chars().count();
                        self.composer
                            .move_cursor(CursorMove::Jump(row as u16, start as u16));
                        self.composer.delete_str(length);
                    } else {
                        self.composer.select_all();
                    }
                    if let Some((_, theme)) = THEME_COMMANDS
                        .iter()
                        .find(|(command, _)| *command == matches[selected])
                    {
                        self.composer.cut();
                        self.saved_draft = None;
                        self.set_theme(*theme);
                    } else {
                        self.composer.insert_str(&matches[selected]);
                        self.saved_draft = None;
                    }
                    self.slash_selected = 0;
                    true
                }
                KeyCode::Esc => {
                    self.slash_dismissed = self.composer.lines().join("\n");
                    true
                }
                _ => false,
            };
            if handled {
                return Action::Redraw;
            }
        }
        let text_budget = self.input_bytes.saturating_sub(self.image_bytes());
        if self.phase != Phase::Stopped
            && self.activity_focus.is_none()
            && let Some(files) = &mut self.files
        {
            files.update(&self.composer);
            if files.input(list_key, &mut self.composer, text_budget) {
                self.escape_deadline = None;
                self.exit_deadline = None;
                if matches!(key.code, KeyCode::Enter | KeyCode::Tab) {
                    self.saved_draft = None;
                }
                return Action::Redraw;
            }
        }
        if let Some(action) = self.activity_input(list_key) {
            return action;
        }
        if key.code == KeyCode::Char('p') && key.modifiers == KeyModifiers::CONTROL {
            return if key.kind == KeyEventKind::Press && self.phase == Phase::Idle {
                Action::Command("/models".into())
            } else {
                Action::None
            };
        }
        if self.phase == Phase::Stopped {
            return Action::None;
        }
        if key.code == KeyCode::Char('v')
            && matches!(key.modifiers, KeyModifiers::CONTROL | KeyModifiers::SUPER)
        {
            return if key.kind == KeyEventKind::Press {
                Action::Paste
            } else {
                Action::None
            };
        }
        if key.code == KeyCode::Esc && key.modifiers.is_empty() && self.phase == Phase::Running {
            if self.escape_deadline.is_some() {
                self.escape_deadline = None;
                self.stopping();
                return Action::Cancel;
            }
            self.escape_deadline = Some(now + Duration::from_secs(1));
            self.escape_notice_latest = true;
            return Action::Redraw;
        }
        if key.modifiers.is_empty() && key.kind == KeyEventKind::Press {
            match key.code {
                KeyCode::Up if self.saved_draft.is_some() => return Action::None,
                KeyCode::Up
                    if !self.previous_prompt.is_empty()
                        && (self.composer.is_empty()
                            || (up_armed && self.composer.screen_cursor().row == 0)) =>
                {
                    self.saved_draft = Some(Draft {
                        composer: self.composer.clone(),
                        images: self.images.clone(),
                    });
                    self.composer.select_all();
                    self.composer.insert_str(&self.previous_prompt);
                    self.images.clone_from(&self.previous_images);
                    return Action::Redraw;
                }
                KeyCode::Up => self.up_armed = true,
                KeyCode::Down => {
                    if let Some(draft) = self.saved_draft.take() {
                        self.composer = draft.composer;
                        self.images = draft.images;
                        return Action::Redraw;
                    }
                }
                _ => {}
            }
        }
        let clear_draft = match key.code {
            KeyCode::Backspace | KeyCode::Char('\u{8}') => key.modifiers == KeyModifiers::SUPER,
            KeyCode::Char('u') => key.modifiers == KeyModifiers::CONTROL,
            _ => false,
        };
        if clear_draft {
            if self.composer.is_empty() {
                return Action::None;
            }
            self.composer = new_composer();
            self.saved_draft = None;
            return Action::Redraw;
        }
        if key.modifiers.is_empty()
            && matches!(key.code, KeyCode::Backspace | KeyCode::Delete)
            && self.composer.selection_range().is_none()
            && !self.images.is_empty()
        {
            let DataCursor(row, column) = self.composer.cursor();
            let line = &self.composer.lines()[row];
            let offset = line
                .char_indices()
                .nth(column)
                .map_or(line.len(), |(offset, _)| offset);
            let marker_length = self
                .images
                .iter()
                .find(|attachment| {
                    if key.code == KeyCode::Backspace {
                        line[..offset].ends_with(&attachment.marker)
                    } else {
                        line[offset..].starts_with(&attachment.marker)
                    }
                })
                .map(|attachment| attachment.marker.len());
            if let Some(length) = marker_length {
                for _ in 0..length {
                    if key.code == KeyCode::Backspace {
                        self.composer.delete_char();
                    } else {
                        self.composer.delete_next_char();
                    }
                }
                self.saved_draft = None;
                return Action::Redraw;
            }
        }
        // Legacy ESC BS is decoded as Ctrl+Alt+H; CSI-u BS retains its codepoint.
        let delete_word = match key.code {
            KeyCode::Backspace | KeyCode::Char('\u{8}') => key.modifiers == KeyModifiers::ALT,
            KeyCode::Char('h') => key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT),
            KeyCode::Char('w') => key.modifiers == KeyModifiers::CONTROL,
            _ => false,
        };
        if delete_word {
            return if self.composer.delete_word() {
                self.saved_draft = None;
                Action::Redraw
            } else {
                Action::None
            };
        }
        let movement = match (key.code, key.modifiers) {
            (KeyCode::Left, KeyModifiers::SUPER) | (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                self.composer.move_cursor(CursorMove::Top);
                Some(CursorMove::Head)
            }
            (KeyCode::Right, KeyModifiers::SUPER) | (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                self.composer.move_cursor(CursorMove::Bottom);
                Some(CursorMove::End)
            }
            (KeyCode::Left, KeyModifiers::ALT) | (KeyCode::Char('b'), KeyModifiers::ALT) => {
                Some(CursorMove::WordBack)
            }
            (KeyCode::Right, KeyModifiers::ALT) | (KeyCode::Char('f'), KeyModifiers::ALT) => {
                Some(CursorMove::WordForward)
            }
            _ => None,
        };
        if let Some(movement) = movement {
            self.composer.move_cursor(movement);
            return Action::Redraw;
        }
        if key.code == KeyCode::Enter && key.modifiers.is_empty() {
            if self.composer.lines().len() == 1
                && matches!(self.composer.lines()[0].as_str(), "exit" | ":q" | ":x")
            {
                self.stopping();
                return Action::Exit;
            }
            let prompt = self.composer.lines().join("\n");
            let session_command = matches!(
                prompt.split_whitespace().next(),
                Some("/new" | "/resume" | "/sessions" | "/rewind")
            );
            if session_command && self.phase != Phase::Idle {
                self.input_notice(
                    "Cannot switch sessions while a turn is active. The draft is unchanged.",
                );
                return Action::Redraw;
            }
            let local_command = self.images.is_empty()
                && prompt
                    .split_whitespace()
                    .next()
                    .is_some_and(|name| SLASH_COMMANDS.contains(&name) || name == "/model");
            if !self.can_submit() && !(local_command && self.commands_available()) {
                return Action::None;
            }
            let steering = self.phase == Phase::Running;
            if prompt.trim().is_empty() {
                return Action::None;
            }
            let mut skill_instructions = String::new();
            let mut selected_skills = BTreeSet::new();
            if !prompt.starts_with('/') {
                for name in prompt
                    .split_whitespace()
                    .filter_map(|word| word.strip_prefix('$'))
                {
                    let mut matches = self.skills.iter().filter(|skill| name == skill.name);
                    if let Some(skill) = matches.next() {
                        if !selected_skills.insert(name) {
                            continue;
                        }
                        if matches.next().is_some() {
                            self.input_notice(
                                "Multiple skills have this name. Use a unique skill name.",
                            );
                            return Action::Redraw;
                        }
                        let id = match serde_json::to_string(&skill.id) {
                            Ok(id) => id,
                            Err(error) => {
                                self.input_notice(&error.to_string());
                                return Action::Redraw;
                            }
                        };
                        skill_instructions.push_str(&format!(
                            "Use the ${name} skill. Before doing any work, load it with await cocoa.load_skill({{id:{id}}}) inside exec, then finish the cell and follow the delivered instructions.\n\n"
                        ));
                        if skill_instructions.len() + self.draft_bytes() > self.input_bytes {
                            self.input_notice(
                                "Skill request exceeds the configured input byte budget.",
                            );
                            return Action::Redraw;
                        }
                    }
                }
            }
            if prompt.starts_with('/') && (self.images.is_empty() || session_command) {
                self.composer.select_all();
                self.composer.cut();
                self.saved_draft = None;
                if prompt.split_whitespace().next() == Some("/theme") {
                    if let Some((_, theme)) = THEME_COMMANDS
                        .iter()
                        .find(|(command, _)| *command == prompt.trim())
                    {
                        self.set_theme(*theme);
                    } else {
                        self.input_notice("Use /theme to select dark-chocolate, cacao-grove, milk-chocolate or terminal-native.");
                    }
                    return Action::Redraw;
                }
                if prompt.trim() == "/copy" {
                    if let Some(message) = self
                        .history
                        .iter()
                        .rev()
                        .find(|message| matches!(message.kind, MessageKind::Assistant))
                    {
                        return Action::Copy(message.text.clone());
                    }
                    self.input_notice("No agent response to copy yet.");
                    return Action::Redraw;
                }
                return Action::Command(prompt);
            }
            if !steering {
                self.phase = Phase::Running;
                self.progress = Progress::Thinking;
                self.turn_elapsed = Some(Duration::ZERO);
                self.stream_characters = None;
                self.stream_elapsed = Duration::ZERO;
                self.stream_tokens_per_second = None;
                self.preview.clear();
                self.invocation = None;
                self.hidden_wait_operations
                    .retain(|(agent, _, _)| agent.is_some());
                self.activity_elapsed = Duration::ZERO;
            }
            self.scroll = 0;
            self.append_history(MessageKind::User, &prompt);
            self.previous_prompt.clone_from(&prompt);
            self.images
                .sort_by_key(|attachment| prompt.find(&attachment.marker));
            self.previous_images.clone_from(&self.images);
            let prompt = if skill_instructions.is_empty() {
                prompt
            } else {
                skill_instructions + &prompt
            };
            let input = if self.images.is_empty() {
                UserInput::Text(prompt)
            } else {
                UserInput::WithImages {
                    text: prompt,
                    images: std::mem::take(&mut self.images)
                        .into_iter()
                        .map(|attachment| attachment.image)
                        .collect(),
                }
            };
            self.saved_draft = None;
            self.composer = new_composer();
            return if steering {
                Action::Steer(input)
            } else {
                Action::Submit(input)
            };
        }
        let newline = key.modifiers == KeyModifiers::CONTROL
            && matches!(key.code, KeyCode::Enter | KeyCode::Char('j'));
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
            && !newline
        {
            return Action::None;
        }
        let addition = match key.code {
            _ if newline => 1,
            KeyCode::Char(character) if !character.is_control() => character.len_utf8(),
            KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End => 0,
            _ => return Action::None,
        };
        let bytes = self.draft_bytes();
        if bytes + addition > self.input_bytes {
            return Action::None;
        }
        if newline {
            self.composer.insert_newline();
            self.saved_draft = None;
        } else if self.composer.input(key) {
            self.saved_draft = None;
        }
        Action::Redraw
    }

    /// Insert pasted text where typing is accepted. A paste never submits.
    fn paste(&mut self, text: &str) -> Action {
        if self.phase == Phase::Stopped
            || self.activity_focus.is_some()
            || matches!(self.progress, Progress::Paused { .. })
        {
            return Action::None;
        }
        match self.paste_text(text) {
            Ok(()) => Action::Redraw,
            Err(error) => {
                self.input_notice(&error.to_string());
                Action::Redraw
            }
        }
    }

    fn insert_image(&mut self, image: Image, replaced: &str) -> Result<(), PasteError> {
        if self.images.len() >= 4 {
            return Err(PasteError::ImageLimit);
        }
        let sequence = self
            .image_sequence
            .checked_add(1)
            .ok_or(PasteError::ImageLimit)?;
        let marker = format!("[Image {sequence}]");
        let trailing = &replaced[replaced.trim_end().len()..];
        if self
            .draft_bytes()
            .saturating_sub(replaced.len())
            .saturating_add(marker.len() + trailing.len())
            .saturating_add(image.data_url().len())
            > self.input_bytes
        {
            return Err(PasteError::InputLimit);
        }
        for _ in replaced.chars() {
            self.composer.delete_char();
        }
        self.image_sequence = sequence;
        self.composer.insert_str(&marker);
        self.composer.insert_str(trailing);
        self.images.push(Attachment { marker, image });
        self.saved_draft = None;
        self.selection.invalidate();
        self.reset_gestures();
        Ok(())
    }

    fn prune_images(&mut self) {
        self.images.retain(|attachment| {
            self.composer
                .lines()
                .iter()
                .any(|line| line.contains(&attachment.marker))
        });
    }

    fn draft_bytes(&self) -> usize {
        let text_bytes = self.composer.lines().iter().map(String::len).sum::<usize>()
            + self.composer.lines().len()
            - 1;
        text_bytes.saturating_add(self.image_bytes())
    }

    fn image_bytes(&self) -> usize {
        self.images.iter().fold(0usize, |bytes, attachment| {
            bytes.saturating_add(attachment.image.data_url().len())
        })
    }

    fn skill_mention(&self) -> Option<std::ops::Range<usize>> {
        let DataCursor(row, column) = self.composer.cursor();
        let line = &self.composer.lines()[row];
        let offset = crate::selection::byte_offset(line, column);
        let start = line[..offset]
            .trim_end_matches(|character: char| !character.is_whitespace())
            .len();
        if !line[start..offset].starts_with('$') {
            return None;
        }
        let end = line[offset..]
            .find(char::is_whitespace)
            .map_or(line.len(), |index| offset + index);
        Some(start..end)
    }

    fn slash_matches(&self) -> Vec<String> {
        if self.composer.lines().join("\n") == self.slash_dismissed {
            return Vec::new();
        }
        if self.can_submit()
            && let Some(range) = self.skill_mention()
        {
            let DataCursor(row, column) = self.composer.cursor();
            let line = &self.composer.lines()[row];
            let offset = crate::selection::byte_offset(line, column);
            let query = &line[range.start + 1..offset];
            if self
                .skills
                .iter()
                .any(|skill| skill.name == line[range.start + 1..range.end])
            {
                return Vec::new();
            }
            let mut matches = Vec::new();
            for skill in &self.skills {
                let command = format!("${}", skill.name);
                if skill.name.starts_with(query) && !matches.contains(&command) {
                    matches.push(command);
                }
            }
            return matches;
        }
        if !self.commands_available() || self.composer.lines().len() != 1 {
            return Vec::new();
        }
        let draft = &self.composer.lines()[0];
        if !draft.starts_with('/') {
            return Vec::new();
        }
        if draft == "/theme" || draft.starts_with("/theme ") {
            return THEME_COMMANDS
                .iter()
                .filter(|(command, _)| command.starts_with(draft) && command != draft)
                .map(|(command, _)| (*command).to_owned())
                .collect();
        }
        if let Some((command, query)) = draft.split_once(' ') {
            if query.contains(' ') {
                return Vec::new();
            }
            if matches!(command, "/models" | "/model") {
                let candidates = &self.aliases;
                if candidates.iter().any(|name| name == query) {
                    return Vec::new();
                }
                return candidates
                    .iter()
                    .filter(|name| name.starts_with(query))
                    .map(|name| format!("{command} {name}"))
                    .collect();
            }
            if !matches!(command, "/login" | "/logout") {
                return Vec::new();
            }
            return cocoa_llm::ProviderKind::ALL
                .into_iter()
                .filter(|provider| {
                    command != "/login" || *provider != cocoa_llm::ProviderKind::Ollama
                })
                .map(|provider| provider.to_string())
                .filter(|provider| provider.starts_with(query) && provider != query)
                .map(|provider| format!("{command} {provider}"))
                .collect();
        }
        let commands: Vec<String> = SLASH_COMMANDS
            .iter()
            .map(|command| (*command).to_owned())
            .collect();
        if commands.contains(draft) || draft == "/model" {
            return Vec::new();
        }
        commands
            .into_iter()
            .filter(|command| command.starts_with(draft))
            .collect()
    }

    fn render_slash(&mut self, frame: &mut Frame<'_>, composer: Rect) {
        let palette = self.theme.palette();
        let matches = self.slash_matches();
        if matches.is_empty() || self.activity_focus.is_some() {
            return;
        }
        let height = (matches.len() as u16).min(composer.y.saturating_sub(frame.area().y));
        let area = Rect::new(
            composer.x + 2.min(composer.width),
            composer.y - height,
            composer.width.saturating_sub(4),
            height,
        );
        frame.render_widget(Clear, area);
        frame.render_widget(Block::new().style(self.theme.style()), area);
        let selected = self.slash_selected.min(matches.len() - 1);
        let first = selected.saturating_sub(usize::from(height.saturating_sub(1)));
        let command_width = matches
            .iter()
            .map(|command| Line::from(command.as_str()).width())
            .max()
            .unwrap_or_default();
        let lines: Vec<_> = matches
            .iter()
            .enumerate()
            .skip(first)
            .take(usize::from(height))
            .map(|(index, command)| {
                let description = if area.width >= 40 {
                    match command.as_str() {
                        "/login" => "Connect a provider",
                        "/logout" => "Disconnect a provider",
                        "/copy" => "Copy the last agent response",
                        "/usage" => "Manage ChatGPT usage",
                        "/model" => "Select a model for this session",
                        "/models" => "Select an alias or provider model",
                        "/aliases" => "Edit alias model choices and priorities",
                        "/theme" => "Select a theme",
                        "/update" => "Update Cocoa or pin a release",
                        "/mcp" => "Manage MCP servers",
                        "/new" => "Start a new session",
                        "/resume" => "Resume a saved session",
                        "/rewind" => "Fork before an earlier prompt",
                        "/sessions" => "Search saved conversations",
                        _ => self
                            .skills
                            .iter()
                            .find(|skill| command.strip_prefix('$') == Some(skill.name.as_str()))
                            .map(|skill| skill.description.as_str())
                            .unwrap_or_default(),
                    }
                } else {
                    ""
                };
                Line::from(vec![
                    Span::styled(
                        format!(
                            "{} {command}{}",
                            if index == selected { "›" } else { " " },
                            " ".repeat(command_width - Line::from(command.as_str()).width() + 2),
                        ),
                        if index == selected {
                            Style::new().fg(palette.accent)
                        } else {
                            Style::default()
                        },
                    ),
                    Span::styled(description, Style::new().fg(palette.muted)),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
        self.popup = area;
    }

    fn activity_input(&mut self, key: KeyEvent) -> Option<Action> {
        if key.code == KeyCode::Tab && key.modifiers.is_empty() {
            self.reset_gestures();
            self.activity_focus = if self.activity_focus.is_some() {
                None
            } else {
                self.activity_keys().last().copied()
            };
            self.reveal_focus = true;
            return Some(Action::Redraw);
        }
        let focus = self.activity_focus?;
        if key.modifiers.is_empty() {
            match key.code {
                KeyCode::Esc => {
                    self.activity_focus = None;
                    self.reset_gestures();
                }
                KeyCode::Up | KeyCode::Down => {
                    let keys = self.activity_keys();
                    if let Some(index) = keys.iter().position(|key| *key == focus) {
                        let index = if key.code == KeyCode::Up {
                            index.saturating_sub(1)
                        } else {
                            (index + 1).min(keys.len() - 1)
                        };
                        self.activity_focus = Some(keys[index]);
                        self.reset_gestures();
                        self.reveal_focus = true;
                    }
                }
                KeyCode::Right => self.toggle_activity(focus, Some(true)),
                KeyCode::Left => self.toggle_activity(focus, Some(false)),
                KeyCode::Enter | KeyCode::Char(' ') if key.kind == KeyEventKind::Press => {
                    self.toggle_activity(focus, None)
                }
                _ => return Some(Action::None),
            }
            self.headers.clear();
            return Some(Action::Redraw);
        }
        Some(Action::None)
    }

    fn toggle_activity(&mut self, key: ActivityKey, expanded: Option<bool>) {
        self.reset_gestures();
        for message in &mut self.history {
            if let MessageKind::Activity(activity) = &mut message.kind
                && activity.key == key
                && activity.expandable()
            {
                activity.expanded = expanded.unwrap_or(!activity.expanded);
                break;
            }
        }
        if self
            .activity_focus
            .is_some_and(|focus| !self.activity_keys().contains(&focus))
        {
            self.activity_focus = Some(key);
        }
        self.reveal_focus = true;
        self.headers.clear();
        self.selection.invalidate();
    }

    /// Track display suppression by agent and invocation. Evict the smallest
    /// identity when the tracking budget is full.
    fn hide_wait(&mut self, key: ActivityKey, operation: OperationId) {
        if self.hidden_wait_operations.len() >= HIDDEN_WAIT_OPERATIONS
            && let Some(oldest) = self.hidden_wait_operations.iter().next().copied()
        {
            self.hidden_wait_operations.remove(&oldest);
        }
        self.hidden_wait_operations
            .insert((key.agent, key.invocation.0, operation));
    }

    fn hide_spawn_activity(&mut self, group: ActivityKey, name: &str) {
        let key = self.history.iter().find_map(|message| match &message.kind {
            MessageKind::Activity(activity)
                if activity.key.agent.is_none()
                    && activity.key.invocation == group.invocation
                    && activity.title == "cocoa.spawn"
                    && message.text == name =>
            {
                Some(activity.key)
            }
            _ => None,
        });
        if let Some(key) = key
            && let ActivityKind::Tool { operation } = key.kind
        {
            self.hide_wait(key, operation);
            self.history.retain(|message| !matches!(&message.kind, MessageKind::Activity(activity) if activity.key == key));
            if self.activity_focus == Some(key) {
                self.activity_focus = Some(group);
            }
        }
    }

    fn observe_capability(&mut self, key: ActivityKey, event: &EventKind) {
        let ActivityKind::Tool { operation } = key.kind else {
            return;
        };
        let hidden = match event {
            EventKind::CapabilityStarted { name, .. } => name == HIDDEN_WAIT_NAME,
            EventKind::CapabilityInput { title, text, .. } => {
                title == HIDDEN_WAIT_NAME
                    || title == HIDDEN_WAIT_TITLE
                    || (title == "cocoa.spawn"
                        && key.agent.is_none()
                        && self.agents.iter().any(|agent| {
                            agent.key.invocation == key.invocation && agent.name == *text
                        }))
            }
            _ => false,
        };
        if hidden {
            self.hide_wait(key, operation);
            self.history.retain(|message| !matches!(&message.kind, MessageKind::Activity(activity) if activity.key == key));
            if self.activity_focus == Some(key) {
                self.activity_focus = None;
            }
        }
        if self
            .hidden_wait_operations
            .contains(&(key.agent, key.invocation.0, operation))
        {
            return;
        }
        match event {
            EventKind::CapabilityStarted { name, .. } => {
                self.ensure_activity(key, tool_title(name));
            }
            EventKind::CapabilityInput {
                title,
                text,
                summary,
                ..
            } => {
                let index = self.ensure_activity(key, "Tool activity");
                if let MessageKind::Activity(activity) = &mut self.history[index].kind {
                    activity.title = display_label(title, 256);
                    activity.summary = *summary;
                    activity.output = None;
                }
                self.history[index].text.clear();
                self.activity_text(key, text);
            }
            EventKind::CapabilityOutput { output, .. } => {
                let index = self.ensure_activity(key, "Tool activity");
                if let MessageKind::Activity(activity) = &mut self.history[index].kind {
                    activity.output = Some(output.kind);
                    activity.summary = output.summary.or(activity.summary);
                }
                self.history[index].text.clear();
                self.activity_text(key, &output.text);
            }
            EventKind::CapabilityFinished { status, .. } => {
                self.ensure_activity(key, "Tool activity");
                self.finish_activity(key, *status);
            }
            _ => {}
        }
    }

    fn ensure_activity(&mut self, key: ActivityKey, title: &str) -> usize {
        let parent = if let Some(agent) = key.agent {
            self.agents[agent].key
        } else {
            let parent = ActivityKey {
                invocation: key.invocation,
                agent: None,
                kind: ActivityKind::Work,
            };
            self.insert_activity(parent, None, "Work");
            parent
        };
        self.insert_activity(key, Some(parent), title)
    }

    fn insert_activity(
        &mut self,
        key: ActivityKey,
        parent: Option<ActivityKey>,
        title: &str,
    ) -> usize {
        if let Some(index) = self.history.iter().position(|message| matches!(&message.kind, MessageKind::Activity(activity) if activity.key == key)) {
            return index;
        }
        let index = parent.and_then(|parent| {
            self.history.iter().rposition(|message| matches!(&message.kind, MessageKind::Activity(activity) if activity.key == parent || activity.parent == Some(parent) || (matches!(parent.kind, ActivityKind::Work) && activity.parent.is_some_and(|ancestor| matches!(ancestor.kind, ActivityKind::Agent) && ancestor.invocation == parent.invocation))))
        }).map_or(self.history.len(), |index| index + 1);
        self.history.insert(
            index,
            Message {
                kind: MessageKind::Activity(Activity {
                    key,
                    parent,
                    title: display_label(title, 256),
                    summary: None,
                    output: None,
                    provider_notice: None,
                    provider_attempt: 0,
                    state: ActivityState::Running,
                    expanded: matches!(key.kind, ActivityKind::Work),
                    collapse_on_finish: matches!(key.kind, ActivityKind::Work),
                }),
                text: String::new(),
                markdown: Markdown::default(),
                layout: WrapLayout::default(),
            },
        );
        self.selection.invalidate();
        index
    }

    fn activity_layout(&self) -> Vec<Option<u16>> {
        let mut layout = Vec::with_capacity(self.history.len());
        let mut groups = BTreeMap::new();
        for message in &self.history {
            let MessageKind::Activity(activity) = &message.kind else {
                layout.push(None);
                continue;
            };
            let depth = match activity.parent {
                None => Some(0),
                Some(parent) => groups.get(&(parent.invocation.0, parent.agent)).and_then(
                    |(depth, expanded)| {
                        if *expanded { Some(depth + 1) } else { None }
                    },
                ),
            };
            if let Some(depth) = depth
                && matches!(activity.key.kind, ActivityKind::Work | ActivityKind::Agent)
            {
                groups.insert(
                    (activity.key.invocation.0, activity.key.agent),
                    (depth, activity.expanded),
                );
            }
            layout.push(depth);
        }
        layout
    }

    fn activity_keys(&self) -> Vec<ActivityKey> {
        self.history
            .iter()
            .zip(self.activity_layout())
            .filter_map(|(message, depth)| match &message.kind {
                MessageKind::Activity(activity) if depth.is_some() && activity.expandable() => {
                    Some(activity.key)
                }
                _ => None,
            })
            .collect()
    }

    fn activity_text(&mut self, key: ActivityKey, text: &str) {
        for message in &mut self.history {
            if let MessageKind::Activity(activity) = &mut message.kind
                && activity.key == key
            {
                message.text.extend(sanitized(text));
                break;
            }
        }
    }

    fn finish_activity(&mut self, key: ActivityKey, status: ActivityStatus) {
        for message in &mut self.history {
            if let MessageKind::Activity(activity) = &mut message.kind
                && activity.key == key
            {
                activity.state = match status {
                    ActivityStatus::Running => ActivityState::Ongoing,
                    status => ActivityState::Finished(status),
                };
                break;
            }
        }
        self.collapse_finished_work();
    }

    fn finish_activities(&mut self, status: ActivityStatus) {
        self.headers.clear();
        for message in &mut self.history {
            if let MessageKind::Activity(activity) = &mut message.kind
                && activity.key.agent.is_none()
                && activity.state == ActivityState::Running
            {
                activity.state = ActivityState::Finished(status);
            }
        }
        self.collapse_finished_work();
    }

    fn collapse_finished_work(&mut self) {
        let running: BTreeSet<_> = self
            .history
            .iter()
            .filter_map(|message| match &message.kind {
                MessageKind::Activity(activity) if activity.state.is_running() => Some(
                    activity
                        .key
                        .agent
                        .map_or(activity.key.invocation, |agent| {
                            self.agents[agent].key.invocation
                        })
                        .0,
                ),
                _ => None,
            })
            .collect();
        let mut finished = Vec::new();
        for message in &mut self.history {
            if let MessageKind::Activity(activity) = &mut message.kind
                && activity.collapse_on_finish
                && !running.contains(&activity.key.invocation.0)
            {
                activity.collapse_on_finish = false;
                finished.push(activity.key);
            }
        }
        for key in finished {
            self.toggle_activity(key, Some(false));
        }
    }

    fn reset_gestures(&mut self) -> bool {
        let changed = self.escape_deadline.is_some() || self.exit_deadline.is_some();
        self.escape_deadline = None;
        self.exit_deadline = None;
        changed
    }

    fn expire_gestures(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for deadline in [&mut self.escape_deadline, &mut self.exit_deadline] {
            if deadline.is_some_and(|deadline| now >= deadline) {
                *deadline = None;
                changed = true;
            }
        }
        changed
    }

    fn loader(&self) -> Option<PodPress> {
        if matches!(self.phase, Phase::Stopping | Phase::Stopped)
            || (self.phase == Phase::Running
                && matches!(
                    self.progress,
                    Progress::Retrying { .. } | Progress::Paused { .. }
                ))
        {
            None
        } else if self.reduced_motion {
            Some(PodPress::Bar)
        } else {
            let index =
                (self.activity_elapsed.as_nanos() / LOADER_TICK_DURATION.as_nanos()) as usize;
            Some(POD_PRESS[index % POD_PRESS.len()])
        }
    }

    fn activity_label(&self) -> &'static str {
        let index = if self.reduced_motion {
            0
        } else {
            (self.activity_elapsed.as_nanos() / ACTIVITY_LABEL_DURATION.as_nanos()) as usize
        };
        ACTIVITY_LABELS[index]
    }

    fn render_history(&mut self, frame: &mut Frame<'_>, area: Rect, selection: &mut SelectionMap) {
        let theme = self.theme;
        let palette = theme.palette();
        self.headers.clear();
        let area = area.inner(Margin::new(2, 0));
        if area.is_empty() {
            return;
        }
        let mut paragraphs = Vec::with_capacity(self.history.len() * 2 + 2);
        let loader = self.loader().map(PodPress::activity);
        let activity_label = self.activity_label();
        let pending_work = self.phase == Phase::Running
            && !self.history.iter().any(|message| {
                matches!(
                    &message.kind,
                    MessageKind::Activity(activity)
                        if activity.key.agent.is_none()
                            && matches!(activity.key.kind, ActivityKind::Work)
                            && Some(activity.key.invocation) == self.invocation
                )
            });
        let layout = self.activity_layout();
        let running_work = self
            .history
            .iter()
            .filter_map(|message| match &message.kind {
                MessageKind::Activity(activity)
                    if !matches!(activity.key.kind, ActivityKind::Work)
                        && activity.state.is_running() =>
                {
                    Some(
                        activity
                            .key
                            .agent
                            .map_or(activity.key.invocation.0, |agent| {
                                self.agents[agent].key.invocation.0
                            }),
                    )
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let mut messages = self
            .history
            .iter_mut()
            .zip(layout)
            .filter(|(message, depth)| {
                depth.is_some() || !matches!(message.kind, MessageKind::Activity(_))
            })
            .peekable();
        while let Some((message, depth)) = messages.next() {
            if let MessageKind::Activity(activity) = &message.kind {
                let Some(depth) = depth else {
                    continue;
                };
                let indent = (depth * 2).min(area.width);
                let width = area.width.saturating_sub(indent);
                let gap = usize::from(
                    !messages
                        .peek()
                        .is_some_and(|(next, _)| matches!(next.kind, MessageKind::Activity(_))),
                );
                let expandable = activity.expandable();
                let expanded = expandable && activity.expanded;
                let selected = self.activity_focus == Some(activity.key) && expandable;
                let style = if selected {
                    theme.selection_style()
                } else {
                    Style::new().fg(palette.muted)
                };
                paragraphs.push(TranscriptPart {
                    body: Text::from(activity.header(
                        width,
                        loader,
                        matches!(activity.key.kind, ActivityKind::Work)
                            && running_work.contains(&activity.key.invocation.0),
                        theme,
                        activity_label,
                    )),
                    style,
                    height: 1,
                    inset: indent,
                    padding: Padding::ZERO,
                    layout: None,
                    gap: if expanded { 0 } else { gap },
                    header: expandable.then_some(activity.key),
                });
                if expanded
                    && !matches!(activity.key.kind, ActivityKind::Work | ActivityKind::Agent)
                {
                    let text = if message.text.is_empty() {
                        "No details yet."
                    } else {
                        &message.text
                    };
                    let file_diff = activity.output == Some(ActivityOutputKind::Diff);
                    let inset = (indent + if file_diff { 2 } else { 0 }).min(area.width);
                    let padding = Padding::new(
                        if file_diff { 0 } else { 2.min(area.width) },
                        0,
                        u16::from(file_diff),
                        u16::from(file_diff),
                    );
                    let (body, text) = if matches!(activity.key.kind, ActivityKind::AgentAnswer) {
                        let rendered = message.markdown.render(
                            text,
                            area.width.saturating_sub(inset + padding.left),
                            theme,
                        );
                        (rendered.text(), rendered.plain.as_str())
                    } else if file_diff {
                        (activity_diff(text, theme), text)
                    } else {
                        (Text::from(text), text)
                    };
                    message.layout.update(
                        &body,
                        text,
                        area.width.saturating_sub(inset + padding.left),
                    );
                    let height =
                        message.layout.height() + usize::from(padding.top + padding.bottom);
                    paragraphs.push(TranscriptPart {
                        body,
                        style: if file_diff {
                            Style::new().bg(palette.surface)
                        } else {
                            Style::new().fg(palette.foreground)
                        },
                        height,
                        inset,
                        padding,
                        layout: Some(&message.layout),
                        gap,
                        header: None,
                    });
                }
                continue;
            }
            let (text, plain) =
                if matches!(message.kind, MessageKind::Assistant | MessageKind::Notice) {
                    let rendered = message.markdown.render(&message.text, area.width, theme);
                    (rendered.text(), rendered.plain.as_str())
                } else {
                    (Text::from(message.text.as_str()), message.text.as_str())
                };
            let (style, padding) = match &message.kind {
                MessageKind::User => {
                    // Spacer rows must scroll with the message, not stay at the viewport edges.
                    (
                        Style::new().bg(palette.surface).fg(palette.foreground),
                        Padding::new(2, 2, 1, 1),
                    )
                }
                MessageKind::Assistant | MessageKind::Notice => (Style::default(), Padding::ZERO),
                MessageKind::Error => (Style::new().fg(palette.error), Padding::ZERO),
                MessageKind::Activity(_) => unreachable!("activities rendered above"),
            };
            message.layout.update(
                &text,
                plain,
                area.width.saturating_sub(padding.left + padding.right),
            );
            let height = message.layout.height() + usize::from(padding.top + padding.bottom);
            paragraphs.push(TranscriptPart {
                body: text,
                style,
                height,
                inset: 0,
                padding,
                layout: Some(&message.layout),
                gap: 1,
                header: None,
            });
        }
        if pending_work {
            let indicator = work_indicator(area.width, loader);
            paragraphs.push(TranscriptPart {
                body: Text::from(Line::from(vec![
                    Span::styled(format!("{indicator} "), Style::new().fg(palette.accent)),
                    Span::raw(fit_label(
                        activity_label,
                        area.width.saturating_sub(indicator.len() as u16 + 1),
                    )),
                ])),
                style: Style::new().fg(palette.muted),
                height: 1,
                inset: 0,
                padding: Padding::ZERO,
                layout: None,
                gap: 1,
                header: None,
            });
        }
        if !self.preview.is_empty() {
            let rendered = self
                .preview_markdown
                .render(&self.preview, area.width, theme);
            let body = rendered.text();
            self.preview_layout
                .update(&body, &rendered.plain, area.width);
            let height = self.preview_layout.height();
            paragraphs.push(TranscriptPart {
                body,
                style: Style::default(),
                height,
                inset: 0,
                padding: Padding::ZERO,
                layout: Some(&self.preview_layout),
                gap: 1,
                header: None,
            });
        }
        let height = paragraphs
            .iter()
            .map(|part| part.height + part.gap)
            .sum::<usize>();
        if self.scroll > 0 && self.transcript_height > 0 {
            self.scroll = self
                .scroll
                .saturating_add(height.saturating_sub(self.transcript_height))
                .saturating_sub(self.transcript_height.saturating_sub(height));
        }
        self.transcript_height = height;
        let maximum = height.saturating_sub(usize::from(area.height));
        self.scroll = self.scroll.min(maximum);
        let mut offset = maximum.saturating_sub(self.scroll);
        if self.reveal_focus {
            let mut top = 0;
            for part in &paragraphs {
                if part.header.is_some() && part.header == self.activity_focus {
                    if top < offset {
                        offset = top;
                    } else if top >= offset + usize::from(area.height) {
                        offset = top + 1 - usize::from(area.height);
                    }
                    break;
                }
                top += part.height + part.gap;
            }
            self.scroll = maximum.saturating_sub(offset);
            self.reveal_focus = false;
        }
        let mut top = 0usize;
        for part in paragraphs {
            if top >= offset + usize::from(area.height) {
                break;
            }
            let TranscriptPart {
                body,
                style,
                height,
                inset,
                padding,
                layout,
                gap,
                header,
            } = part;
            let skipped = offset.saturating_sub(top).min(height);
            let row = top.saturating_sub(offset);
            let visible = height
                .saturating_sub(skipped)
                .min(usize::from(area.height).saturating_sub(row));
            if visible > 0 {
                let row = row as u16;
                let visible = visible as u16;
                let leading = usize::from(padding.top)
                    .saturating_sub(skipped)
                    .min(usize::from(visible)) as u16;
                let target = Rect::new(
                    area.x + inset,
                    area.y + row,
                    area.width.saturating_sub(inset),
                    visible,
                );
                frame.buffer_mut().set_style(target, style);
                if let Some(key) = header {
                    self.headers
                        .push((key, Rect::new(area.x, area.y + row, area.width, 1)));
                }
                if let Some(layout) = layout {
                    let content = Rect::new(
                        target.x + padding.left,
                        target.y + leading,
                        target.width.saturating_sub(padding.left + padding.right),
                        visible.saturating_sub(leading),
                    );
                    let skipped = skipped.saturating_sub(usize::from(padding.top));
                    selection.paragraph(layout, content, skipped);
                    layout.render(&body, skipped, content, frame.buffer_mut());
                } else {
                    Paragraph::new(body)
                        .style(style)
                        .render(target, frame.buffer_mut());
                }
            }
            top += height + gap;
        }
    }

    fn append_history(&mut self, kind: MessageKind, text: &str) {
        self.selection.invalidate();
        let mut entry = match kind {
            MessageKind::Error => String::from("Error: "),
            _ => String::new(),
        };
        entry.extend(sanitized(text));
        self.history.push_back(Message {
            kind,
            text: entry,
            markdown: Markdown::default(),
            layout: WrapLayout::default(),
        });
    }
}

struct Draft {
    composer: TextArea<'static>,
    images: Vec<Attachment>,
}

#[derive(Clone)]
struct Attachment {
    marker: String,
    image: Image,
}

struct Message {
    kind: MessageKind,
    text: String,
    markdown: Markdown,
    layout: WrapLayout,
}

enum MessageKind {
    User,
    Assistant,
    Notice,
    Error,
    Activity(Activity),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ActivityKey {
    invocation: InvocationId,
    agent: Option<usize>,
    kind: ActivityKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActivityKind {
    Work,
    Agent,
    AgentAnswer,
    AgentNotice,
    Tool { operation: OperationId },
}

struct AgentActivity {
    session: u64,
    id: AgentId,
    name: String,
    key: ActivityKey,
}

struct Activity {
    key: ActivityKey,
    parent: Option<ActivityKey>,
    title: String,
    summary: Option<ActivitySummary>,
    output: Option<ActivityOutputKind>,
    provider_notice: Option<String>,
    provider_attempt: u32,
    state: ActivityState,
    expanded: bool,
    collapse_on_finish: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActivityState {
    Running,
    Ongoing,
    Finished(ActivityStatus),
}

impl ActivityState {
    fn is_running(self) -> bool {
        matches!(self, Self::Running | Self::Ongoing)
    }
}

struct TranscriptPart<'a> {
    body: Text<'a>,
    style: Style,
    height: usize,
    inset: u16,
    padding: Padding,
    layout: Option<&'a WrapLayout>,
    gap: usize,
    header: Option<ActivityKey>,
}

impl Activity {
    fn expandable(&self) -> bool {
        if matches!(self.key.kind, ActivityKind::Work | ActivityKind::Agent) {
            true
        } else if self.title.starts_with("Read ") {
            false
        } else if ["Write ", "Edit ", "Running "]
            .iter()
            .any(|prefix| self.title.starts_with(prefix))
        {
            self.output.is_some()
        } else {
            true
        }
    }

    fn header(
        &self,
        width: u16,
        loader: Option<&str>,
        running_children: bool,
        theme: Theme,
        decorative: &'static str,
    ) -> Line<'static> {
        let palette = theme.palette();
        let working = self.state.is_running() || running_children;
        let indicator = if matches!(self.key.kind, ActivityKind::Work | ActivityKind::Agent)
            && self.provider_notice.is_none()
        {
            work_indicator(width, loader.filter(|_| working))
        } else {
            ""
        };
        let indicator = if indicator.trim().is_empty() {
            String::new()
        } else {
            format!("{indicator} ")
        };
        let padding_width = indicator.len() as u16 + 2;
        let mut title = match self.key.kind {
            ActivityKind::Work if working => decorative.to_owned(),
            ActivityKind::Work => "Work Done".to_owned(),
            ActivityKind::Agent if self.provider_notice.is_some() => format!(
                "{} · {}",
                self.title,
                self.provider_notice.as_deref().unwrap_or_default(),
            ),
            ActivityKind::Agent => format!(
                "{}{} {}",
                self.title,
                if self.title == "Agent" || self.title.ends_with(" Agent") {
                    ""
                } else {
                    " Agent"
                },
                if working {
                    decorative
                } else if matches!(
                    self.state,
                    ActivityState::Finished(
                        ActivityStatus::Failed { .. } | ActivityStatus::TimedOut
                    )
                ) {
                    "stopped"
                } else {
                    "Worked"
                }
            ),
            _ => tool_progress_label(&self.title, self.state.is_running())
                .unwrap_or_else(|| self.title.clone()),
        };
        title.push_str(match self.state {
            ActivityState::Finished(ActivityStatus::Interrupted) => " · Interrupted",
            ActivityState::Finished(ActivityStatus::Deferred) => " · Deferred",
            ActivityState::Finished(ActivityStatus::Unknown) => " · Unknown",
            _ => "",
        });
        let arrow = if !self.expandable() {
            ""
        } else if self.expanded {
            "▾"
        } else {
            "▸"
        };
        if width < 4 {
            return Line::from(arrow);
        }
        let status_label = match self.state {
            ActivityState::Finished(ActivityStatus::Failed {
                exit_code: Some(code),
            }) => fit_label(&format!(" · exit {code}"), width - 4),
            ActivityState::Finished(ActivityStatus::Failed { exit_code: None }) => {
                fit_label(" · failed", width - 4)
            }
            ActivityState::Finished(ActivityStatus::Signalled { signal }) => {
                fit_label(&format!(" · {}", signal_label(signal)), width - 4)
            }
            ActivityState::Finished(ActivityStatus::TimedOut) => {
                fit_label(" · timed out", width - 4)
            }
            _ => String::new(),
        };
        let status_width = Line::from(status_label.as_str()).width() as u16;
        let mut summary = Vec::new();
        match self.summary {
            Some(ActivitySummary::LineRange { start, end }) => {
                summary.push(Span::raw(if start == end {
                    format!(" L{start}")
                } else {
                    format!(" L{start}-L{end}")
                }));
            }
            Some(ActivitySummary::LineChanges { added, removed }) => {
                if added > 0 || removed == 0 {
                    summary.push(Span::styled(
                        format!(" +{added}"),
                        if added > 0 {
                            Style::new().fg(palette.success)
                        } else {
                            Style::default()
                        },
                    ));
                }
                if removed > 0 {
                    summary.push(Span::styled(
                        format!(" −{removed}"),
                        Style::new().fg(palette.error),
                    ));
                }
            }
            None => {}
        }
        let mut summary_width = summary.iter().map(Span::width).sum::<usize>() as u16;
        if summary_width + status_width + padding_width + 1 > width {
            summary.clear();
            summary_width = 0;
        }
        let title = fit_label(
            &title,
            width.saturating_sub(padding_width + summary_width + status_width),
        );
        let title_style = if matches!(
            self.state,
            ActivityState::Finished(
                ActivityStatus::Failed { .. }
                    | ActivityStatus::Signalled { .. }
                    | ActivityStatus::TimedOut
            )
        ) {
            Style::new().fg(palette.error)
        } else {
            Style::default()
        };
        let mut spans = vec![
            Span::styled(indicator, Style::new().fg(palette.accent)),
            Span::styled(title, title_style),
            Span::styled(status_label, title_style),
        ];
        spans.extend(summary);
        if !arrow.is_empty() {
            spans.push(Span::raw(format!(" {arrow}")));
        }
        Line::from(spans)
    }
}

impl PodPress {
    fn composer(self) -> &'static str {
        match self {
            Self::Pod => " (o|o) ",
            Self::Press => " (|||) ",
            Self::Mold => " [|||] ",
            Self::Nibs => " [:::] ",
            Self::Bar => " [###] ",
        }
    }

    fn activity(self) -> &'static str {
        match self {
            Self::Pod => "(o)",
            Self::Press => "(|)",
            Self::Mold => "[|]",
            Self::Nibs => "[:]",
            Self::Bar => "[#]",
        }
    }
}

fn new_composer() -> TextArea<'static> {
    let mut composer = TextArea::default();
    composer.set_max_histories(0);
    composer.set_cursor_line_style(Style::default());
    composer.set_wrap_mode(WrapMode::WordOrGlyph);
    composer
}

fn work_indicator(width: u16, loader: Option<&str>) -> &str {
    if width < 15 {
        " "
    } else {
        loader.unwrap_or("   ")
    }
}

fn retry_seconds(retry_at: Instant, now: Instant) -> u64 {
    let remaining = retry_at.saturating_duration_since(now);
    remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0)
}

fn image_path(line: &str) -> Option<ImagePath> {
    let end = line.trim_end().len();
    let begin = line.ceil_char_boundary(end.saturating_sub(4096));
    let mut start = begin;
    let mut quote = None;
    let mut escaped = false;
    let mut decoded = String::new();
    for (offset, character) in line[begin..end].char_indices() {
        if escaped {
            decoded.push(character);
            escaped = false;
        } else if character == '\\' && quote != Some('\'') {
            escaped = true;
        } else if Some(character) == quote {
            quote = None;
        } else if quote.is_some() {
            decoded.push(character);
        } else if matches!(character, '\'' | '"')
            && (decoded.is_empty() || decoded.starts_with('/') || decoded.starts_with('.'))
        {
            quote = Some(character);
        } else if character.is_whitespace() {
            start = begin + offset + character.len_utf8();
            decoded.clear();
        } else {
            decoded.push(character);
        }
    }
    if quote.is_some()
        || escaped
        || line.len() - start > 4096
        || (begin > 0
            && start == begin
            && line[..begin]
                .chars()
                .next_back()
                .is_some_and(|character| !character.is_whitespace()))
    {
        return None;
    }
    let path = if decoded.starts_with("file://") {
        url::Url::parse(&decoded).ok()?.to_file_path().ok()?
    } else if let Some(relative) = decoded.strip_prefix("~/") {
        std::env::home_dir()?.join(relative)
    } else if Path::new(&decoded).is_absolute()
        || decoded.starts_with("./")
        || decoded.starts_with("../")
    {
        PathBuf::from(decoded)
    } else {
        return None;
    };
    if !matches!(
        path.extension()?.to_str()?.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp"
    ) {
        return None;
    }
    Some(ImagePath {
        source: line[start..].to_owned(),
        path,
    })
}

fn signal_label(signal: i32) -> String {
    use rustix::process::Signal;

    for (value, name) in [
        (Signal::HUP, "SIGHUP"),
        (Signal::INT, "SIGINT"),
        (Signal::QUIT, "SIGQUIT"),
        (Signal::ILL, "SIGILL"),
        (Signal::TRAP, "SIGTRAP"),
        (Signal::ABORT, "SIGABRT"),
        (Signal::BUS, "SIGBUS"),
        (Signal::FPE, "SIGFPE"),
        (Signal::KILL, "SIGKILL"),
        (Signal::USR1, "SIGUSR1"),
        (Signal::SEGV, "SIGSEGV"),
        (Signal::USR2, "SIGUSR2"),
        (Signal::PIPE, "SIGPIPE"),
        (Signal::ALARM, "SIGALRM"),
        (Signal::TERM, "SIGTERM"),
    ] {
        if value.as_raw() == signal {
            return name.into();
        }
    }
    format!("signal {signal}")
}

/// Progressive form of a coding tool's activity title, which the tools emit as
/// "Read <target>", "Running <command>" and so on.
fn tool_progress_label(title: &str, running: bool) -> Option<String> {
    for (prefix, working, finished) in [
        ("Read ", "Reading", "Read"),
        ("Write ", "Writing", "Wrote"),
        ("Edit ", "Editing", "Edited"),
        ("Running ", "Running", "Ran"),
    ] {
        if let Some(target) = title.strip_prefix(prefix) {
            let verb = if running { working } else { finished };
            return Some(format!("{verb} {target}"));
        }
    }
    None
}

fn tool_title(name: &str) -> &str {
    match name {
        "cocoa.read" => "Read file",
        "cocoa.write" => "Write file",
        "cocoa.edit" => "Edit file",
        "cocoa.run" => "Running command",
        _ => name,
    }
}

fn fit_label(text: &str, width: u16) -> String {
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

/// Retain the first 32 completion names that fit the 1–64 byte command alphabet.
fn completion_names(candidates: &[String]) -> Vec<String> {
    candidates
        .iter()
        .take(32)
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 64
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
        .cloned()
        .collect()
}

/// Drop the configured provider prefix from a direct `provider/model` label,
/// keeping later slashes in IDs such as `openrouter/anthropic/claude-sonnet-4`.
fn model_label(selection: &str) -> &str {
    match selection.split_once('/') {
        Some((_, model)) if !model.is_empty() => model,
        _ => selection,
    }
}

fn display_label(text: &str, limit: usize) -> String {
    let mut label = String::new();
    if append_sanitized(&mut label, text, limit) {
        while label.len() > limit.saturating_sub('…'.len_utf8()) {
            label.pop();
        }
        label.push('…');
    }
    label.replace('\n', " ")
}

fn directory_title(directory: &str, width: usize) -> String {
    if Line::from(directory).width() <= width {
        return directory.to_owned();
    }
    let components: Vec<_> = directory
        .trim_start_matches("~/")
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    for replaced in 1..components.len() {
        let shortened = "../".repeat(replaced) + &components[replaced..].join("/");
        if Line::from(shortened.as_str()).width() <= width {
            return shortened;
        }
    }
    let basename = components.last().copied().unwrap_or(directory);
    let shortened = format!("../{basename}");
    if Line::from(shortened.as_str()).width() <= width {
        return shortened;
    }
    let mut tail = String::new();
    for character in basename.chars().rev() {
        let candidate = format!("{character}{tail}");
        if Line::from(candidate.as_str()).width() > width.saturating_sub(3) {
            break;
        }
        tail = candidate;
    }
    if width >= 3 {
        format!("../{tail}")
    } else {
        ".".repeat(width)
    }
}

fn sanitized(text: &str) -> impl Iterator<Item = char> + '_ {
    text.chars()
        .map(|character| if character == '\t' { ' ' } else { character })
        .filter(|&character| !character.is_control() || character == '\n')
}

fn append_sanitized(destination: &mut String, text: &str, limit: usize) -> bool {
    for character in sanitized(text) {
        if destination.len() + character.len_utf8() > limit {
            return true;
        }
        destination.push(character);
    }
    false
}
