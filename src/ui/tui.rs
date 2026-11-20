//! Full-screen terminal UI over the small runtime event vocabulary.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, Instant};

use crossterm::cursor;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event as InputEvent, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, disable_raw_mode, enable_raw_mode};
use crossterm::{execute, queue};

use crate::platform::ProcessRole;
use crate::runtime::{Event, Observer, Runtime, SessionSummary, Waiting};
use crate::ui::history::Kind;
use crate::ui::markdown;
use crate::ui::tui_editor::{Edit, Editor};

const POLL: Duration = Duration::from_millis(50);
/// Worker events handled between two polls for the keyboard. The loop always
/// renders and polls after a batch, so a flood of streamed text cannot starve
/// PgUp/PgDown the way draining the channel to empty could.
const EVENT_BATCH: usize = 64;
const TRANSCRIPT_BYTES: usize = 1024 * 1024;
// PgUp/PgDown move by one screen of transcript less this overlap, so the top
// three lines of a page become the bottom three of the next one.
const PAGE_OVERLAP: usize = 3;
const ENABLE_TMUX_KEYS: &str = "\x1b[>4;2m";
const DISABLE_TMUX_KEYS: &str = "\x1b[>4;0m";
// rmux `status::AMBER`: keep shared terminal chrome visually consistent.
// Inline code in the transcript reuses it, so the two ambers stay in step.
pub(super) const STATUS_COLOR: Color = Color::AnsiValue(222);
// The status title is the gear and its variation selector followed by one
// space. U+FE0F is zero-width; StyledLine attaches it to the gear cell so the
// cell model still matches the columns the terminal prints. The title keeps
// those cells in every blink phase: some terminals draw the gear and its
// selector as two symbols, so swapping the gear for spaces would shift the
// status text left and right as it blinks.
const STATUS_TITLE: &str = " \u{26ed}\u{fe0f} ";
/// The gear when it is not shown. Black keeps its cells without the glyph
/// becoming readable on the status bar's dark background.
const GEAR_HIDDEN: Color = Color::Black;
// The working gear blinks at 1 Hz: visible for the first half of each second,
// hidden for the second half.
const BLINK_HALF: Duration = Duration::from_millis(500);

/// The role colors rush's prompt uses: red for System, amber for Interactive,
/// light green for None. The working gear takes the role's color; the fields
/// after it stay chrome.
fn gear_color(role: ProcessRole) -> Color {
    match role {
        ProcessRole::System => Color::AnsiValue(196),
        ProcessRole::Interactive => Color::AnsiValue(214),
        ProcessRole::None => Color::AnsiValue(120),
    }
}

fn gear_visible(elapsed: Duration) -> bool {
    (elapsed.as_millis() / BLINK_HALF.as_millis()).is_multiple_of(2)
}

pub fn run(
    runtime: &mut Runtime,
    initial: Option<String>,
    interactive: bool,
    config: &crate::config::Config,
) -> Result<(), String> {
    runtime.set_interactive(interactive)?;
    let mut screen = Screen::enter()?;
    let mut app = App::new(runtime, interactive)?;
    let result = (|| {
        for notice in runtime.take_startup_notices() {
            app.apply(Event::Notice(notice));
        }
        if let Some(prompt) = initial {
            app.push(Kind::User, prompt.clone(), false);
            run_turn(runtime, prompt, &mut app)?;
            if !interactive {
                app.render()?;
                return Ok(());
            }
        }
        while !app.quit {
            show_runtime_notices(runtime, &mut app);
            app.render()?;
            let input = event::read().map_err(|error| error.to_string())?;
            let Some(action) = app.edit(input) else {
                continue;
            };
            match action {
                Action::Quit => app.quit = true,
                Action::Submit(text) if text.starts_with('/') => {
                    // A command's output is the newest transcript text.
                    app.scroll_to_bottom();
                    let result = handle_command(runtime, &mut app, &text, config);
                    show_runtime_notices(runtime, &mut app);
                    if let Err(error) = result {
                        app.push(Kind::Error, error, false);
                    }
                }
                Action::Submit(text) if !text.trim().is_empty() => {
                    // The submitted prompt is the newest line, so the view
                    // returns to the bottom before the turn's output arrives.
                    app.scroll_to_bottom();
                    app.push(Kind::User, text.clone(), false);
                    if let Err(error) = run_turn(runtime, text, &mut app) {
                        app.push(Kind::Error, error, false);
                    }
                }
                Action::Submit(_) => {}
            }
        }
        Ok(())
    })();
    let leave = screen.leave(&app.session.id);
    result.and(leave)
}

enum Action {
    Submit(String),
    Quit,
}

struct Screen {
    left: bool,
    tmux: bool,
}

/// Motor OS takes one Ctrl+C registration per process, and the setup questions
/// and the session each enter the screen.
fn enable_ctrl_c_events() -> Result<(), String> {
    static ENABLED: OnceLock<Result<(), String>> = OnceLock::new();
    ENABLED
        .get_or_init(|| event::enable_ctrl_c_events().map_err(|error| error.to_string()))
        .clone()
}

impl Screen {
    fn enter() -> Result<Self, String> {
        enable_ctrl_c_events()?;
        enable_raw_mode().map_err(|error| error.to_string())?;
        let screen = Self {
            left: false,
            tmux: std::env::var_os("TMUX").is_some(),
        };
        paint_enter(&mut io::stdout(), screen.tmux)
            .and_then(|()| io::stdout().flush())
            .map_err(|error| error.to_string())?;
        Ok(screen)
    }

    fn leave(&mut self, session: &str) -> Result<(), String> {
        let (width, height) = terminal::size().map_err(|error| error.to_string())?;
        let raw = disable_raw_mode();
        let output = {
            let mut output = io::stdout().lock();
            paint_exit(&mut output, width, height, session, self.tmux).and_then(|()| output.flush())
        };
        self.left = true;
        raw.and(output).map_err(|error| error.to_string())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        if self.left {
            return;
        }
        let _ = disable_raw_mode();
        if self.tmux {
            let _ = execute!(io::stdout(), Print(DISABLE_TMUX_KEYS));
        }
        let _ = execute!(
            io::stdout(),
            PopKeyboardEnhancementFlags,
            ResetColor,
            cursor::Show,
            DisableBracketedPaste
        );
    }
}

struct Entry {
    kind: Kind,
    text: String,
    live: bool,
    /// The entry's wrapped rows, kept between frames so a frame wraps only the
    /// entries whose text changed. See `Entry::refresh`.
    rows: EntryRows,
}

/// An entry's wrapped rows, split at the last completed source line.
#[derive(Default)]
struct EntryRows {
    /// The width the rows were wrapped at; another width starts over.
    width: usize,
    /// Rows of the text up to `settled`.
    rows: Vec<TranscriptLine>,
    /// Byte offset of the end of the last source line wrapped into `rows`.
    settled: usize,
    /// Fenced-code state at `settled`.
    fenced: bool,
    /// Rows of the text after `settled`: the last, still growing source line.
    tail: Vec<TranscriptLine>,
    /// The text length the rows were built from, so a frame that changes
    /// nothing does no work at all.
    taken: usize,
    /// `rows.len() + tail.len()`, so a frame counts rows without walking them.
    count: usize,
}

impl EntryRows {
    fn iter(&self) -> impl Iterator<Item = &TranscriptLine> {
        self.rows.iter().chain(self.tail.iter())
    }
}

impl Entry {
    /// Wrap what changed since the last frame. The text and the width are the
    /// only inputs, so a frame that only scrolls wraps nothing at all, and a
    /// streaming entry wraps only what arrived since its last settled line.
    fn refresh(&mut self, width: usize) {
        let Self {
            kind, text, rows, ..
        } = self;
        if rows.width != width {
            *rows = EntryRows {
                width,
                ..EntryRows::default()
            };
        } else if rows.taken == text.len() {
            // Neither the width nor the text moved since the last frame.
            return;
        }
        let lead_prefix = prefix(*kind);
        let indent = " ".repeat(lead_prefix.len());
        while let Some(end) = text[rows.settled..].find('\n') {
            let line = &text[rows.settled..rows.settled + end];
            rows.settled += end + 1;
            // Only the entry's first source line carries the prefix; everything
            // after it indents by the prefix's width.
            let lead = if rows.rows.is_empty() {
                lead_prefix
            } else {
                indent.as_str()
            };
            let more = source_rows(*kind, line, width, &indent, lead, &mut rows.fenced);
            #[cfg(test)]
            SETTLED_WRAPS.with(|count| count.set(count.get() + 1));
            rows.rows.extend(more);
        }
        // The tail is restyled whenever the text changed: it is one source
        // line's worth, and anything else would have to guess where the next
        // newline lands.
        let mut fenced = rows.fenced;
        let lead = if rows.rows.is_empty() {
            lead_prefix
        } else {
            indent.as_str()
        };
        rows.tail = source_rows(
            *kind,
            &text[rows.settled..],
            width,
            &indent,
            lead,
            &mut fenced,
        );
        rows.count = rows.rows.len() + rows.tail.len();
        rows.taken = text.len();
    }
}

/// The label a transcript row of `kind` starts with; continuations indent by
/// its width so a multi-row entry keeps its columns straight.
fn prefix(kind: Kind) -> &'static str {
    match kind {
        Kind::User => "you> ",
        Kind::Assistant => "agent> ",
        Kind::Reasoning => "think> ",
        Kind::Tool => "tool> ",
        Kind::ToolOutput => "out> ",
        Kind::Notice => "note> ",
        Kind::Error => "error> ",
    }
}

/// Counts the source lines a frame settled, so a test can prove that a frame
/// wraps only what changed instead of the whole transcript. Thread-local, so
/// tests running side by side do not count each other's frames.
#[cfg(test)]
thread_local! {
    static SETTLED_WRAPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One source line as transcript rows, wrapped to `width`. `lead` is what the
/// line's first row starts with: the entry's prefix on its first source line,
/// and the indent that matches the prefix's width on every other one.
fn source_rows(
    kind: Kind,
    line: &str,
    width: usize,
    indent: &str,
    lead: &str,
    fenced: &mut bool,
) -> Vec<TranscriptLine> {
    let available = width.saturating_sub(prefix(kind).len()).max(1);
    let base = color(kind);
    // Only the agent's prose is markdown; the other entries are already
    // structured by their prefix.
    let spans = if kind == Kind::Assistant {
        markdown::source_line(line, base, fenced)
    } else {
        vec![(base.into(), line.to_string())]
    };
    let mut rows = Vec::with_capacity(1);
    for (index, spans) in markdown::wrap(spans, available).into_iter().enumerate() {
        let mut row = TranscriptLine {
            kind,
            spans: Vec::new(),
        };
        markdown::push(&mut row.spans, base, if index == 0 { lead } else { indent });
        for (style, text) in spans {
            markdown::push(&mut row.spans, style, &text);
        }
        rows.push(row);
    }
    rows
}

struct App {
    transcript: VecDeque<Entry>,
    bytes: usize,
    editor: Editor,
    /// Transcript lines the view is scrolled back from the bottom.
    scroll: usize,
    /// The width and transcript line count of the last frame. New lines push
    /// the view up only while it is at the bottom; scrolled back, they only
    /// grow `scroll`.
    shown: Option<(usize, usize)>,
    /// The largest scroll the last frame allowed.
    max_scroll: usize,
    /// The transcript rows PgUp/PgDown move by, from the last frame.
    page_rows: usize,
    status: String,
    model: String,
    context_used: u64,
    context_max: u64,
    session: SessionSummary,
    role: ProcessRole,
    input_enabled: bool,
    /// When the current operation started; drives the gear's working blink.
    busy_since: Instant,
    quit: bool,
    last_frame: Option<Frame>,
}

#[derive(Debug, PartialEq, Eq)]
struct Layout {
    transcript_rows: usize,
    editor_start: usize,
    editor_rows: usize,
    status_row: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StyledChar {
    character: char,
    mark: Option<char>,
    style: markdown::Style,
}

/// The columns a character takes in the terminal's cell grid: none for
/// combining marks and variation selectors, two for the East Asian wide and
/// emoji ranges, one otherwise.
pub(super) fn columns(character: char) -> usize {
    /// Code points that stand on their own cell's glyph without taking one.
    const ZERO: &[(u32, u32)] = &[
        (0x0300, 0x036F), // combining diacritical marks
        (0x1AB0, 0x1AFF), // combining diacritical marks extended
        (0x1DC0, 0x1DFF), // combining diacritical marks supplement
        (0x20D0, 0x20F0), // combining marks for symbols
        (0xFE00, 0xFE0F), // variation selectors
        (0xFE20, 0xFE2F), // combining half marks
    ];
    /// Code points a terminal draws in two cells.
    const WIDE: &[(u32, u32)] = &[
        (0x1100, 0x115F),   // Hangul jamo
        (0x2E80, 0xA4CF),   // CJK radicals, kana, ideographs, Yi
        (0xA960, 0xA97F),   // Hangul jamo extended-a
        (0xAC00, 0xD7A3),   // Hangul syllables
        (0xF900, 0xFAFF),   // CJK compatibility ideographs
        (0xFE30, 0xFE6F),   // CJK compatibility forms
        (0xFF00, 0xFF60),   // fullwidth forms
        (0xFFE0, 0xFFE6),   // fullwidth signs
        (0x1F000, 0x1FAFF), // emoji and pictographs
        (0x20000, 0x3FFFD), // CJK extension planes
    ];
    let code = character as u32;
    let inside = |ranges: &[(u32, u32)]| {
        ranges
            .iter()
            .any(|&(low, high)| (low..=high).contains(&code))
    };
    if inside(ZERO) {
        0
    } else if inside(WIDE) {
        2
    } else {
        1
    }
}

/// The columns a glyph takes: its character's width, but at least two when
/// U+FE0F asks the terminal for the emoji presentation. Under-counting a glyph
/// is what must not happen: a row then prints past the terminal's width, wraps,
/// and scrolls every row of the screen out from under the last frame.
fn glyph_columns(character: char, mark: Option<char>) -> usize {
    let columns = columns(character);
    if mark == Some('\u{fe0f}') {
        columns.max(2)
    } else {
        columns
    }
}

fn cell_columns(cell: &StyledChar) -> usize {
    glyph_columns(cell.character, cell.mark)
}

fn text_columns(text: &str) -> usize {
    let characters: Vec<char> = text.chars().collect();
    characters
        .iter()
        .enumerate()
        .map(|(index, character)| {
            let mark = characters
                .get(index + 1)
                .copied()
                .filter(|next| matches!(next, '\u{fe0e}' | '\u{fe0f}'));
            glyph_columns(*character, mark)
        })
        .sum()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StyledLine(Vec<StyledChar>);

impl StyledLine {
    fn columns(&self) -> usize {
        self.0.iter().map(cell_columns).sum()
    }

    /// A bare `Color` is a plain style, so the chrome's callers stay unchanged.
    /// `width` is in columns: a glyph that does not fit stops the line, so no
    /// row ever prints wider than the terminal.
    fn push(&mut self, style: impl Into<markdown::Style>, text: &str, width: usize) {
        let style = style.into();
        let mut used = self.columns();
        for character in text.chars() {
            if matches!(character, '\u{fe0e}' | '\u{fe0f}') {
                if let Some(last) = self.0.last_mut() {
                    let widened = glyph_columns(last.character, Some(character));
                    if used - cell_columns(last) + widened <= width {
                        used = used - cell_columns(last) + widened;
                        last.mark = Some(character);
                    }
                }
                continue;
            }
            let columns = columns(character);
            if used + columns > width {
                break;
            }
            used += columns;
            self.0.push(StyledChar {
                character,
                mark: None,
                style,
            });
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Frame {
    width: usize,
    lines: Vec<StyledLine>,
    cursor: Option<(u16, u16)>,
}

impl Frame {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            lines: vec![StyledLine::default(); height],
            cursor: None,
        }
    }
}

fn screen_layout(height: usize, editor_rows: usize) -> Layout {
    let status_row = height.saturating_sub(1);
    let available = status_row;
    let editor_rows = editor_rows.min(available.saturating_sub(1));
    let editor_start = status_row.saturating_sub(editor_rows);
    Layout {
        transcript_rows: editor_start,
        editor_start,
        editor_rows,
        status_row,
    }
}

impl App {
    fn begin_operation(&mut self) {
        self.input_enabled = false;
        self.busy_since = Instant::now();
        // The runtime reports its own waits once the turn starts; until then the
        // turn is reading the prompt and the session.
        self.status = "starting turn".to_string();
    }

    fn new(runtime: &Runtime, interactive: bool) -> Result<Self, String> {
        let mut app = Self {
            transcript: VecDeque::new(),
            bytes: 0,
            editor: Editor::new(),
            scroll: 0,
            shown: None,
            max_scroll: 0,
            page_rows: 1,
            status: "idle".to_string(),
            model: runtime.model().to_string(),
            context_used: 0,
            context_max: 1,
            session: runtime.summary(),
            role: crate::platform::process_role(),
            input_enabled: true,
            busy_since: Instant::now(),
            quit: false,
            last_frame: None,
        };
        if interactive {
            app.restore_history(runtime)?;
        }
        app.refresh(runtime)?;
        Ok(app)
    }

    fn restore_history(&mut self, runtime: &Runtime) -> Result<(), String> {
        self.transcript.clear();
        self.bytes = 0;
        self.scroll = 0;
        self.shown = None;
        super::history::replay(runtime.session(), |kind, text| {
            self.push(kind, text, false);
            Ok(())
        })?;
        self.refresh(runtime)?;
        self.status = "idle".into();
        self.invalidate();
        Ok(())
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Text(text) => {
                self.status = "responding".to_string();
                self.push(Kind::Assistant, text, true);
            }
            Event::Reasoning(text) => {
                self.status = "thinking".to_string();
                self.push(Kind::Reasoning, text, true);
            }
            Event::ToolStart { name, detail, .. } => {
                self.status = format!("running {name}");
                self.push(Kind::Tool, format!("{name}> {detail}"), false);
            }
            Event::ToolOutput { name, stream, text } => {
                self.push(Kind::ToolOutput, format!("{name} {stream:?}> {text}"), true)
            }
            Event::ToolEnd {
                name,
                content,
                is_error,
                ..
            } => {
                // The result goes back to the model next, and `Waiting` reports
                // that wait; the outcome already has its transcript line.
                self.push(
                    if is_error { Kind::Error } else { Kind::Tool },
                    format!("{name}> {}", fold(&content)),
                    false,
                );
            }
            Event::Waiting(Waiting::Model) => self.status = "waiting for model".to_string(),
            Event::Waiting(Waiting::ToolOutput) => {
                self.status = "processing tool output".to_string()
            }
            Event::Waiting(Waiting::Compaction) => self.status = "compacting".to_string(),
            Event::Notice(text) => self.push(Kind::Notice, text, false),
            Event::Usage { .. } => {}
            Event::ContextUsage { used, max } => {
                self.context_used = used;
                self.context_max = max;
            }
            Event::ModelChanged(model) => self.model = model,
            Event::SessionChanged(session) => self.session = session,
            Event::TurnEnd { .. } => self.status = "idle".to_string(),
        }
    }

    fn push(&mut self, kind: Kind, text: String, live: bool) {
        let text = clean(&text);
        let text = suffix(&text, TRANSCRIPT_BYTES);
        if text.is_empty() {
            return;
        }
        if live
            && self
                .transcript
                .back()
                .is_some_and(|entry| entry.live && entry.kind == kind)
        {
            let entry = self.transcript.back_mut().expect("live entry exists");
            self.bytes -= entry.text.len();
            entry.text.push_str(&text);
            if entry.text.len() > TRANSCRIPT_BYTES {
                entry.text = suffix(&entry.text, TRANSCRIPT_BYTES);
                // The text lost its front, so the rows settled against it no
                // longer line up; the next frame wraps the entry again.
                entry.rows = EntryRows::default();
            }
            self.bytes += entry.text.len();
        } else {
            for entry in &mut self.transcript {
                entry.live = false;
            }
            self.bytes += text.len();
            self.transcript.push_back(Entry {
                kind,
                text,
                live,
                rows: EntryRows::default(),
            });
        }
        while self.bytes > TRANSCRIPT_BYTES {
            let removed = self.transcript.pop_front().expect("transcript has bytes");
            self.bytes -= removed.text.len();
        }
    }

    /// Return the view to the newest transcript line.
    fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
    }

    /// PgUp/PgDown move by one screen of transcript, overlapping by
    /// `PAGE_OVERLAP` lines so the top three lines become the bottom three.
    /// Scrolling back stops at the oldest line.
    fn page(&mut self, up: bool) {
        self.scroll = if up {
            self.scroll
                .saturating_add(self.page_rows)
                .min(self.max_scroll)
        } else {
            self.scroll.saturating_sub(self.page_rows)
        };
    }

    /// New transcript lines push the view up only while it is at the bottom.
    /// Scrolled back, the same lines stay on screen and only the hidden count
    /// grows. A width change rewraps everything, so it is not a scroll.
    fn follow(&mut self, width: usize, lines: usize) {
        if self.scroll != 0
            && let Some((shown_width, shown_lines)) = self.shown
            && shown_width == width
        {
            let added = lines as isize - shown_lines as isize;
            self.scroll = self.scroll.saturating_add_signed(added);
        }
        self.shown = Some((width, lines));
    }

    fn edit(&mut self, input: InputEvent) -> Option<Action> {
        match input {
            InputEvent::Resize(_, _) => None,
            InputEvent::Paste(text) => {
                self.editor.insert_paste(&text);
                None
            }
            InputEvent::Key(key) if key.kind == KeyEventKind::Press => self.key(key),
            _ => None,
        }
    }

    fn key(&mut self, key: KeyEvent) -> Option<Action> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if control => {
                if self.editor.text().is_empty() {
                    Some(Action::Quit)
                } else {
                    self.editor.submit();
                    None
                }
            }
            KeyCode::Char('d') if control && self.editor.text().is_empty() => Some(Action::Quit),
            KeyCode::PageUp => {
                self.page(true);
                None
            }
            KeyCode::PageDown => {
                self.page(false);
                None
            }
            KeyCode::Enter if !key.modifiers.is_empty() => {
                self.editor.insert_char('\n');
                None
            }
            KeyCode::Char('j') if control => {
                self.editor.insert_char('\n');
                None
            }
            KeyCode::Char('\n') => {
                self.editor.insert_char('\n');
                None
            }
            KeyCode::Enter => {
                if self.editor.continue_after_backslash() {
                    None
                } else {
                    Some(Action::Submit(self.editor.submit()))
                }
            }
            KeyCode::Backspace => {
                self.editor.backspace();
                None
            }
            KeyCode::Delete => {
                self.editor.delete();
                None
            }
            KeyCode::Left => {
                self.editor.left();
                None
            }
            KeyCode::Right => {
                self.editor.right();
                None
            }
            KeyCode::Home => {
                self.editor.home();
                None
            }
            KeyCode::End => {
                self.editor.end();
                None
            }
            KeyCode::Up => {
                self.editor.older();
                None
            }
            KeyCode::Down => {
                self.editor.newer();
                None
            }
            KeyCode::Tab => {
                self.editor.insert_char('\t');
                None
            }
            KeyCode::Char(character) if !control && !character.is_control() => {
                self.editor.insert_char(character);
                None
            }
            _ => None,
        }
    }

    fn replace_draft(&mut self, draft: String) {
        if self.editor.replace(draft) == Edit::Full {
            self.push(
                Kind::Error,
                "selected prompt exceeds editor limit".to_string(),
                false,
            );
        }
    }

    fn refresh(&mut self, runtime: &Runtime) -> Result<(), String> {
        self.model = runtime.model().to_string();
        self.session = runtime.summary();
        let (used, max) = runtime.context_usage()?;
        self.context_used = used;
        self.context_max = max;
        Ok(())
    }

    fn render(&mut self) -> Result<(), String> {
        let (width, height) = terminal::size().map_err(|error| error.to_string())?;
        let next = self.frame(width, height);
        let mut output = io::stdout().lock();
        paint_frame(&mut output, self.last_frame.as_ref(), &next)
            .map_err(|error| error.to_string())?;
        output.flush().map_err(|error| error.to_string())?;
        self.last_frame = Some(next);
        Ok(())
    }

    fn invalidate(&mut self) {
        self.last_frame = None;
    }

    fn frame(&mut self, width: u16, height: u16) -> Frame {
        let width = usize::from(width).max(1);
        let height = usize::from(height);
        let mut frame = Frame::new(width, height);
        if height == 0 {
            return frame;
        }
        let editor_width = width.saturating_sub(2).max(1);
        let draft_lines = wrap(self.editor.text(), editor_width);
        let (cursor_row, cursor_column) =
            editor_cursor(self.editor.text(), self.editor.cursor(), editor_width);
        let layout = screen_layout(height, draft_lines.len());
        // Frames used to re-wrap the whole transcript, which is what starved the
        // keyboard while a big reply streamed in: the caches make a frame cost
        // the entries whose text changed plus the rows on screen.
        self.refresh_rows(width);
        let total = self.transcript.iter().map(|entry| entry.rows.count).sum();
        self.follow(width, total);
        let max_scroll = total.saturating_sub(layout.transcript_rows);
        self.scroll = self.scroll.min(max_scroll);
        self.max_scroll = max_scroll;
        self.page_rows = layout.transcript_rows.saturating_sub(PAGE_OVERLAP).max(1);
        let end = total - self.scroll;
        let start = end.saturating_sub(layout.transcript_rows);
        let transcript_start = layout.transcript_rows.saturating_sub(end - start);
        self.paint_transcript(&mut frame, width, start, end, transcript_start);
        let editor_first = cursor_row
            .saturating_add(1)
            .saturating_sub(layout.editor_rows)
            .min(draft_lines.len().saturating_sub(layout.editor_rows));
        for (shown, (index, line)) in draft_lines
            .iter()
            .enumerate()
            .skip(editor_first)
            .take(layout.editor_rows)
            .enumerate()
        {
            let prefix = if index == 0 { "> " } else { "  " };
            let target = &mut frame.lines[layout.editor_start + shown];
            target.push(
                if self.input_enabled {
                    STATUS_COLOR
                } else {
                    Color::DarkGrey
                },
                prefix,
                width,
            );
            target.push(Color::White, line, width);
        }
        // The gear shows while working, blinking at 1 Hz in the role's color;
        // the rest of the time it is painted black in place. The rest of the
        // bar stays chrome.
        let status = single_line(&format!(
            "{} | {} | {}{} | {}",
            context_status(self.context_used, self.context_max),
            self.model,
            self.session.name.as_deref().unwrap_or(&self.session.id),
            if self.session.ephemeral {
                " (ephemeral)"
            } else {
                ""
            },
            self.status
        ));
        // The status row is the terminal's last one, and a terminal that wraps
        // eagerly scrolls a line as soon as that row's final column is written.
        // The bar stops one column short: an empty last column never scrolls.
        let bar = width.saturating_sub(1);
        let line = &mut frame.lines[layout.status_row];
        let gear = if !self.input_enabled && gear_visible(self.busy_since.elapsed()) {
            gear_color(self.role)
        } else {
            GEAR_HIDDEN
        };
        line.push(gear, STATUS_TITLE, bar);
        // The hidden-line count is right-aligned and keeps its cells, so the
        // status text gives up the room for it.
        let hidden = (self.scroll != 0).then(|| hidden_status(self.scroll));
        let reserve = hidden.as_ref().map_or(0, |hidden| text_columns(hidden) + 1);
        let room = bar.saturating_sub(text_columns(STATUS_TITLE) + reserve);
        line.push(STATUS_COLOR, &clipped(&status, room), bar);
        if let Some(hidden) = hidden {
            let pad = bar.saturating_sub(line.columns() + text_columns(&hidden));
            line.push(STATUS_COLOR, &" ".repeat(pad), bar);
            line.push(STATUS_COLOR, &hidden, bar);
        }
        if layout.editor_rows != 0 {
            let row = layout.editor_start + cursor_row.saturating_sub(editor_first);
            let column = (cursor_column + 2).min(width.saturating_sub(1));
            frame.cursor = Some((column as u16, row as u16));
        }
        frame
    }

    /// Bring every entry's rows up to date. Only the entries whose text changed
    /// and the width are work: a frame that only scrolls wraps nothing.
    fn refresh_rows(&mut self, width: usize) {
        for entry in &mut self.transcript {
            entry.refresh(width);
        }
    }

    /// Draw the transcript rows `[start, end)` at frame row `first` and below.
    fn paint_transcript(
        &self,
        frame: &mut Frame,
        width: usize,
        start: usize,
        end: usize,
        first: usize,
    ) {
        let mut base = 0;
        for entry in &self.transcript {
            let count = entry.rows.count;
            let next = base + count;
            if next <= start {
                base = next;
                continue;
            }
            if base >= end {
                break;
            }
            let from = start.max(base) - base;
            let to = end.min(next) - base;
            for (index, line) in entry.rows.iter().skip(from).take(to - from).enumerate() {
                let target = &mut frame.lines[first + base + from + index - start];
                for (style, text) in &line.spans {
                    target.push(*style, text, width);
                }
            }
            base = next;
        }
    }

    /// Every wrapped row, for the tests that check the wrapping itself.
    #[cfg(test)]
    fn transcript_lines(&mut self, width: usize) -> Vec<TranscriptLine> {
        self.refresh_rows(width);
        self.transcript
            .iter()
            .flat_map(|entry| entry.rows.iter().cloned())
            .collect()
    }
}

/// One transcript row: the entry it came from and the styled spans it draws.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TranscriptLine {
    kind: Kind,
    spans: Vec<(markdown::Style, String)>,
}

/// Repaint the cells that changed. The frame's last row stays one column short
/// of `width`: writing that cell would scroll an eager-wrap terminal.
fn paint_frame<W: Write>(output: &mut W, previous: Option<&Frame>, next: &Frame) -> io::Result<()> {
    let full = previous.is_none_or(|previous| {
        previous.width != next.width || previous.lines.len() != next.lines.len()
    });
    let changed = full
        || previous.is_some_and(|previous| {
            previous
                .lines
                .iter()
                .zip(&next.lines)
                .any(|(old, new)| old != new)
        });
    if changed {
        queue!(output, cursor::Hide)?;
    }
    if full {
        queue!(output, cursor::MoveTo(0, 0), Clear(ClearType::All))?;
    }
    for (row, new) in next.lines.iter().enumerate() {
        let old = (!full).then(|| &previous.expect("non-full frame exists").lines[row]);
        if old == Some(new) || full && new.0.is_empty() {
            continue;
        }
        paint_line(output, row as u16, old, new)?;
    }
    match next.cursor {
        Some((column, row)) => queue!(output, cursor::MoveTo(column, row), cursor::Show)?,
        None => queue!(output, cursor::Hide)?,
    }
    Ok(())
}

fn paint_enter<W: Write>(output: &mut W, tmux: bool) -> io::Result<()> {
    if tmux {
        // tmux uses modifyOtherKeys requests, even when forwarding CSI-u keys.
        queue!(output, Print(ENABLE_TMUX_KEYS))?;
    }
    queue!(
        output,
        EnableBracketedPaste,
        // Ask capable terminals to distinguish modified Enter from bare CR.
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        cursor::Hide
    )
}

fn paint_exit<W: Write>(
    output: &mut W,
    width: u16,
    height: u16,
    session: &str,
    tmux: bool,
) -> io::Result<()> {
    if tmux {
        queue!(output, Print(DISABLE_TMUX_KEYS))?;
    }
    let message = single_line(&format!("Motor OS Gears session {session} exited."));
    queue!(
        output,
        PopKeyboardEnhancementFlags,
        cursor::MoveTo(0, height.saturating_sub(1)),
        Clear(ClearType::CurrentLine),
        SetForegroundColor(STATUS_COLOR),
        Print(clipped(&message, usize::from(width))),
        ResetColor,
        Print("\r\n\r\n"),
        cursor::Show,
        DisableBracketedPaste
    )
}

fn paint_line<W: Write>(
    output: &mut W,
    row: u16,
    previous: Option<&StyledLine>,
    next: &StyledLine,
) -> io::Result<()> {
    let common = previous.map_or(0, |previous| {
        previous
            .0
            .iter()
            .zip(&next.0)
            .take_while(|(old, new)| old == new && cell_columns(old) == 1)
            .count()
    });
    // The cursor moves in columns, which the cells it skips do not all share.
    // A cell wider than one column ends the comparison above: a terminal that
    // draws that glyph in a different number of cells than counted here would
    // otherwise take everything after it along with the shift.
    let column: usize = next.0[..common].iter().map(cell_columns).sum();
    queue!(output, cursor::MoveTo(column as u16, row))?;
    let mut index = common;
    // The previous row ended with ResetColor, which clears attributes too, so
    // the first run only pays for the attribute when it is bold. A run costs an
    // extra escape only when its weight differs from the run before it.
    let mut bold = false;
    while index < next.0.len() {
        let style = next.0[index].style;
        let mut text = String::new();
        while index < next.0.len() && next.0[index].style == style {
            text.push(next.0[index].character);
            if let Some(mark) = next.0[index].mark {
                text.push(mark);
            }
            index += 1;
        }
        if style.bold != bold {
            bold = style.bold;
            queue!(
                output,
                SetAttribute(if bold {
                    Attribute::Bold
                } else {
                    Attribute::NormalIntensity
                })
            )?;
        }
        queue!(output, SetForegroundColor(style.color), Print(text))?;
    }
    if previous.is_some_and(|previous| previous.columns() > next.columns()) {
        queue!(output, Clear(ClearType::UntilNewLine))?;
    }
    queue!(output, ResetColor)
}

enum TurnMessage {
    Event(Event),
    Done(Result<(), String>),
}

fn run_turn(runtime: &mut Runtime, prompt: String, app: &mut App) -> Result<(), String> {
    run_operation(runtime, super::Operation::Turn(prompt), app)
}

fn run_operation(
    runtime: &mut Runtime,
    operation: super::Operation,
    app: &mut App,
) -> Result<(), String> {
    app.begin_operation();
    app.render()?;
    let cancellation = runtime.cancellation();
    let (sender, receiver) = mpsc::channel();
    let result = std::thread::scope(|scope| {
        let runtime = &mut *runtime;
        let worker_sender = sender.clone();
        scope.spawn(move || {
            let mut observer = ChannelObserver {
                sender: worker_sender.clone(),
            };
            let result = operation.run(runtime, &mut observer);
            let _ = sender.send(TurnMessage::Done(result));
        });
        let mut blink_phase = None;
        loop {
            let mut dirty = false;
            // Bounded: a worker that floods events must not keep the loop away
            // from the keyboard, so a batch is always followed by a frame and a
            // poll for keys.
            for _ in 0..EVENT_BATCH {
                match receiver.try_recv() {
                    Ok(message) => match message {
                        TurnMessage::Event(event) => {
                            app.apply(event);
                            dirty = true;
                        }
                        TurnMessage::Done(result) => {
                            app.input_enabled = true;
                            app.status = "idle".to_string();
                            app.render()?;
                            return result;
                        }
                    },
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        app.input_enabled = true;
                        return Err("agent worker stopped unexpectedly".to_string());
                    }
                }
            }
            // Repaint on events and on each gear blink edge (1 Hz).
            let visible = gear_visible(app.busy_since.elapsed());
            if dirty || blink_phase != Some(visible) {
                app.render()?;
                blink_phase = Some(visible);
            }
            if event::poll(POLL).map_err(|error| error.to_string())? {
                match event::read().map_err(|error| error.to_string())? {
                    InputEvent::Key(key)
                        if key.kind == KeyEventKind::Press
                            && key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        cancellation.cancel();
                        app.status = "cancelling".to_string();
                    }
                    InputEvent::Key(key)
                        if key.kind == KeyEventKind::Press && key.code == KeyCode::PageUp =>
                    {
                        app.page(true);
                    }
                    InputEvent::Key(key)
                        if key.kind == KeyEventKind::Press && key.code == KeyCode::PageDown =>
                    {
                        app.page(false);
                    }
                    InputEvent::Resize(_, _) => {}
                    _ => {}
                }
                app.render()?;
            }
        }
    });
    app.refresh(runtime).and(result)
}

fn run_compact(runtime: &mut Runtime, focus: Option<String>, app: &mut App) -> Result<(), String> {
    app.input_enabled = false;
    app.busy_since = Instant::now();
    app.status = "compacting".to_string();
    app.render()?;
    let cancellation = runtime.cancellation();
    let (sender, receiver) = mpsc::channel();
    let result = std::thread::scope(|scope| {
        let runtime = &mut *runtime;
        let worker_sender = sender.clone();
        scope.spawn(move || {
            let mut observer = ChannelObserver {
                sender: worker_sender,
            };
            let result = runtime.compact(focus, &mut observer);
            let _ = sender.send(TurnMessage::Done(result));
        });
        let mut blink_phase = None;
        loop {
            let mut dirty = false;
            for _ in 0..EVENT_BATCH {
                match receiver.try_recv() {
                    Ok(TurnMessage::Event(event)) => {
                        app.apply(event);
                        dirty = true;
                    }
                    Ok(TurnMessage::Done(result)) => {
                        app.input_enabled = true;
                        app.status = "idle".to_string();
                        app.render()?;
                        return result;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        app.input_enabled = true;
                        return Err("compaction worker stopped unexpectedly".to_string());
                    }
                }
            }
            // Repaint on events and on each gear blink edge (1 Hz).
            let visible = gear_visible(app.busy_since.elapsed());
            if dirty || blink_phase != Some(visible) {
                app.render()?;
                blink_phase = Some(visible);
            }
            if event::poll(POLL).map_err(|error| error.to_string())? {
                match event::read().map_err(|error| error.to_string())? {
                    InputEvent::Key(key)
                        if key.kind == KeyEventKind::Press
                            && key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        cancellation.cancel();
                        app.status = "cancelling".to_string();
                    }
                    InputEvent::Resize(_, _) => {}
                    _ => {}
                }
                app.render()?;
            }
        }
    });
    app.refresh(runtime).and(result)
}

struct ChannelObserver {
    sender: mpsc::Sender<TurnMessage>,
}

impl Observer for ChannelObserver {
    fn event(&mut self, event: Event) -> Result<(), String> {
        self.sender
            .send(TurnMessage::Event(event))
            .map_err(|_| "terminal UI closed".to_string())
    }
}

fn handle_command(
    runtime: &mut Runtime,
    app: &mut App,
    text: &str,
    config: &crate::config::Config,
) -> Result<(), String> {
    let (name, argument) = text[1..]
        .split_once(char::is_whitespace)
        .map_or((&text[1..], ""), |(name, rest)| (name, rest.trim()));
    match name {
        "quit" | "exit" => app.quit = true,
        "help" => app.push(Kind::Notice, super::help(runtime), false),
        "status" => app.push(
            Kind::Notice,
            format!(
                "provider: {}\nmodel: {}\nusage: {}\nsession: {}",
                runtime.provider_name(),
                runtime.model(),
                runtime.usage().summary(),
                runtime.summary().id
            ),
            false,
        ),
        "tools" => app.push(Kind::Notice, super::format_tools(runtime), false),
        "context" => app.push(
            Kind::Notice,
            super::context_command(runtime, argument)?,
            false,
        ),
        "session" => {
            let session = runtime.summary();
            app.push(
                Kind::Notice,
                format!(
                    "id: {}\npath: {}\nname: {}\nentries: {}",
                    session.id,
                    session
                        .path
                        .as_deref()
                        .map_or("<ephemeral>".to_string(), |path| path.display().to_string()),
                    session.name.as_deref().unwrap_or(""),
                    session.entries
                ),
                false,
            );
        }
        "name" => runtime.set_name(argument)?,
        "new" => {
            runtime.new_session(argument == "--ephemeral", None)?;
            app.restore_history(runtime)?;
            show_runtime_notices(runtime, app);
        }
        "setup" => match runtime.setup(&mut AppAsk(app))? {
            Some(chosen) => {
                runtime.apply_setup(&chosen.provider, chosen.model, &mut AppObserver(app))?;
            }
            None => app.push(Kind::Notice, "Setup cancelled.".into(), false),
        },
        "reset" => {
            let choices = ["No".to_string(), "Yes".to_string()];
            if picker(app, crate::reset::DESCRIPTION, &choices)? != Some(1) {
                app.push(Kind::Notice, "Reset cancelled.".into(), false);
                return Ok(());
            }
            let removed = runtime.reset()?;
            app.restore_history(runtime)?;
            app.push(Kind::Notice, super::reset_notice(removed), false);
            show_runtime_notices(runtime, app);
        }
        "resume" => {
            let selector = if argument.is_empty() {
                let sessions = runtime.list_sessions()?;
                let labels = sessions
                    .iter()
                    .map(|session| {
                        format!(
                            "{}  {}  {} messages",
                            session.id,
                            session.name.as_deref().unwrap_or(""),
                            session.messages
                        )
                    })
                    .collect::<Vec<_>>();
                let selected =
                    picker(app, "Resume session", &labels)?.ok_or("session selection cancelled")?;
                sessions[selected].id.clone()
            } else {
                argument.to_string()
            };
            runtime.resume(&selector)?;
            app.restore_history(runtime)?;
            show_runtime_notices(runtime, app);
        }
        "label" => {
            let (entry, label) = argument
                .split_once(char::is_whitespace)
                .map_or((argument, ""), |(entry, label)| (entry, label.trim()));
            if entry.is_empty() {
                return Err("usage: /label ENTRY [TEXT]".to_string());
            }
            runtime.set_label(entry, (!label.is_empty()).then_some(label))?;
        }
        "tree" => {
            let id = if argument.is_empty() {
                let tree = runtime.tree();
                let labels = tree
                    .iter()
                    .map(|item| {
                        format!(
                            "{}{} [{}] {}",
                            if item.active { "* " } else { "  " },
                            item.id,
                            item.kind,
                            item.summary
                        )
                    })
                    .collect::<Vec<_>>();
                let selected = picker(
                    app,
                    "Select branch (shell side effects are unchanged)",
                    &labels,
                )?
                .ok_or("tree selection cancelled")?;
                tree[selected].id.clone()
            } else {
                argument.to_string()
            };
            let draft = runtime.select_entry(&id)?;
            app.restore_history(runtime)?;
            if let Some(draft) = draft {
                app.replace_draft(draft);
            }
            app.push(
                Kind::Notice,
                "conversation branch changed; shell side effects were not undone".to_string(),
                false,
            );
        }
        "fork" => {
            let id = if argument.is_empty() {
                let tree = runtime
                    .tree()
                    .into_iter()
                    .filter(|item| item.user_message)
                    .collect::<Vec<_>>();
                let labels = tree
                    .iter()
                    .map(|item| format!("{} {}", item.id, item.summary))
                    .collect::<Vec<_>>();
                let selected = picker(app, "Fork from user message", &labels)?
                    .ok_or("fork selection cancelled")?;
                tree[selected].id.clone()
            } else {
                argument.to_string()
            };
            let draft = runtime.fork_at(&id)?;
            app.restore_history(runtime)?;
            show_runtime_notices(runtime, app);
            app.replace_draft(draft);
            app.push(
                Kind::Notice,
                "new session forked; shell side effects were not undone".to_string(),
                false,
            );
        }
        "clone" => {
            runtime.clone_session()?;
            app.restore_history(runtime)?;
            show_runtime_notices(runtime, app);
            app.push(
                Kind::Notice,
                "active branch cloned; shell side effects were not undone".to_string(),
                false,
            );
        }
        "compact" => {
            app.status = "compacting".to_string();
            run_compact(
                runtime,
                (!argument.is_empty()).then(|| argument.to_string()),
                app,
            )?;
        }
        "model" => {
            // A model change is always saved as the default; --default is
            // accepted for compatibility.
            let (argument, _) = super::selection_and_default(argument);
            let model = if argument.is_empty() {
                let mut choices = vec![runtime.model().to_string()];
                for model in super::model_choices(config, runtime.provider_name()) {
                    if !choices.contains(&model) {
                        choices.push(model);
                    }
                }
                let selected =
                    picker(app, "Select model", &choices)?.ok_or("model selection cancelled")?;
                choices[selected].clone()
            } else {
                argument.to_string()
            };
            runtime.set_model(model, &mut AppObserver(app))?;
            let notice = match runtime.save_default_selection() {
                Ok(()) => "saved provider and model defaults in gears.toml".to_string(),
                Err(error) => format!("could not save provider and model defaults: {error}"),
            };
            app.push(Kind::Notice, notice, false);
        }
        "provider" => {
            let (argument, save_default) = super::selection_and_default(argument);
            let provider = if argument.is_empty() {
                let mut choices = vec![runtime.provider_name().to_string()];
                for provider in config.providers.keys() {
                    if !choices.contains(provider) {
                        choices.push(provider.clone());
                    }
                }
                let selected = picker(app, "Select provider", &choices)?
                    .ok_or("provider selection cancelled")?;
                choices[selected].clone()
            } else {
                argument.to_string()
            };
            runtime.ask_provider_key(&provider, &mut AppAsk(app))?;
            runtime.set_provider(&provider, &mut AppObserver(app))?;
            if save_default {
                runtime.save_default_selection()?;
                app.push(
                    Kind::Notice,
                    "saved provider and model defaults in gears.toml".to_string(),
                    false,
                );
            }
        }
        "" => {}
        other => {
            app.push(Kind::Error, format!("unknown command /{other}"), false);
        }
    }
    app.refresh(runtime)?;
    Ok(())
}

fn show_runtime_notices(runtime: &mut Runtime, app: &mut App) {
    for notice in runtime.take_startup_notices() {
        app.push(Kind::Notice, notice, false);
    }
}

struct AppObserver<'a>(&'a mut App);

impl Observer for AppObserver<'_> {
    fn event(&mut self, event: Event) -> Result<(), String> {
        self.0.apply(event);
        self.0.render()
    }
}

fn picker(app: &mut App, title: &str, items: &[String]) -> Result<Option<usize>, String> {
    let selected = pick(title, items);
    app.invalidate();
    app.render()?;
    selected
}

fn pick(title: &str, items: &[String]) -> Result<Option<usize>, String> {
    if items.is_empty() {
        return Err("nothing to select".to_string());
    }
    let mut selected = 0;
    let mut previous = None;
    loop {
        render_picker(title, items, selected, &mut previous)?;
        match event::read().map_err(|error| error.to_string())? {
            InputEvent::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => selected = (selected + 1).min(items.len() - 1),
                KeyCode::Enter => return Ok(Some(selected)),
                KeyCode::Esc => return Ok(None),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                _ => {}
            },
            _ => {}
        }
    }
}

/// One line of text; `hidden` input shows a dot per character.
fn text_entry(title: &str, hidden: bool) -> Result<Option<String>, String> {
    let mut text = String::new();
    let mut previous = None;
    loop {
        let (width, height) = terminal::size().map_err(|error| error.to_string())?;
        let next = text_frame(width, height, title, &text, hidden);
        let mut output = io::stdout().lock();
        paint_frame(&mut output, previous.as_ref(), &next).map_err(|error| error.to_string())?;
        output.flush().map_err(|error| error.to_string())?;
        previous = Some(next);
        match event::read().map_err(|error| error.to_string())? {
            InputEvent::Paste(pasted) => text.push_str(pasted.trim_end_matches(['\r', '\n'])),
            InputEvent::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Enter => return Ok(Some(text)),
                KeyCode::Esc => return Ok(None),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    text.push(character);
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn text_frame(width: u16, height: u16, title: &str, text: &str, hidden: bool) -> Frame {
    let width = usize::from(width).max(3);
    let height = usize::from(height).max(1);
    let mut frame = Frame::new(width, height);
    let title = wrap(&single_line(title), width);
    let rows = title.len().min(height - 1);
    for (row, line) in title.iter().take(rows).enumerate() {
        frame.lines[row].push(Color::Cyan, line, width);
    }
    let shown = if hidden {
        "•".repeat(text.chars().count())
    } else {
        single_line(text)
    };
    // Long input keeps its end in view.
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0;
    for character in shown.chars().rev() {
        let columns = columns(character);
        if used + columns > width.saturating_sub(3) {
            break;
        }
        used += columns;
        tail.push(character);
    }
    let tail: String = tail.into_iter().rev().collect();
    frame.lines[rows].push(Color::White, "> ", width);
    frame.lines[rows].push(Color::White, &tail, width);
    frame.cursor = Some(((2 + used) as u16, rows as u16));
    frame
}

/// Setup questions asked before the session UI starts.
pub struct SetupScreen {
    _screen: Screen,
}

impl SetupScreen {
    pub fn enter() -> Result<Self, String> {
        Screen::enter().map(|screen| Self { _screen: screen })
    }
}

impl crate::setup::Ask for SetupScreen {
    fn choose(&mut self, title: &str, items: &[String]) -> Result<Option<usize>, String> {
        pick(title, items)
    }

    fn text(&mut self, title: &str, hidden: bool) -> Result<Option<String>, String> {
        text_entry(title, hidden)
    }
}

/// `/setup` inside a running session; the transcript is redrawn afterwards.
struct AppAsk<'a>(&'a mut App);

impl crate::setup::Ask for AppAsk<'_> {
    fn choose(&mut self, title: &str, items: &[String]) -> Result<Option<usize>, String> {
        picker(self.0, title, items)
    }

    fn text(&mut self, title: &str, hidden: bool) -> Result<Option<String>, String> {
        let text = text_entry(title, hidden);
        self.0.invalidate();
        self.0.render()?;
        text
    }
}

fn render_picker(
    title: &str,
    items: &[String],
    selected: usize,
    previous: &mut Option<Frame>,
) -> Result<(), String> {
    let (width, height) = terminal::size().map_err(|error| error.to_string())?;
    let next = picker_frame(width, height, title, items, selected);
    let mut output = io::stdout().lock();
    paint_frame(&mut output, previous.as_ref(), &next).map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())?;
    *previous = Some(next);
    Ok(())
}

fn picker_frame(width: u16, height: u16, title: &str, items: &[String], selected: usize) -> Frame {
    let width = usize::from(width).max(1);
    let height = usize::from(height);
    let mut frame = Frame::new(width, height);
    if height == 0 {
        return frame;
    }
    // A long title wraps, leaving at least one row for the items.
    let title = wrap(&single_line(title), width);
    let title_rows = title.len().min(height.saturating_sub(1).max(1));
    for (row, text) in title.iter().take(title_rows).enumerate() {
        frame.lines[row].push(Color::Cyan, text, width);
    }
    let visible = height.saturating_sub(title_rows + 1).max(1);
    let start = selected.saturating_sub(visible / 2);
    let end = (start + visible).min(items.len());
    for (index, item) in items[start..end].iter().enumerate() {
        let actual = start + index;
        let color = if actual == selected {
            Color::Yellow
        } else {
            Color::White
        };
        let line = &mut frame.lines[title_rows + index];
        line.push(color, if actual == selected { "> " } else { "  " }, width);
        line.push(color, &single_line(item), width);
    }
    frame
}

fn color(kind: Kind) -> Color {
    match kind {
        Kind::User => Color::Green,
        Kind::Assistant => Color::White,
        Kind::Reasoning => Color::DarkGrey,
        Kind::Tool => Color::Cyan,
        Kind::ToolOutput => Color::DarkCyan,
        Kind::Notice => Color::Yellow,
        Kind::Error => Color::Red,
    }
}

/// The status bar's context element: tokens the active context holds and how
/// much of the model's window that is.
fn context_status(used: u64, max: u64) -> String {
    format!("context: {used} used {}", super::percent(used, max))
}

/// The status bar's scroll element: how many transcript lines the view hides
/// below it.
fn hidden_status(hidden: usize) -> String {
    if hidden == 1 {
        "1 line hidden".to_string()
    } else {
        format!("{hidden} lines hidden")
    }
}

fn clean(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() && !matches!(character, '\n' | '\t') {
                '�'
            } else {
                character
            }
        })
        .collect()
}

fn single_line(text: &str) -> String {
    // A tab would move the cursor to a column stop we do not model.
    clean(text).replace('\n', "↵").replace('\t', " ")
}

/// Soft-wrap `text` to `width` columns, breaking at spaces so words stay
/// whole; a word longer than a row is still broken. Each row is the half-open
/// range of character indices it shows, so a cursor position can be mapped
/// back to a row and column. A break space stays at the end of its row, which
/// keeps the ranges contiguous and the space invisible. A wide glyph that does
/// not fit the rest of its row starts the next one.
fn wrap_rows(text: &str, width: usize) -> Vec<(usize, usize)> {
    let width = width.max(1);
    let characters: Vec<char> = text.chars().collect();
    let mut rows = Vec::new();
    let mut start = 0;
    loop {
        let newline = characters[start..]
            .iter()
            .position(|character| *character == '\n')
            .map(|index| start + index);
        let end = newline.unwrap_or(characters.len());
        let mut cursor = start;
        loop {
            // The longest run of characters that fits the row.
            let mut used = 0;
            let mut window = cursor;
            while window < end {
                let columns = columns(characters[window]);
                if used + columns > width {
                    break;
                }
                used += columns;
                window += 1;
            }
            if window == end {
                rows.push((cursor, end));
                break;
            }
            if window == cursor {
                // A glyph wider than the whole row still takes one, which the
                // row then clips.
                window = cursor + 1;
            }
            let split = characters[cursor..window]
                .iter()
                .rposition(|character| matches!(character, ' ' | '\t'))
                .filter(|index| *index > 0)
                .map_or(window, |index| cursor + index + 1);
            rows.push((cursor, split));
            cursor = split;
            if cursor == end {
                // The last row was the last glyph; there is no tail to wrap.
                break;
            }
        }
        match newline {
            Some(index) => start = index + 1,
            None => break,
        }
    }
    rows
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    wrap_rows(text, width)
        .into_iter()
        .map(|(start, end)| {
            characters[start..end]
                .iter()
                .map(|character| if *character == '\t' { ' ' } else { *character })
                .collect()
        })
        .collect()
}

fn editor_cursor(text: &str, cursor: usize, width: usize) -> (usize, usize) {
    // The editor's cursor is a byte offset; rows count characters.
    let cursor = text
        .get(..cursor)
        .map_or_else(|| text.chars().count(), |prefix| prefix.chars().count());
    let rows = wrap_rows(text, width);
    let row = rows
        .iter()
        .rposition(|(start, _)| *start <= cursor)
        .unwrap_or(0);
    // The column is in cells, so a wide glyph before the cursor counts twice.
    let column = text
        .chars()
        .skip(rows[row].0)
        .take(cursor - rows[row].0)
        .map(columns)
        .sum();
    (row, column)
}

/// The longest prefix of `text` that fits `width` columns.
fn clipped(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for character in text.chars() {
        let columns = columns(character);
        if used + columns > width {
            break;
        }
        used += columns;
        result.push(character);
    }
    result
}

fn suffix(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let start = text.len() - limit;
    let start = (start..text.len())
        .find(|index| text.is_char_boundary(*index))
        .unwrap_or(text.len());
    text[start..].to_string()
}

fn fold(text: &str) -> String {
    let text = text.trim();
    if text.len() <= 200 && !text.contains('\n') {
        text.to_string()
    } else {
        format!("{} bytes", text.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved_runtime(label: &str) -> (Runtime, std::path::PathBuf) {
        use crate::provider::{
            Completion, EventSink, Message, Provider, ProviderError, Request, ToolCall,
        };
        use crate::session::{Compaction, Store};

        struct NoRequests;
        impl Provider for NoRequests {
            fn complete(
                &self,
                _: &Request,
                _: &mut dyn EventSink,
            ) -> Result<Completion, ProviderError> {
                panic!("displaying saved history requested a model response");
            }
        }

        let root = std::env::temp_dir().join(format!("gears-tui-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let store = Store::with_root(&root, root.join("state")).unwrap();
        let mut session = store.create(false, None).unwrap();
        session
            .append_message(&Message::user("saved question\x1b"))
            .unwrap();
        let branch = session
            .append_message(&Message::assistant("saved answer"))
            .unwrap();
        session
            .append_message(&Message::assistant("abandoned answer"))
            .unwrap();
        session.select(&branch).unwrap();
        let mut message = Message::assistant_response(
            "saved follow-up".into(),
            vec![ToolCall::new(
                "c1",
                "sh",
                "{\"command\":\"touch must-not-run\"}",
            )],
        );
        message.reasoning = Some("saved reasoning".into());
        session.append_message(&message).unwrap();
        session
            .append_message(&Message::tool_result("c1", "sh", "saved output", false))
            .unwrap();
        session.append_error("saved failure", true).unwrap();
        session
            .append_compaction(&Compaction {
                summary: "model context summary".into(),
                retained_tail: Vec::new(),
                tokens_before: 10,
            })
            .unwrap();
        session
            .append_message(&Message::assistant("latest answer"))
            .unwrap();
        let id = session.id().to_string();
        drop(session);
        let session = store.open(&id).unwrap();
        let runtime = Runtime::new(
            std::sync::Arc::new(NoRequests),
            store,
            session,
            &crate::config::Config::default(),
            "test/model".into(),
        )
        .unwrap();
        (runtime, root)
    }

    #[test]
    fn resumed_session_displays_saved_active_history_without_running_it() {
        let (runtime, root) = saved_runtime("resume-history");
        let before = std::fs::read(runtime.session().path().unwrap()).unwrap();
        let context = runtime.session().context_messages().unwrap();
        let mut app = App::new(&runtime, true).unwrap();
        let frame = visible(&mut app);
        for text in [
            "saved question",
            "saved answer",
            "saved reasoning",
            "saved follow-up",
            "saved output",
            "saved failure",
            "latest answer",
        ] {
            assert!(frame.contains(text), "missing {text:?}: {frame}");
        }
        assert!(!frame.contains("abandoned answer"));
        assert!(!frame.contains('\x1b'));
        assert!(!root.join("must-not-run").exists());
        let mut one_shot = App::new(&runtime, false).unwrap();
        assert!(!visible(&mut one_shot).contains("saved answer"));
        assert_eq!(runtime.session().context_messages().unwrap(), context);
        assert_eq!(
            std::fs::read(runtime.session().path().unwrap()).unwrap(),
            before
        );
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn visible(app: &mut App) -> String {
        app.frame(100, 40)
            .lines
            .iter()
            .map(|line| line.0.iter().map(|cell| cell.character).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The rows a terminal of `width` x `height` shows, one string per row.
    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        app.frame(width, height)
            .lines
            .iter()
            .map(|line| line.0.iter().map(|cell| cell.character).collect::<String>())
            .collect()
    }

    fn filled(app: &mut App, lines: std::ops::Range<usize>) {
        for index in lines {
            app.push(Kind::Notice, format!("line {index}"), false);
        }
    }

    #[test]
    fn paging_moves_one_screen_less_three_lines() {
        let mut app = app();
        filled(&mut app, 0..100);
        // A 24 row terminal leaves 22 transcript rows above the prompt and the
        // status bar; a page overlaps by three of them.
        let bottom = screen(&mut app, 80, 24);
        assert_eq!(app.page_rows, 19);
        assert_eq!(app.max_scroll, 78);
        assert!(bottom[0].starts_with("note> line 78"), "{}", bottom[0]);
        assert!(bottom[21].starts_with("note> line 99"), "{}", bottom[21]);

        app.page(true);
        let page = screen(&mut app, 80, 24);
        assert_eq!(app.scroll, 19);
        assert!(page[0].starts_with("note> line 59"), "{}", page[0]);
        // The three lines at the top of the first page are the bottom three of
        // the next one.
        assert_eq!(page[19..22], bottom[0..3]);

        // Scrolling back stops at the oldest line instead of overshooting it.
        for _ in 0..10 {
            app.page(true);
        }
        assert_eq!(app.scroll, 78);
        for _ in 0..10 {
            app.page(false);
        }
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn new_output_while_scrolled_back_keeps_the_view() {
        let mut app = app();
        filled(&mut app, 0..100);
        let _ = screen(&mut app, 80, 24);
        app.page(true);
        let page = screen(&mut app, 80, 24);
        assert_eq!(app.scroll, 19);

        // Agent output does not scroll the screen while it is not at the
        // bottom; only the hidden count grows.
        filled(&mut app, 100..110);
        let after = screen(&mut app, 80, 24);
        // The transcript rows are unchanged; only the status bar differs.
        assert_eq!(after[..22], page[..22]);
        assert_eq!(app.scroll, 29);
        assert!(after[23].ends_with("29 lines hidden"), "{}", after[23]);
        // The bar keeps the terminal's last column empty, so the row is one
        // column short of the width.
        assert_eq!(app.frame(80, 24).lines[23].columns(), 79);

        // Back at the bottom the count is gone and the view follows again.
        app.scroll = 0;
        let bottom = screen(&mut app, 80, 24);
        assert!(!bottom[23].contains("hidden"), "{}", bottom[23]);
        assert!(bottom[21].starts_with("note> line 109"), "{}", bottom[21]);
    }

    /// The width of each row in columns, so a test can check that the terminal
    /// is never asked to print past its own width.
    fn row_columns(frame: &Frame) -> Vec<usize> {
        frame.lines.iter().map(StyledLine::columns).collect()
    }

    #[test]
    fn no_row_prints_past_the_terminals_width() {
        // A terminal draws a wide glyph in two cells. A row of `width`
        // characters holding wide glyphs would print past the last column, wrap,
        // and scroll every row of the screen out from under the next frame,
        // which then leaves the rows it believes unchanged stale on screen.
        let mut app = app();
        app.push(
            Kind::Assistant,
            "日本語のテキストは全角文字なので折り返しがずれる。".repeat(6),
            false,
        );
        app.push(
            Kind::Notice,
            "😀 emoji and ⚙\u{fe0f} gear ".repeat(4),
            false,
        );
        app.editor.insert_paste(&"全角の入力".repeat(20));
        for width in [1_u16, 8, 20, 33, 80] {
            for scroll in [0, 3] {
                app.scroll = scroll;
                let frame = app.frame(width, 24);
                let rows = row_columns(&frame);
                let width = usize::from(width);
                assert!(
                    rows[..23].iter().all(|columns| *columns <= width),
                    "{width}: {rows:?}"
                );
                // The status row is the terminal's last one, so it stops a
                // column short: an eager-wrap terminal never scrolls it.
                assert!(rows[23] < width, "{width}: {rows:?}");
            }
        }
    }

    /// The cell a wide glyph's second column holds, so a test can compare the
    /// screen with the frame's one cell per glyph.
    const CONTINUATION: char = '\u{0}';

    /// A terminal as `paint_frame`'s output leaves it: one cell per column, an
    /// eager wrap at the last column, and a scroll when a wrap happens on the
    /// last row. `wide` models a terminal that draws the wide glyphs (CJK and
    /// emoji) in two cells, as xterm and its kin do.
    struct Terminal {
        width: usize,
        height: usize,
        wide: bool,
        cells: Vec<Vec<char>>,
        row: usize,
        column: usize,
        pending_wrap: bool,
        scrolled: bool,
    }

    impl Terminal {
        fn new(width: usize, height: usize, wide: bool) -> Self {
            Self {
                width,
                height,
                wide,
                cells: vec![vec![' '; width]; height],
                row: 0,
                column: 0,
                pending_wrap: false,
                scrolled: false,
            }
        }

        fn rows(&self) -> Vec<String> {
            self.cells
                .iter()
                .map(|row| {
                    row.iter()
                        .filter(|cell| **cell != CONTINUATION)
                        .collect::<String>()
                        .trim_end_matches(' ')
                        .to_string()
                })
                .collect()
        }

        fn feed(&mut self, output: &[u8]) {
            let text = String::from_utf8(output.to_vec()).unwrap();
            let mut characters = text.chars().peekable();
            while let Some(character) = characters.next() {
                if character != '\u{1b}' {
                    self.print(character);
                    continue;
                }
                let Some('[') = characters.next() else {
                    continue;
                };
                let mut parameters = String::new();
                while let Some(next) = characters.peek() {
                    if next.is_ascii_digit() || matches!(next, ';' | '?') {
                        parameters.push(characters.next().unwrap());
                    } else {
                        break;
                    }
                }
                let command = characters.next().unwrap_or(' ');
                match command {
                    'H' => {
                        let numbers: Vec<usize> = parameters
                            .split(';')
                            .map(|number| number.parse::<usize>().unwrap_or(1))
                            .collect();
                        self.row = numbers.first().copied().unwrap_or(1) - 1;
                        self.column = numbers.get(1).copied().unwrap_or(1) - 1;
                        self.pending_wrap = false;
                    }
                    'J' => {
                        for row in &mut self.cells {
                            row.fill(' ');
                        }
                        self.pending_wrap = false;
                    }
                    'K' => {
                        for cell in &mut self.cells[self.row][self.column..] {
                            *cell = ' ';
                        }
                        self.pending_wrap = false;
                    }
                    // SGR, cursor visibility: nothing to draw.
                    _ => {}
                }
            }
        }

        fn print(&mut self, character: char) {
            // The terminal draws a variation selector in no cell at all; a
            // terminal with wide glyphs draws the glyph before it in two.
            let columns = if matches!(character, '\u{fe0e}' | '\u{fe0f}') {
                0
            } else if self.wide {
                columns(character)
            } else {
                1
            };
            if columns == 0 {
                // A variation selector takes no cell; the emoji presentation
                // widens the glyph before it by one.
                if self.wide && !self.pending_wrap && self.column > 0 && self.column < self.width {
                    self.cells[self.row][self.column] = CONTINUATION;
                    self.column += 1;
                    if self.column + 1 >= self.width {
                        self.pending_wrap = true;
                    }
                }
                return;
            }
            if self.pending_wrap || (columns == 2 && self.column + 1 >= self.width) {
                if self.row + 1 == self.height {
                    self.cells.remove(0);
                    self.cells.push(vec![' '; self.width]);
                    self.scrolled = true;
                } else {
                    self.row += 1;
                }
                self.column = 0;
                self.pending_wrap = false;
            }
            self.cells[self.row][self.column] = character;
            if columns == 2 && self.column + 1 < self.width {
                self.cells[self.row][self.column + 1] = CONTINUATION;
            }
            if self.column + columns >= self.width {
                self.column = self.width - 1;
                self.pending_wrap = true;
            } else {
                self.column += columns;
            }
        }
    }

    fn frame_rows(frame: &Frame) -> Vec<String> {
        frame
            .lines
            .iter()
            .map(|line| {
                line.0
                    .iter()
                    .map(|cell| cell.character)
                    .collect::<String>()
                    .trim_end_matches(' ')
                    .to_string()
            })
            .collect()
    }

    /// Paint one frame and check the terminal ends up showing it.
    fn paint_and_check(
        app: &mut App,
        width: u16,
        height: u16,
        terminal: &mut Terminal,
        previous: &mut Option<Frame>,
    ) {
        let next = app.frame(width, height);
        let mut output = Vec::new();
        paint_frame(&mut output, previous.as_ref(), &next).unwrap();
        terminal.feed(&output);
        // A scroll moves every row up out from under the last frame, so the
        // rows that did not change are never repainted and stay stale.
        assert!(
            !terminal.scrolled,
            "the frame scrolled the terminal (row {row} of the status?)",
            row = terminal.height - 1
        );
        assert_eq!(
            terminal.rows(),
            frame_rows(&next),
            "the screen and the frame differ (scrolled: {})",
            terminal.scrolled
        );
        *previous = Some(next);
    }

    #[test]
    fn the_screen_shows_the_frame_while_paging_through_output() {
        for (width, height) in [(80_u16, 24_u16), (40, 12), (21, 8)] {
            for wide in [false, true] {
                let mut app = app();
                filled(&mut app, 0..100);
                app.editor.insert_paste("draft");
                let mut terminal = Terminal::new(width as usize, height as usize, wide);
                let mut previous = None;
                paint_and_check(&mut app, width, height, &mut terminal, &mut previous);

                app.page(true);
                paint_and_check(&mut app, width, height, &mut terminal, &mut previous);
                for index in 0..120 {
                    let text = format!(
                        "answer line {index} with 全角 text, a 😀 emoji and a long tail {}",
                        ".".repeat(index % 40)
                    );
                    app.push(Kind::Assistant, format!("{text}\n"), true);
                    paint_and_check(&mut app, width, height, &mut terminal, &mut previous);
                    if index % 20 == 0 {
                        app.page(true);
                        paint_and_check(&mut app, width, height, &mut terminal, &mut previous);
                    }
                    if index % 30 == 0 {
                        app.page(false);
                        paint_and_check(&mut app, width, height, &mut terminal, &mut previous);
                    }
                }
                assert!(
                    !previous.unwrap().lines[height as usize - 1].0.is_empty(),
                    "the status bar is empty"
                );
            }
        }
    }

    #[test]
    fn streaming_while_scrolled_back_only_grows_the_hidden_count() {
        let mut app = app();
        filled(&mut app, 0..100);
        let _ = screen(&mut app, 80, 24);
        app.page(true);
        let view = screen(&mut app, 80, 24);
        let mut previous = app.scroll;
        assert_eq!(previous, 19);
        for index in 0..200 {
            app.push(
                Kind::Assistant,
                format!("answer line {index}, 全角 text\n"),
                true,
            );
            let page = screen(&mut app, 80, 24);
            // The view stays where it was; only the count moves.
            assert_eq!(page[..22], view[..22], "the view scrolled at {index}");
            assert!(
                app.scroll > previous,
                "the count stalled at {} for line {index}",
                app.scroll
            );
            previous = app.scroll;
            assert!(
                page[23].ends_with(&format!("{} lines hidden", app.scroll)),
                "{}",
                page[23]
            );
        }
    }

    #[test]
    fn the_status_bar_counts_one_hidden_line_in_the_singular() {
        assert_eq!(hidden_status(1), "1 line hidden");
        assert_eq!(hidden_status(2), "2 lines hidden");
        let mut app = app();
        filled(&mut app, 0..100);
        let _ = screen(&mut app, 80, 24);
        app.scroll = 1;
        let rows = screen(&mut app, 80, 24);
        assert!(rows[23].ends_with("1 line hidden"), "{}", rows[23]);
    }

    #[test]
    fn the_status_bar_never_fills_the_terminals_last_column() {
        // An eager-wrap terminal scrolls a line when the last row's last column
        // is written, and the status row is the last one: a full bar would
        // shift the whole screen on every repaint, e.g. on each gear blink.
        let mut app = app();
        filled(&mut app, 0..100);
        for width in [1_u16, 20, 40, 80, 120] {
            for scroll in [0, 1, 19] {
                app.scroll = scroll;
                let frame = app.frame(width, 24);
                assert!(
                    frame.lines[23].0.len() < usize::from(width),
                    "width {width}, scroll {scroll}"
                );
            }
        }
    }

    #[test]
    fn session_and_branch_commands_replace_the_visible_history() {
        let (mut runtime, root) = saved_runtime("switch-history");
        let id = runtime.session().id().to_string();
        let branch = runtime
            .session()
            .entries()
            .iter()
            .find(|entry| entry.data["content"][0]["text"] == "saved answer")
            .unwrap()
            .id
            .clone();
        let mut app = App::new(&runtime, true).unwrap();
        app.scroll = 100;
        handle_command(
            &mut runtime,
            &mut app,
            "/new",
            &crate::config::Config::default(),
        )
        .unwrap();
        assert!(!visible(&mut app).contains("saved answer"));
        handle_command(
            &mut runtime,
            &mut app,
            &format!("/resume {id}"),
            &crate::config::Config::default(),
        )
        .unwrap();
        assert!(visible(&mut app).contains("latest answer"));
        assert_eq!(app.scroll, 0);
        handle_command(
            &mut runtime,
            &mut app,
            &format!("/tree {branch}"),
            &crate::config::Config::default(),
        )
        .unwrap();
        assert!(visible(&mut app).contains("saved answer"));
        assert!(!visible(&mut app).contains("latest answer"));
        assert!(!visible(&mut app).contains("abandoned answer"));
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn context_command_reports_and_changes_the_window() {
        let (mut runtime, root) = saved_runtime("context-command");
        let config = crate::config::Config::default();
        let directory = root.join("state");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("gears.toml"),
            "version = 1\nsystem_prompt_linux = \"SYSTEM.md\"\nsystem_prompt_motor_os = \"SYSTEM.md\"\n",
        )
        .unwrap();
        let mut app = App::new(&runtime, true).unwrap();
        handle_command(&mut runtime, &mut app, "/context", &config).unwrap();
        let frame = visible(&mut app);
        assert!(frame.contains("window: 128000 tokens"), "{frame}");
        assert!(frame.contains("output reserve: 16384 tokens"), "{frame}");
        assert!(frame.contains("recent tail: 20000 tokens"), "{frame}");

        handle_command(&mut runtime, &mut app, "/context set 200,000", &config).unwrap();
        assert_eq!(runtime.context().window_tokens, 200_000);
        let frame = visible(&mut app);
        assert!(
            frame.contains(
                "context window set to 200000 tokens and saved as the default in gears.toml"
            ),
            "{frame}"
        );
        assert!(frame.contains("window: 200000 tokens"), "{frame}");
        let saved = crate::config::Config::parse(
            &std::fs::read_to_string(directory.join("gears.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.context.window_tokens, 200_000);

        // A window that leaves no room for the reserve and tail is refused.
        let error =
            handle_command(&mut runtime, &mut app, "/context set 4096", &config).unwrap_err();
        assert!(
            error.contains("leave room for the 16384 token output reserve"),
            "{error}"
        );
        assert_eq!(runtime.context().window_tokens, 200_000);
        let saved = crate::config::Config::parse(
            &std::fs::read_to_string(directory.join("gears.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.context.window_tokens, 200_000);
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_saved_reply_keeps_its_visible_end() {
        let mut app = app();
        app.push(
            Kind::Assistant,
            format!("{}latest end", "α".repeat(TRANSCRIPT_BYTES)),
            false,
        );
        assert!(visible(&mut app).contains("latest end"));
        assert!(app.bytes <= TRANSCRIPT_BYTES);
    }

    fn app() -> App {
        App {
            transcript: VecDeque::new(),
            bytes: 0,
            editor: Editor::new(),
            scroll: 0,
            shown: None,
            max_scroll: 0,
            page_rows: 1,
            status: "starting turn".to_string(),
            model: "test/model".to_string(),
            context_used: 0,
            context_max: 1,
            session: SessionSummary {
                id: "test-session".to_string(),
                path: None,
                name: None,
                entries: 0,
                ephemeral: true,
            },
            role: ProcessRole::Interactive,
            input_enabled: true,
            busy_since: Instant::now(),
            quit: false,
            last_frame: None,
        }
    }

    #[test]
    fn entry_continuations_indent_to_the_prefix_width() {
        let mut app = app();
        app.push(Kind::Notice, "one\ntwo\nthree".to_string(), false);
        app.push(Kind::User, "first\nsecond".to_string(), false);
        assert_eq!(
            plain(&app.transcript_lines(80)),
            [
                (Kind::Notice, "note> one".to_string()),
                (Kind::Notice, "      two".to_string()),
                (Kind::Notice, "      three".to_string()),
                (Kind::User, "you> first".to_string()),
                (Kind::User, "     second".to_string()),
            ]
        );
    }

    fn plain(lines: &[TranscriptLine]) -> Vec<(Kind, String)> {
        lines
            .iter()
            .map(|line| {
                (
                    line.kind,
                    line.spans.iter().map(|(_, text)| text.as_str()).collect(),
                )
            })
            .collect()
    }

    fn styled(color: Color) -> markdown::Style {
        markdown::Style::plain(color)
    }

    fn bold(color: Color) -> markdown::Style {
        markdown::Style::bold(color)
    }

    #[test]
    fn agent_markdown_is_colored() {
        let mut app = app();
        app.push(
            Kind::Assistant,
            "# Title\nrun `lorry build`\n```\ncode\n```".to_string(),
            false,
        );
        let lines = app.transcript_lines(80);
        let spans = |index: usize| lines[index].spans.clone();
        assert_eq!(
            spans(0),
            [
                (styled(Color::White), "agent> ".to_string()),
                (styled(markdown::HEADING), "# Title".to_string()),
            ]
        );
        assert_eq!(
            spans(1),
            [
                (styled(Color::White), "       run ".to_string()),
                (styled(markdown::INLINE_CODE), "`lorry build`".to_string()),
            ]
        );
        assert_eq!(
            spans(2),
            [
                (styled(Color::White), "       ".to_string()),
                (styled(markdown::CODE_BLOCK), "```".to_string()),
            ]
        );
        assert_eq!(
            spans(3),
            [
                (styled(Color::White), "       ".to_string()),
                (styled(markdown::CODE_BLOCK), "code".to_string()),
            ]
        );
    }

    #[test]
    fn agent_bold_reaches_the_frame_as_an_attribute() {
        let mut app = app();
        app.push(Kind::Assistant, "a **b** c".to_string(), false);
        let lines = app.transcript_lines(80);
        assert_eq!(
            lines[0].spans,
            [
                (styled(Color::White), "agent> a ".to_string()),
                (bold(Color::White), "**b**".to_string()),
                (styled(Color::White), " c".to_string()),
            ]
        );
        // The markers stay in what the terminal is asked to print.
        assert!(visible(&mut app).contains("**b**"));

        let frame = app.frame(80, 24);
        // The transcript is anchored to the bottom, so find its row.
        let row = frame
            .lines
            .iter()
            .find(|line| line.0.iter().any(|cell| cell.character == 'b'))
            .expect("the transcript row is on screen");
        let cell = |character| {
            row.0
                .iter()
                .find(|cell| cell.character == character)
                .unwrap()
        };
        assert!(cell('b').style.bold);
        assert!(!cell('a').style.bold);
        assert!(!cell('c').style.bold);

        let mut output = Vec::new();
        paint_line(&mut output, 0, None, row).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\x1b[1m"), "{output:?}");
        // Bold ends before the trailing " c".
        assert!(output.contains("\x1b[22m"), "{output:?}");
    }

    #[test]
    fn other_entries_keep_their_plain_color() {
        let mut app = app();
        app.push(Kind::User, "# not a heading `x` **y**".to_string(), false);
        assert_eq!(
            app.transcript_lines(80)[0].spans,
            [(
                styled(Color::Green),
                "you> # not a heading `x` **y**".to_string()
            )]
        );
    }

    #[test]
    fn enter_after_a_backslash_inserts_a_newline_instead_of_sending() {
        let mut app = app();
        app.editor.insert_paste("one\\");
        assert!(app.edit(InputEvent::Key(KeyCode::Enter.into())).is_none());
        app.editor.insert_paste("two");
        assert_eq!(app.editor.text(), "one\ntwo");
        assert!(matches!(
            app.edit(InputEvent::Key(KeyCode::Enter.into())),
            Some(Action::Submit(text)) if text == "one\ntwo"
        ));
    }

    #[test]
    fn only_a_bare_enter_submits_the_prompt() {
        let newlines = [
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('\n'), KeyModifiers::NONE),
        ]
        .into_iter()
        .chain(
            KeyModifiers::all()
                .iter()
                .map(|modifier| KeyEvent::new(KeyCode::Enter, modifier)),
        )
        .chain([KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT | KeyModifiers::ALT,
        )]);
        for key in newlines {
            let mut app = app();
            app.editor.insert_paste("one");
            assert!(app.edit(InputEvent::Key(key)).is_none(), "{key:?}");
            app.editor.insert_paste("two");
            assert_eq!(app.editor.text(), "one\ntwo", "{key:?}");
            assert!(matches!(
                app.edit(InputEvent::Key(KeyCode::Enter.into())),
                Some(Action::Submit(text)) if text == "one\ntwo"
            ));
        }
    }

    #[test]
    fn hidden_text_shows_dots_and_long_text_keeps_its_end_in_view() {
        let text = |frame: Frame| {
            frame
                .lines
                .iter()
                .map(|line| {
                    line.0
                        .iter()
                        .map(|styled| styled.character)
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };
        let hidden = text_frame(12, 3, "API key", "sk-1", true);
        assert_eq!(hidden.cursor, Some((6, 1)));
        assert_eq!(text(hidden), ["API key", "> ••••", ""]);
        let long = text_frame(8, 2, "Model id", "abcdefghij", false);
        assert_eq!(text(long), ["Model id", "> fghij"]);
    }

    #[test]
    fn a_long_picker_title_wraps_above_the_choices() {
        let text = |frame: Frame| {
            frame
                .lines
                .iter()
                .map(|line| {
                    line.0
                        .iter()
                        .map(|styled| styled.character)
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };
        let frame = picker_frame(
            10,
            6,
            "Reset Gears: remove",
            &["No".into(), "Yes".into()],
            0,
        );
        assert_eq!(
            text(frame),
            ["Reset ", "Gears: ", "remove", "> No", "  Yes", ""]
        );
        // A short screen keeps the selected choice in view.
        let small = picker_frame(
            10,
            5,
            "Reset Gears: remove",
            &["No".into(), "Yes".into()],
            1,
        );
        assert_eq!(text(small), ["Reset ", "Gears: ", "remove", "> Yes", ""]);
    }

    #[test]
    fn wrapping_and_cursor_agree() {
        assert_eq!(wrap("abcdef", 3), ["abc", "def"]);
        assert_eq!(editor_cursor("abcdef", 6, 3), (1, 3));
        assert_eq!(editor_cursor("ab\ncd", 5, 3), (1, 2));
    }

    #[test]
    fn wrapping_counts_a_wide_glyph_as_two_columns() {
        assert_eq!(wrap("日本語", 3), ["日", "本", "語"]);
        assert_eq!(wrap("abc日本", 6), ["abc日", "本"]);
        // The cursor's column is in cells too.
        assert_eq!(editor_cursor("日本", 6, 10), (0, 4));
        assert_eq!(editor_cursor("a日本", 7, 10), (0, 5));
    }

    #[test]
    fn repainting_moves_to_the_column_after_a_wide_prefix() {
        let mut old = StyledLine::default();
        old.push(Color::White, "ab old", 20);
        let mut new = StyledLine::default();
        new.push(Color::White, "ab new", 20);
        let mut output = Vec::new();
        paint_line(&mut output, 0, Some(&old), &new).unwrap();
        let output = String::from_utf8(output).unwrap();
        // Two columns that did not change: the third is where the words part.
        let mut moved = Vec::new();
        queue!(moved, cursor::MoveTo(3, 0)).unwrap();
        let moved = String::from_utf8(moved).unwrap();
        assert!(output.starts_with(&moved), "{output:?}");

        // A wide glyph ends the untouched prefix: a terminal that draws it in
        // another number of cells than counted here would take the rest of the
        // row with it, so the row is painted again from before it.
        let mut old = StyledLine::default();
        old.push(Color::White, "日本 old", 20);
        let mut new = StyledLine::default();
        new.push(Color::White, "日本 new", 20);
        let mut output = Vec::new();
        paint_line(&mut output, 0, Some(&old), &new).unwrap();
        let output = String::from_utf8(output).unwrap();
        let mut moved = Vec::new();
        queue!(moved, cursor::MoveTo(0, 0)).unwrap();
        let moved = String::from_utf8(moved).unwrap();
        assert!(output.starts_with(&moved), "{output:?}");

        // A row that shrank in columns after a wide glyph is cleared, even
        // though it kept the same number of cells.
        let mut short = StyledLine::default();
        short.push(Color::White, "日本", 20);
        let mut cleared = Vec::new();
        paint_line(&mut cleared, 0, Some(&new), &short).unwrap();
        let cleared = String::from_utf8(cleared).unwrap();
        assert!(cleared.contains("\u{1b}[K"), "{cleared:?}");
    }

    #[test]
    fn wrapping_keeps_words_whole() {
        assert_eq!(wrap("hello world", 8), ["hello ", "world"]);
        assert_eq!(wrap("a bb ccc dddd", 6), ["a bb ", "ccc ", "dddd"]);
        // A word longer than the row is still broken.
        assert_eq!(wrap("abcdefgh ij", 4), ["abcd", "efgh", " ij"]);
        assert_eq!(editor_cursor("hello world", 8, 8), (1, 2));
        assert_eq!(editor_cursor("hello world", 6, 8), (1, 0));
    }

    #[test]
    fn controls_are_not_rendered() {
        assert_eq!(clean("a\x1bb"), "a�b");
        assert_eq!(single_line("a\nb\x1bc"), "a↵b�c");
    }

    #[test]
    fn transcript_editor_and_status_are_anchored_from_the_bottom() {
        assert_eq!(
            screen_layout(24, 1),
            Layout {
                transcript_rows: 22,
                editor_start: 22,
                editor_rows: 1,
                status_row: 23,
            }
        );
        assert_eq!(
            screen_layout(24, 10),
            Layout {
                transcript_rows: 13,
                editor_start: 13,
                editor_rows: 10,
                status_row: 23,
            }
        );
    }

    #[test]
    fn incremental_paint_writes_only_the_changed_suffix() {
        let mut old = Frame::new(10, 3);
        old.lines[1].push(Color::Green, "> ", 10);
        old.lines[1].push(Color::White, "a", 10);
        old.cursor = Some((3, 1));
        let mut new = old.clone();
        new.lines[1].push(Color::White, "b", 10);
        new.cursor = Some((4, 1));
        let mut output = Vec::new();

        paint_frame(&mut output, Some(&old), &new).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("[2J"), "{output:?}");
        assert!(output.contains("[2;4H"), "{output:?}");
        assert!(!output.contains("> a"), "{output:?}");
        assert!(output.contains('b'), "{output:?}");
    }

    #[test]
    fn the_status_line_reports_what_the_turn_is_doing() {
        let mut app = app();
        app.begin_operation();
        let status = |app: &mut App| {
            app.frame(120, 24).lines[23]
                .0
                .iter()
                .map(|cell| cell.character)
                .collect::<String>()
        };
        let phase = |app: &mut App, phase: &str| {
            let status = status(app);
            assert!(status.starts_with(" \u{26ed} context: "), "{status}");
            assert!(status.ends_with(&format!(" | {phase}")), "{status}");
        };

        phase(&mut app, "starting turn");
        app.apply(Event::Waiting(Waiting::Model));
        phase(&mut app, "waiting for model");
        app.apply(Event::Reasoning("plan".to_string()));
        phase(&mut app, "thinking");
        app.apply(Event::Text("answer".to_string()));
        phase(&mut app, "responding");
        app.apply(Event::ToolStart {
            call_id: "call_1".to_string(),
            name: "sh".to_string(),
            detail: "uname".to_string(),
        });
        phase(&mut app, "running sh");
        app.apply(Event::ToolEnd {
            call_id: "call_1".to_string(),
            name: "sh".to_string(),
            content: "Motor OS".to_string(),
            is_error: false,
            outcome: crate::provider::ToolOutcome::Completed,
        });
        // The tool's own outcome is in the transcript; the status line moves on
        // to the wait the runtime reports next.
        app.apply(Event::Waiting(Waiting::ToolOutput));
        phase(&mut app, "processing tool output");
        app.apply(Event::Waiting(Waiting::Compaction));
        phase(&mut app, "compacting");
        app.apply(Event::TurnEnd {
            status: "completed",
            tool_rounds: 1,
            last_entry_id: None,
        });
        app.input_enabled = true;
        // Idle paints the gear black in place, so the status text stays put.
        let status = status(&mut app);
        assert!(status.starts_with(" \u{26ed} context: "), "{status}");
        assert!(status.ends_with(" | idle"), "{status}");
    }

    #[test]
    fn usage_totals_do_not_replace_the_reported_phase() {
        let mut app = app();
        let mut total = crate::provider::UsageMeter::new();
        total.add(&crate::provider::Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            ..crate::provider::Usage::default()
        });
        app.apply(Event::Waiting(Waiting::ToolOutput));
        app.apply(Event::Usage {
            request: crate::provider::Usage::default(),
            total,
        });
        assert_eq!(app.status, "processing tool output");
    }

    #[test]
    fn the_status_bar_reports_context_use_before_the_model() {
        let mut app = app();
        app.begin_operation();
        app.context_used = 32_000;
        app.context_max = 128_000;
        let status = app.frame(120, 24).lines[23]
            .0
            .iter()
            .map(|cell| cell.character)
            .collect::<String>();
        assert_eq!(
            status,
            " \u{26ed} context: 32000 used 25% | test/model | test-session (ephemeral) | starting turn"
        );
        assert_eq!(context_status(0, 0), "context: 0 used 0%");
        assert_eq!(context_status(1, 3), "context: 1 used 33%");
    }

    #[test]
    fn context_usage_events_update_the_status_bar_during_a_turn() {
        let mut app = app();
        let status = |app: &mut App| {
            app.frame(80, 24).lines[23]
                .0
                .iter()
                .map(|cell| cell.character)
                .collect::<String>()
        };

        app.apply(Event::ContextUsage {
            used: 1_000,
            max: 4_000,
        });
        let first = status(&mut app);
        assert!(first.contains("context: 1000 used 25% |"), "{first}");
        app.apply(Event::ContextUsage {
            used: 2_000,
            max: 4_000,
        });
        let second = status(&mut app);
        assert!(second.contains("context: 2000 used 50% |"), "{second}");
    }

    #[test]
    fn status_bar_shows_the_gear_only_while_working() {
        let mut app = app();
        app.status = "idle".into();
        let cut = STATUS_TITLE
            .chars()
            .filter(|character| !matches!(character, '\u{fe0e}' | '\u{fe0f}'))
            .count();
        // The black title: the gear keeps its cells, so the status text keeps
        // its columns, whatever the terminal makes of the glyph.
        fn assert_hidden(cells: &[StyledChar]) {
            assert_eq!(cells[0].character, ' ', "{cells:?}");
            assert_eq!(cells[1].character, '\u{26ed}', "{cells:?}");
            assert_eq!(cells[1].mark, Some('\u{fe0f}'), "{cells:?}");
            assert_eq!(cells[2].character, ' ', "{cells:?}");
            assert!(
                cells.iter().all(|cell| cell.style.color == GEAR_HIDDEN),
                "{cells:?}"
            );
        }
        fn title(frame: &Frame, cells: usize) -> &[StyledChar] {
            &frame.lines[23].0[..cells]
        }

        // Idle: the gear's cells are black and the rest of the bar is chrome.
        for role in [
            ProcessRole::System,
            ProcessRole::Interactive,
            ProcessRole::None,
        ] {
            app.role = role;
            let frame = app.frame(80, 24);
            assert_hidden(title(&frame, cut));
            let rest = &frame.lines[23].0[cut..];
            assert!(!rest.is_empty());
            assert!(rest.iter().all(|cell| cell.style.color == STATUS_COLOR));
        }
        assert_eq!(STATUS_COLOR, Color::AnsiValue(222));
        assert!(
            app.frame(80, 24).lines[22].0[..2]
                .iter()
                .all(|cell| cell.style.color == STATUS_COLOR)
        );

        app.begin_operation();
        assert!(!app.input_enabled);
        assert_eq!(app.status, "starting turn");
        // Working: the gear appears in the role's color during the visible
        // half-second and is painted black in place during the off one, so the
        // status text stays where it is.
        for (role, expected) in [
            (ProcessRole::System, Color::AnsiValue(196)),
            (ProcessRole::Interactive, Color::AnsiValue(214)),
            (ProcessRole::None, Color::AnsiValue(120)),
        ] {
            app.role = role;
            app.busy_since = Instant::now();
            let shown = app.frame(80, 24);
            let shown_title = title(&shown, cut);
            assert_eq!(shown_title[1].character, '\u{26ed}', "{role:?}");
            assert_eq!(shown_title[1].mark, Some('\u{fe0f}'), "{role:?}");
            assert!(
                shown_title.iter().all(|cell| cell.style.color == expected),
                "{role:?}"
            );

            app.busy_since = Instant::now() - BLINK_HALF;
            let off = app.frame(80, 24);
            assert_hidden(title(&off, cut));
            assert_eq!(shown.lines[23].0[cut..], off.lines[23].0[cut..], "{role:?}");
        }
        assert!(
            app.frame(80, 24).lines[22].0[..2]
                .iter()
                .all(|cell| cell.style.color == Color::DarkGrey)
        );
    }

    #[test]
    fn a_variation_selector_stays_with_its_cell() {
        let mut line = StyledLine::default();
        // The selector's emoji presentation makes the gear two columns wide.
        line.push(Color::White, "\u{26ed}\u{fe0f} x", 4);
        assert_eq!(line.0.len(), 3);
        assert_eq!(line.columns(), 4);
        assert_eq!(line.0[0].character, '\u{26ed}');
        assert_eq!(line.0[0].mark, Some('\u{fe0f}'));

        let mut output = Vec::new();
        paint_line(&mut output, 0, None, &line).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\u{26ed}\u{fe0f} x"), "{output:?}");
    }

    #[test]
    fn the_gear_blinks_at_1hz() {
        let at = |millis: u64| gear_visible(Duration::from_millis(millis));
        assert!(at(0));
        assert!(at(499));
        assert!(!at(500));
        assert!(!at(999));
        assert!(at(1000));
        assert!(at(1499));
        assert!(!at(1500));
    }

    #[test]
    fn exit_replaces_only_the_status_row_and_leaves_a_shell_gap() {
        let mut output = Vec::new();
        paint_exit(&mut output, 80, 24, "session-42", false).unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(!output.contains("[2J"), "{output:?}");
        assert!(
            output.contains("Motor OS Gears session session-42 exited."),
            "{output:?}"
        );
        assert!(output.contains("\r\n\r\n"), "{output:?}");
        assert!(!output.contains("?1049"), "{output:?}");
    }

    /// A frame used to style and wrap every entry, which is what starved the
    /// keyboard while a big reply streamed in: each event cost a whole
    /// transcript's worth of wrapping, so a burst of streamed text kept the
    /// event loop away from the keyboard for seconds.
    #[test]
    fn a_frame_wraps_only_what_changed() {
        let mut app = app();
        // Two source lines each, so every entry has a settled line to count.
        for index in 0..100 {
            app.push(Kind::Notice, format!("line {index}\nmore"), false);
        }
        let _ = screen(&mut app, 80, 24);

        reset_wraps();
        let _ = screen(&mut app, 80, 24);
        assert_eq!(wraps(), 0, "an unchanged frame wrapped lines again");

        app.page(true);
        let _ = screen(&mut app, 80, 24);
        assert_eq!(wraps(), 0, "scrolling wrapped lines again");

        // A width change is the one thing that rewraps every entry, once.
        let _ = screen(&mut app, 60, 24);
        assert_eq!(wraps(), 100);

        // A streaming entry wraps only the source line it settled, not the ones
        // before it, and not the line that is still growing.
        let mut streaming = self::app();
        streaming.push(Kind::Assistant, "one\ntwo".to_string(), true);
        let _ = screen(&mut streaming, 80, 24);
        reset_wraps();
        streaming.push(Kind::Assistant, "\nthree".to_string(), true);
        let lines = plain(&streaming.transcript_lines(80));
        assert_eq!(wraps(), 1, "streaming wrapped a settled line again");
        assert_eq!(
            lines,
            [
                (Kind::Assistant, "agent> one".to_string()),
                (Kind::Assistant, "       two".to_string()),
                (Kind::Assistant, "       three".to_string()),
            ]
        );
    }

    fn wraps() -> usize {
        SETTLED_WRAPS.with(std::cell::Cell::get)
    }

    fn reset_wraps() {
        SETTLED_WRAPS.with(|count| count.set(0));
    }
}
