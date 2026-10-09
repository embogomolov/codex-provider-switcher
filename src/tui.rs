use crate::{
    app::{self, Group, Settings},
    engine::Result,
    migrate,
};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    collections::HashSet,
    io::{self, Write},
    sync::mpsc,
    time::Duration,
};
use unicode_width::UnicodeWidthChar;

struct Screen {
    frame: Vec<Line>,
    dim: Vec<Option<usize>>,
    providers: Vec<Option<bool>>,
    size: (u16, u16),
    tab: Option<View>,
}
impl Screen {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        if let Err(e) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            Hide,
            EnableBracketedPaste,
            EnableMouseCapture
        ) {
            let _ = terminal::disable_raw_mode();
            return Err(e.into());
        }
        Ok(Self {
            frame: vec![],
            dim: vec![],
            providers: vec![],
            size: (0, 0),
            tab: None,
        })
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            Show,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}
fn safe(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
fn clip(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut used = 0;
    let mut out = String::new();
    for c in safe(s).chars() {
        let n = c.width().unwrap_or(0);
        if used + n > width {
            while used + 1 > width {
                if let Some(c) = out.pop() {
                    used -= c.width().unwrap_or(0);
                } else {
                    break;
                }
            }
            out.push('…');
            break;
        }
        out.push(c);
        used += n;
    }
    out
}
fn input_line(label: &str, value: &str, width: usize) -> String {
    let room = width.saturating_sub(label.chars().map(|c| c.width().unwrap_or(0)).sum());
    let value = safe(value);
    if value.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>() <= room {
        return clip(&format!("{label}{value}"), width);
    }
    let mut used = 0;
    let tail: String = value
        .chars()
        .rev()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used < room
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    clip(&format!("{label}…{tail}"), width)
}
fn wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = vec![];
    let mut current = String::new();
    let mut used = 0;
    for c in safe(s).chars() {
        let n = c.width().unwrap_or(0);
        if used + n > width && !current.is_empty() {
            if let Some(space) = current.rfind(' ').filter(|&i| i > 0) {
                let tail = current[space + 1..].to_owned();
                lines.push(current[..space].to_owned());
                current = tail;
                used = unicode_width::UnicodeWidthStr::width(current.as_str());
            } else {
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
        }
        if current.is_empty() && c == ' ' {
            continue;
        }
        current.push(c);
        used += n;
    }
    lines.push(current);
    lines
}
type Line = (String, Color);
fn paint_provider(output: &mut Vec<u8>, text: &str, current: bool) -> Result<()> {
    let split = text.find(' ').map_or(text.len(), |i| i + 1);
    queue!(
        output,
        SetAttribute(Attribute::Reset),
        ResetColor,
        Print(&text[..split]),
        SetForegroundColor(if current { Color::Cyan } else { Color::Reset }),
        Print(&text[split..]),
        ResetColor
    )?;
    Ok(())
}
impl Screen {
    fn draw(&mut self, lines: &[Line], footer: &str) -> Result<()> {
        self.draw_styled(lines, footer, &[], &[], None)
    }
    fn draw_styled(
        &mut self,
        lines: &[Line],
        footer: &str,
        muted: &[(usize, usize)],
        provider_rows: &[(usize, bool)],
        tab: Option<View>,
    ) -> Result<()> {
        let (w, h) = terminal::size()?;
        let footer_lines = wrap(footer, w.saturating_sub(2) as usize);
        let footer_height = (footer_lines.len() as u16).min(h);
        let mut frame = vec![line(""); h as usize];
        let mut dim = vec![None; h as usize];
        let mut providers = vec![None; h as usize];
        for (i, (s, c)) in lines
            .iter()
            .take(h.saturating_sub(footer_height + 1) as usize)
            .enumerate()
        {
            frame[i] = (clip(s, w.saturating_sub(2) as usize), *c);
            dim[i] = muted
                .iter()
                .find(|(row, _)| *row == i)
                .map(|(_, byte)| *byte);
            providers[i] = provider_rows
                .iter()
                .find(|(row, _)| *row == i)
                .map(|(_, current)| *current);
        }
        for (i, s) in footer_lines
            .into_iter()
            .take(footer_height as usize)
            .enumerate()
        {
            frame[h as usize - footer_height as usize + i] =
                (clip(&s, w.saturating_sub(2) as usize), Color::DarkGrey);
        }
        let resized = self.size != (w, h);
        let mut output = Vec::new();
        if resized {
            queue!(output, Clear(ClearType::All))?;
        }
        for (i, (text, color)) in frame.iter().enumerate() {
            if resized
                || (i == 0 && self.tab != tab)
                || self.dim.get(i) != dim.get(i)
                || self.providers.get(i) != providers.get(i)
                || self
                    .frame
                    .get(i)
                    .is_none_or(|old| old.0 != *text || old.1 != *color)
            {
                queue!(output, MoveTo(0, i as u16), Clear(ClearType::CurrentLine))?;
                if !text.is_empty() && w > 1 {
                    if i == 0
                        && let Some(active) = tab
                    {
                        queue!(output, MoveTo(1, 0), SetAttribute(Attribute::Reset))?;
                        let mut room = w.saturating_sub(2) as usize;
                        for (name, view) in [
                            ("Conversations", View::Conversations),
                            ("   Providers", View::Providers),
                        ] {
                            queue!(
                                output,
                                SetForegroundColor(if active == view {
                                    Color::Cyan
                                } else {
                                    Color::DarkGrey
                                }),
                                Print(clip(name, room))
                            )?;
                            room = room.saturating_sub(name.len());
                        }
                        queue!(output, ResetColor)?;
                        continue;
                    }
                    if let Some(current) = providers[i] {
                        queue!(output, MoveTo(1, i as u16))?;
                        paint_provider(&mut output, text, current)?;
                        continue;
                    }
                    queue!(
                        output,
                        MoveTo(1, i as u16),
                        SetAttribute(Attribute::Reset),
                        ResetColor,
                        SetForegroundColor(
                            if matches!(
                                *color,
                                Color::Cyan | Color::DarkGrey | Color::Yellow | Color::Green
                            ) {
                                Color::Reset
                            } else {
                                *color
                            }
                        ),
                        SetAttribute(if text.starts_with("› ") {
                            Attribute::Reverse
                        } else if matches!(*color, Color::Cyan | Color::Yellow) {
                            Attribute::Bold
                        } else {
                            Attribute::Reset
                        }),
                    )?;
                    if let Some(byte) = dim[i].filter(|b| text.is_char_boundary(*b)) {
                        queue!(output, Print(&text[..byte]))?;
                        if !text.starts_with("› ") {
                            queue!(output, SetAttribute(Attribute::Dim))?;
                        }
                        queue!(output, Print(&text[byte..]))?;
                    } else {
                        queue!(output, Print(text))?;
                    }
                    queue!(output, SetAttribute(Attribute::Reset))?;
                }
            }
        }
        if !output.is_empty() {
            queue!(output, ResetColor)?;
            let mut out = io::stdout().lock();
            out.write_all(&output)?;
            out.flush()?;
        }
        self.size = (w, h);
        self.tab = tab;
        self.frame = frame;
        self.dim = dim;
        self.providers = providers;
        Ok(())
    }
}

fn line(s: impl Into<String>) -> Line {
    (s.into(), Color::Reset)
}
fn confirm_recovery(settings: &Settings, screen: &mut Screen) -> Result<bool> {
    if !settings.home.join(app::JOURNAL).exists() {
        return Ok(true);
    }
    loop {
        screen.draw(
            &[
                line("CODEX / Provider switcher"),
                line(""),
                line("An interrupted transaction needs recovery or cleanup."),
                line("Close Codex before continuing. Recover now? [y/n]"),
            ],
            "Y recover   N quit",
        )?;
        if let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
        {
            match k.code {
                KeyCode::Char('y') => {
                    let _lock = app::lock(&settings.home)?;
                    migrate::recover(settings)?;
                    return Ok(true);
                }
                KeyCode::Char('n') => return Ok(false),
                _ => {}
            }
        }
    }
}
fn normalize_search(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .map(|c| if c == 'ё' { 'е' } else { c })
        .collect()
}
fn search_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(normalize_search)
        .filter(|s| !s.is_empty())
        .collect()
}
struct SearchEntry {
    text: String,
    labels: Vec<String>,
    row: usize,
    providers: std::collections::BTreeMap<String, usize>,
}
impl SearchEntry {
    fn provider(&self) -> &str {
        if self.providers.len() == 1 {
            self.providers.keys().next().unwrap()
        } else {
            "Mixed"
        }
    }
    fn select_label(&mut self, g: &Group, terms: &[String]) {
        if terms.is_empty() {
            self.row = g.rows.iter().position(|r| r.id == g.root).unwrap_or(0);
            return;
        }
        let compact = terms.concat();
        self.row = g
            .rows
            .iter()
            .enumerate()
            .min_by_key(|(i, r)| {
                let label = &self.labels[*i];
                (
                    !compact.is_empty() && label != &compact,
                    !terms.iter().all(|term| label.contains(term)),
                    r.subagent,
                    r.archived != 0,
                    std::cmp::Reverse(r.updated),
                )
            })
            .map(|(i, _)| i)
            .unwrap_or(0);
    }
}
fn search_index(groups: &[Group]) -> Vec<SearchEntry> {
    groups
        .iter()
        .map(|g| {
            let mut providers = std::collections::BTreeMap::new();
            for row in &g.rows {
                *providers
                    .entry(
                        row.model_provider
                            .clone()
                            .unwrap_or_else(|| "openai".into()),
                    )
                    .or_default() += 1;
            }
            let text = g
                .rows
                .iter()
                .flat_map(|r| [&r.label, &r.cwd, &r.id])
                .map(|s| normalize_search(s))
                .collect::<Vec<_>>()
                .join("\0");
            SearchEntry {
                text,
                labels: g.rows.iter().map(|r| normalize_search(&r.label)).collect(),
                row: g.rows.iter().position(|r| r.id == g.root).unwrap_or(0),
                providers,
            }
        })
        .collect()
}
fn provider_matches<'a>(providers: &'a [String], query: &str) -> Vec<&'a String> {
    let query = query.trim().to_lowercase();
    let mut matches: Vec<_> = providers
        .iter()
        .filter(|p| p.to_lowercase().contains(&query))
        .collect();
    matches.sort_by_key(|p| p.to_lowercase() != query);
    matches
}
fn toggle_selection(selected: &mut HashSet<String>, all: impl Iterator<Item = String>) {
    if selected.is_empty() {
        selected.extend(all);
    } else {
        selected.clear();
    }
}

fn scroll_start(start: &mut usize, cursor: usize, rows: usize) -> usize {
    let rows = rows.max(1);
    if cursor < *start {
        *start = cursor;
    } else if cursor >= start.saturating_add(rows) {
        *start = cursor + 1 - rows;
    }
    *start
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Conversations,
    Providers,
    Pick,
    Add,
    Busy,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Conversations,
    Providers,
    Search,
    List,
    Archive,
    SelectAll,
    Change,
    Open,
    Refresh,
    Add,
    Default,
    Choose,
    Field(usize),
    Auth,
    Save,
    Cancel,
    Apply,
    Back,
    Row(usize),
    Check(usize),
    Quit,
}
#[derive(Default)]
struct Edit {
    text: String,
    cursor: usize,
}
impl Edit {
    fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }
    fn insert(&mut self, text: &str) {
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }
    fn key(&mut self, key: KeyEvent) -> bool {
        let control = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER);
        match key.code {
            KeyCode::Char('u' | 'U') if control => self.set(String::new()),
            KeyCode::Char(c) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.insert(&c.to_string())
            }
            KeyCode::Left if self.cursor > 0 => {
                self.cursor = self.text[..self.cursor].char_indices().last().unwrap().0
            }
            KeyCode::Right if self.cursor < self.text.len() => {
                self.cursor += self.text[self.cursor..].chars().next().unwrap().len_utf8()
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            KeyCode::Backspace if self.cursor > 0 => {
                let previous = self.text[..self.cursor].char_indices().last().unwrap().0;
                self.text.drain(previous..self.cursor);
                self.cursor = previous;
            }
            KeyCode::Delete if self.cursor < self.text.len() => {
                let end = self.cursor + self.text[self.cursor..].chars().next().unwrap().len_utf8();
                self.text.drain(self.cursor..end);
            }
            KeyCode::Backspace | KeyCode::Delete | KeyCode::Left | KeyCode::Right => {}
            _ => return false,
        }
        true
    }
    fn display(&self, width: usize, focused: bool) -> String {
        if !focused {
            return clip(&self.text, width);
        }
        let before = &self.text[..self.cursor];
        let after = &self.text[self.cursor..];
        let before = input_line("", before, width.saturating_sub(1));
        clip(&format!("{before}▏{after}"), width)
    }
    fn masked(&self) -> Self {
        Self {
            text: "*".repeat(self.text.chars().count()),
            cursor: self.text[..self.cursor].chars().count(),
        }
    }
}
#[derive(Clone, Copy, PartialEq)]
enum Authentication {
    ApiKey,
    Environment,
    None,
}
struct Hit {
    row: usize,
    x: usize,
    width: usize,
    action: Action,
}
struct Frame {
    lines: Vec<Line>,
    hits: Vec<Hit>,
    controls: Vec<Action>,
    width: usize,
    focus: Action,
    muted: Vec<(usize, usize)>,
    providers: Vec<(usize, bool)>,
}
impl Frame {
    fn new(width: usize, focus: Action) -> Self {
        Self {
            lines: vec![],
            hits: vec![],
            controls: vec![],
            width,
            focus,
            muted: vec![],
            providers: vec![],
        }
    }
    fn text(&mut self, text: impl Into<String>) {
        self.lines.push(line(text));
    }
    fn wrapped(&mut self, text: &str, color: Color) {
        self.lines
            .extend(wrap(text, self.width).into_iter().map(|s| (s, color)));
    }
    fn buttons(&mut self, buttons: &[(Action, &str, bool)]) {
        for &(action, label, enabled) in buttons {
            if !enabled {
                if action == Action::Change {
                    self.lines.push((format!("  {label}"), Color::DarkGrey));
                }
                continue;
            }
            let active = self.focus == action;
            self.controls.push(action);
            self.hits.push(Hit {
                row: self.lines.len(),
                x: 0,
                width: self.width,
                action,
            });
            self.lines.push((
                format!("{} {label}", if active { "›" } else { " " }),
                if active { Color::Cyan } else { Color::Reset },
            ));
        }
    }
    fn divider(&mut self) {
        self.text("");
        self.lines
            .push(("─".repeat(self.width.min(44)), Color::DarkGrey));
    }
    fn input(&mut self, action: Action, label: &str, input: &Edit) {
        self.controls.push(action);
        self.hits.push(Hit {
            row: self.lines.len(),
            x: 0,
            width: self.width,
            action,
        });
        let prefix = format!(
            "{}{}: ",
            if self.focus == action { "> " } else { "  " },
            label
        );
        self.lines.push((
            format!(
                "{}{}",
                prefix,
                input.display(
                    self.width.saturating_sub(prefix.len()),
                    self.focus == action
                )
            ),
            if self.focus == action {
                Color::Cyan
            } else {
                Color::Reset
            },
        ));
    }
    fn list_row(
        &mut self,
        index: usize,
        selected: bool,
        checked: Option<bool>,
        title: &str,
        _subtitle: &str,
    ) {
        let row = self.lines.len();
        self.hits.push(Hit {
            row,
            x: 0,
            width: self.width,
            action: Action::Row(index),
        });
        if checked.is_some() {
            self.hits.push(Hit {
                row,
                x: 2,
                width: 3,
                action: Action::Check(index),
            });
        }
        let active = selected && self.focus == Action::List;
        self.lines.push((
            format!(
                "{} {}{}",
                if active { "›" } else { " " },
                checked
                    .map(|v| if v { "[x] " } else { "[ ] " })
                    .unwrap_or(""),
                title
            ),
            if active { Color::Cyan } else { Color::Reset },
        ));
    }
    fn conversation_row(
        &mut self,
        index: usize,
        selected: bool,
        checked: bool,
        title: &str,
        provider: &str,
        column: usize,
    ) {
        let room = self.width.saturating_sub(6);
        let title_room = if column == 0 {
            room
        } else {
            room.saturating_sub(column + 2)
        };
        self.list_row(index, selected, Some(checked), &clip(title, title_room), "");
        if column == 0 {
            let row = self.lines.len();
            self.hits.push(Hit {
                row,
                x: 0,
                width: self.width,
                action: Action::Row(index),
            });
            self.muted.push((row, 0));
            self.text(format!("      {}", clip(provider, room)));
        } else {
            let row = self.lines.len() - 1;
            let text = &mut self.lines[row].0;
            let used = unicode_width::UnicodeWidthStr::width(text.as_str());
            text.push_str(&" ".repeat(self.width.saturating_sub(column + used)));
            self.muted.push((row, text.len()));
            text.push_str(&clip(provider, column));
        }
    }
}
struct Ui {
    list_start: usize,
    provider_start: usize,
    parents: Vec<(View, Action)>,
    settings: Settings,
    data: Option<app::Catalog>,
    searchable: Vec<SearchEntry>,
    filter: Option<(String, bool)>,
    visible: Vec<usize>,
    view: View,
    focus: Action,
    selected: HashSet<String>,
    targets: HashSet<String>,
    current_providers: HashSet<String>,
    single_pick: bool,
    cursor: usize,
    archived: bool,
    query: Edit,
    provider_query: Edit,
    provider_cursor: usize,
    fields: [Edit; 3],
    authentication: Authentication,
    target: String,
    status: String,
    error: bool,
}
impl Ui {
    fn new(settings: Settings) -> Self {
        Self {
            list_start: 0,
            provider_start: 0,
            parents: vec![],
            settings,
            data: None,
            searchable: vec![],
            filter: None,
            visible: vec![],
            view: View::Conversations,
            focus: Action::Search,
            selected: HashSet::new(),
            targets: HashSet::new(),
            current_providers: HashSet::new(),
            single_pick: false,
            cursor: 0,
            archived: false,
            query: Edit::default(),
            provider_query: Edit::default(),
            provider_cursor: 0,
            fields: Default::default(),
            authentication: Authentication::ApiKey,
            target: String::new(),
            status: String::new(),
            error: false,
        }
    }
    fn fail(&mut self, error: impl ToString) {
        self.status = error.to_string();
        self.error = true;
    }
    fn note(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.error = false;
    }
    fn reload(&mut self, conversations: bool) -> Result<()> {
        self.settings = Settings::load(&self.settings.home)?;
        if conversations {
            let data = app::catalog(&self.settings)?;
            self.searchable = search_index(&data.groups);
            self.data = Some(data);
            self.filter = None;
            self.selected.retain(|id| {
                self.data
                    .as_ref()
                    .unwrap()
                    .groups
                    .iter()
                    .any(|g| &g.root == id)
            });
        }
        Ok(())
    }
    fn filter(&mut self) {
        if self
            .filter
            .as_ref()
            .is_none_or(|(q, a)| q != &self.query.text || *a != self.archived)
        {
            let terms = search_terms(&self.query.text);
            self.visible = self
                .data
                .as_ref()
                .map(|d| {
                    d.groups
                        .iter()
                        .enumerate()
                        .filter(|(i, g)| {
                            g.archived == self.archived
                                && terms
                                    .iter()
                                    .all(|term| self.searchable[*i].text.contains(term))
                        })
                        .map(|(i, _)| i)
                        .collect()
                })
                .unwrap_or_default();
            if let Some(data) = &self.data {
                for &i in &self.visible {
                    self.searchable[i].select_label(&data.groups[i], &terms);
                }
            }
            self.filter = Some((self.query.text.clone(), self.archived));
        }
        self.cursor = self.cursor.min(self.visible.len().saturating_sub(1));
        self.provider_cursor = self.provider_cursor.min(
            provider_matches(&self.settings.providers, &self.provider_query.text)
                .len()
                .saturating_sub(1),
        );
    }
    fn provider(&self) -> Option<String> {
        provider_matches(&self.settings.providers, &self.provider_query.text)
            .get(self.provider_cursor)
            .map(|p| (*p).clone())
    }
    fn editor(&mut self) -> Option<&mut Edit> {
        match (self.view, self.focus) {
            (View::Conversations, Action::Search) => Some(&mut self.query),
            (View::Providers | View::Pick, Action::Search) => Some(&mut self.provider_query),
            (View::Add, Action::Field(i)) => self.fields.get_mut(i),
            _ => None,
        }
    }
    fn footer(&self) -> &'static str {
        match self.view {
            View::Conversations if !matches!(self.focus, Action::List | Action::Search) => {
                "↑↓ action  Enter run  ← Back  Tab switch  Ctrl+C quit"
            }
            View::Conversations => {
                "↑↓ choose  Enter open  Space select  → Actions  Tab switch  Ctrl+C quit"
            }
            View::Providers if !matches!(self.focus, Action::List | Action::Search) => {
                "↑↓ action  Enter run  ← Back  Tab switch  Ctrl+C quit"
            }
            View::Providers => "↑↓ choose  → Actions  Tab switch  Ctrl+C quit",
            View::Add => "↑↓ / Tab next  Enter save  Esc back",
            View::Busy => "Please wait; changes must finish or be recovered.",
            _ => "↑↓ choose  → Actions  Enter confirm  Esc back",
        }
    }
    fn render(&mut self, width: usize, height: usize) -> Frame {
        self.filter();
        let mut f = Frame::new(width, self.focus);
        f.text("Conversations   Providers");
        if matches!(self.view, View::Conversations | View::Providers) {
            f.hits.push(Hit {
                row: 0,
                x: 0,
                width: 13,
                action: Action::Conversations,
            });
            f.hits.push(Hit {
                row: 0,
                x: 16,
                width: 9,
                action: Action::Providers,
            });
        }
        f.text("");
        match self.view {
            View::Pick => {
                f.text(if self.single_pick {
                    "Choose provider".into()
                } else {
                    format!("Choose provider · {} selected", self.targets.len())
                });
                f.text("");
            }
            View::Add => {
                f.text("New provider");
                f.text("");
            }
            View::Busy => {
                f.text("Switching…");
                f.text("");
            }
            _ => {}
        }
        match self.view {
            View::Conversations => {
                f.input(Action::Search, "Search", &self.query);
                f.text("");
                f.controls.push(Action::List);
                let column = if width < 60 {
                    0
                } else {
                    self.visible
                        .iter()
                        .map(|&i| {
                            unicode_width::UnicodeWidthStr::width(self.searchable[i].provider())
                        })
                        .max()
                        .unwrap_or(6)
                        .clamp(6, (width / 4).min(24))
                };
                let row_height = if column == 0 { 2 } else { 1 };
                let footer_height = wrap(self.footer(), width).len() + 1;
                let status_height = if self.status.is_empty() { 0 } else { 4 };
                let page =
                    (height.saturating_sub(12 + footer_height + status_height) / row_height).max(1);
                let start = scroll_start(&mut self.list_start, self.cursor, page);
                if let Some(data) = &self.data {
                    for (position, &i) in self.visible.iter().enumerate().skip(start).take(page) {
                        let g = &data.groups[i];
                        let entry = &self.searchable[i];
                        let label = if g.forked_from.is_some() {
                            format!("Fork · {}", g.rows[entry.row].label)
                        } else {
                            g.rows[entry.row].label.clone()
                        };
                        f.conversation_row(
                            position,
                            self.cursor == position,
                            self.selected.contains(&g.root),
                            &label,
                            entry.provider(),
                            column,
                        );
                    }
                }
                f.text("");
                if self.visible.is_empty() {
                    f.text("No matches");
                }
                f.lines.push((
                    format!(
                        "  {} of {}{}",
                        if self.visible.is_empty() {
                            0
                        } else {
                            self.cursor + 1
                        },
                        self.visible.len(),
                        if self.selected.is_empty() {
                            String::new()
                        } else {
                            format!(" · {} selected", self.selected.len())
                        }
                    ),
                    Color::DarkGrey,
                ));
                f.divider();
                f.buttons(&[
                    (
                        Action::Change,
                        &format!("Change provider ({})", self.selected.len()),
                        !self.selected.is_empty(),
                    ),
                    (
                        Action::SelectAll,
                        if self.selected.is_empty() {
                            "Select all"
                        } else {
                            "Clear selection"
                        },
                        !self.visible.is_empty() || !self.selected.is_empty(),
                    ),
                    (
                        Action::Archive,
                        if self.archived {
                            "Show active"
                        } else {
                            "Show archived"
                        },
                        true,
                    ),
                    (Action::Refresh, "Refresh", true),
                ]);
            }
            View::Providers | View::Pick => {
                f.input(Action::Search, "Search", &self.provider_query);
                f.text("");
                f.controls.push(Action::List);
                let current = self
                    .settings
                    .default_provider()
                    .unwrap_or_else(|_| "unknown".into());
                let matches = provider_matches(&self.settings.providers, &self.provider_query.text);
                let page = height
                    .saturating_sub(
                        f.lines.len()
                            + 5
                            + wrap(self.footer(), width).len()
                            + 1
                            + if self.status.is_empty() { 0 } else { 4 },
                    )
                    .max(1);
                let start = scroll_start(&mut self.provider_start, self.provider_cursor, page);
                for (i, p) in matches.iter().enumerate().skip(start).take(page) {
                    let candidate = if self.view == View::Pick && self.focus != Action::List {
                        **p == self.target
                    } else {
                        i == self.provider_cursor
                    };
                    f.list_row(
                        i,
                        candidate,
                        None,
                        &format!(
                            "{p}{}",
                            if self.view == View::Providers && **p == current {
                                "  · default"
                            } else {
                                ""
                            }
                        ),
                        "",
                    );
                    if candidate && self.view == View::Pick && self.focus != Action::List {
                        let text = &mut f.lines.last_mut().unwrap().0;
                        text.replace_range(..2, "› ");
                    }
                    f.providers.push((
                        f.lines.len() - 1,
                        if self.view == View::Providers {
                            **p == current
                        } else {
                            self.current_providers.contains(*p)
                        },
                    ));
                }
                if matches.is_empty() {
                    f.text("No matches");
                }
                f.divider();
                if self.view == View::Providers {
                    f.buttons(&[
                        (
                            Action::Default,
                            "Use as default",
                            self.provider().is_some_and(|p| p != current),
                        ),
                        (Action::Add, "Add provider", true),
                        (Action::Refresh, "Refresh", true),
                    ]);
                } else {
                    f.buttons(&[
                        (
                            Action::Apply,
                            "Apply",
                            if self.single_pick {
                                !self.target.is_empty()
                            } else {
                                self.provider().is_some()
                            },
                        ),
                        (Action::Add, "Add provider", true),
                        (Action::Cancel, "Back", true),
                    ]);
                }
            }
            View::Add => {
                f.input(Action::Field(0), "Provider Name", &self.fields[0]);
                f.input(Action::Field(1), "Base URL", &self.fields[1]);
                f.buttons(&[(
                    Action::Auth,
                    match self.authentication {
                        Authentication::ApiKey => "Authentication: API key",
                        Authentication::Environment => "Authentication: environment variable",
                        Authentication::None => "Authentication: none",
                    },
                    true,
                )]);
                match self.authentication {
                    Authentication::ApiKey => {
                        f.input(Action::Field(2), "API key", &self.fields[2].masked())
                    }
                    Authentication::Environment => {
                        f.input(Action::Field(2), "Variable name", &self.fields[2])
                    }
                    Authentication::None => {}
                }
                f.text("");
                if self.focus == Action::Field(1) {
                    f.lines.push((
                        "  Example: http://127.0.0.1:3000/v1".into(),
                        Color::DarkGrey,
                    ));
                }
                if self.authentication != Authentication::None && self.focus == Action::Field(2) {
                    f.lines.push((
                        match self.authentication {
                            Authentication::ApiKey => {
                                "  Paste your key. Saved locally in config.toml."
                            }
                            _ => "  Variable name, not the secret value.",
                        }
                        .into(),
                        Color::DarkGrey,
                    ));
                }
                f.divider();
                f.buttons(&[
                    (Action::Save, "Save", true),
                    (Action::Cancel, "Cancel", true),
                ]);
            }
            View::Busy => {
                f.text("Applying changes…");
                f.wrapped(&self.status, Color::Cyan);
                f.text("Keep Codex closed.");
            }
        }
        if self.view != View::Busy && !self.status.is_empty() {
            f.text("");
            let available = height.saturating_sub(f.lines.len() + 3);
            f.lines.extend(
                wrap(&self.status, width)
                    .into_iter()
                    .take(available)
                    .map(|s| {
                        (
                            s,
                            if self.error {
                                Color::Yellow
                            } else {
                                Color::Green
                            },
                        )
                    }),
            );
        }
        f
    }
    fn go(&mut self, view: View) -> Result<()> {
        self.parents.clear();
        self.view = view;
        self.focus = Action::List;
        self.status.clear();
        if view == View::Conversations && self.data.is_none() {
            self.reload(true)?;
        }
        Ok(())
    }
    fn root(&self) -> View {
        self.parents.first().map(|p| p.0).unwrap_or(self.view)
    }
    fn enter(&mut self, view: View, return_focus: Action) {
        self.parents.push((self.view, return_focus));
        self.view = view;
        self.status.clear();
    }
    fn cancel(&mut self) {
        if let Some((view, focus)) = self.parents.pop() {
            self.fields = Default::default();
            self.view = view;
            self.focus = focus;
            self.status.clear();
        }
    }
    fn escape(&mut self) {
        if self.view != View::Add && self.focus != Action::List {
            self.focus = Action::List;
        } else {
            self.cancel();
        }
    }
    fn move_focus(&mut self, controls: &[Action], down: bool) {
        let index = controls.iter().position(|a| *a == self.focus).unwrap_or(0);
        if !controls.is_empty() {
            self.focus = controls[if down {
                (index + 1).min(controls.len() - 1)
            } else {
                index.saturating_sub(1)
            }];
        }
    }
    fn arrow(&mut self, code: KeyCode, controls: &[Action]) {
        if self.view == View::Add {
            if matches!(code, KeyCode::Up | KeyCode::Down) {
                self.move_focus(controls, code == KeyCode::Down);
            }
            return;
        }
        let actions: Vec<_> = controls
            .iter()
            .copied()
            .filter(|a| !matches!(a, Action::Search | Action::List))
            .collect();
        if code == KeyCode::Right {
            if let Some(action) = actions.first() {
                self.focus = *action;
            }
        } else if code == KeyCode::Left || self.focus == Action::Search {
            self.focus = Action::List;
        } else if self.focus == Action::List {
            let cursor = if self.view == View::Conversations {
                &mut self.cursor
            } else {
                &mut self.provider_cursor
            };
            if code == KeyCode::Down {
                *cursor = cursor.saturating_add(1);
            } else {
                *cursor = cursor.saturating_sub(1);
            }
        } else {
            self.move_focus(&actions, code == KeyCode::Down);
        }
    }
    fn activate(&mut self, action: Action) -> Result<bool> {
        match action {
            Action::Conversations => self.go(View::Conversations)?,
            Action::Providers => self.go(View::Providers)?,
            Action::Search | Action::List | Action::Field(_) => self.focus = action,
            Action::Archive => {
                self.archived = !self.archived;
                self.cursor = 0;
                self.focus = Action::List;
            }
            Action::SelectAll => {
                if let Some(data) = &self.data {
                    toggle_selection(
                        &mut self.selected,
                        self.visible.iter().map(|&i| data.groups[i].root.clone()),
                    );
                }
            }
            Action::Row(i) | Action::Check(i) => {
                if self.view == View::Conversations {
                    self.cursor = i;
                    if matches!(action, Action::Check(_))
                        && let Some(g) = self
                            .data
                            .as_ref()
                            .and_then(|d| self.visible.get(i).map(|&n| &d.groups[n]))
                        && !self.selected.remove(&g.root)
                    {
                        self.selected.insert(g.root.clone());
                    }
                } else {
                    self.provider_cursor = i;
                    if self.view == View::Pick
                        && self.single_pick
                        && let Some(provider) = self.provider()
                    {
                        self.settings.validate_provider(&provider)?;
                        self.target = provider;
                    }
                }
                self.focus = Action::List;
            }
            Action::Change | Action::Open => {
                self.single_pick = action == Action::Open;
                self.targets = self.selected.clone();
                let mut current = None;
                if self.single_pick {
                    if self.selected.len() > 1 {
                        return Ok(false);
                    }
                    let index = if self.selected.is_empty() {
                        self.visible.get(self.cursor).copied()
                    } else {
                        self.data
                            .as_ref()
                            .unwrap()
                            .groups
                            .iter()
                            .position(|g| self.selected.contains(&g.root))
                    };
                    if let Some(i) = index {
                        let group = &self.data.as_ref().unwrap().groups[i];
                        self.targets.insert(group.root.clone());
                        current = group.rows[self.searchable[i].row].model_provider.clone();
                    }
                }
                if !self.targets.is_empty() {
                    self.current_providers = self
                        .data
                        .as_ref()
                        .unwrap()
                        .groups
                        .iter()
                        .zip(&self.searchable)
                        .filter(|(group, _)| self.targets.contains(&group.root))
                        .flat_map(|(_, entry)| entry.providers.keys().cloned())
                        .collect();
                    self.reload(false)?;
                    self.enter(
                        View::Pick,
                        if self.single_pick {
                            Action::List
                        } else {
                            Action::Change
                        },
                    );
                    self.provider_query.set(String::new());
                    self.target = current.clone().unwrap_or_default();
                    self.provider_cursor = current
                        .and_then(|current| {
                            provider_matches(&self.settings.providers, "")
                                .iter()
                                .position(|p| **p == current)
                        })
                        .unwrap_or(0);
                    self.provider_start = 0;
                    self.focus = Action::List;
                    self.status.clear();
                }
            }
            Action::Refresh => {
                self.reload(self.view == View::Conversations)?;
                self.note("Updated.");
            }
            Action::Add => {
                self.enter(View::Add, Action::Add);
                self.fields = Default::default();
                self.fields[0].set(self.provider_query.text.trim().into());
                self.authentication = Authentication::ApiKey;
                self.focus = Action::Field(0);
                self.status.clear();
            }
            Action::Auth => {
                self.authentication = match self.authentication {
                    Authentication::ApiKey => Authentication::Environment,
                    Authentication::Environment => Authentication::None,
                    Authentication::None => Authentication::ApiKey,
                };
                self.fields[2] = Edit::default();
                self.status.clear();
            }
            Action::Save => {
                let candidate = app::NewProvider {
                    id: self.fields[0].text.trim().into(),
                    base_url: self.fields[1].text.trim().trim_end_matches('/').into(),
                    env_key: if self.authentication == Authentication::Environment {
                        self.fields[2].text.trim().into()
                    } else {
                        String::new()
                    },
                    api_key: if self.authentication == Authentication::ApiKey {
                        self.fields[2].text.trim().into()
                    } else {
                        String::new()
                    },
                };
                if self.authentication != Authentication::None
                    && self.fields[2].text.trim().is_empty()
                {
                    self.focus = Action::Field(2);
                    return Err(match self.authentication {
                        Authentication::ApiKey => {
                            "Paste your API key, or choose Authentication: none."
                        }
                        _ => "Enter the variable name, or choose Authentication: none.",
                    }
                    .into());
                }
                candidate.validate()?;
                let id = candidate.id.clone();
                let mut settings = Settings::load(&self.settings.home)?;
                settings.new_provider = Some(candidate);
                settings.update_default = false;
                self.settings = settings.save_configuration(&id)?;
                self.fields = Default::default();
                self.cancel();
                self.provider_query.set(String::new());
                self.provider_cursor = self
                    .settings
                    .providers
                    .iter()
                    .position(|provider| provider == &id)
                    .unwrap_or(0);
                if self.view == View::Pick && self.single_pick {
                    self.target = id.clone();
                }
                self.focus = Action::List;
                self.note(format!("Saved {id}."));
            }
            Action::Choose => {
                if let Some(provider) = self.provider() {
                    self.settings.validate_provider(&provider)?;
                    self.target = provider;
                    self.focus = Action::Apply;
                }
            }
            Action::Default => {
                if let Some(provider) = self.provider() {
                    let mut settings = Settings::load(&self.settings.home)?;
                    settings.update_default = true;
                    self.settings = settings.save_configuration(&provider)?;
                    self.note(format!("Default: {provider}"));
                }
            }
            Action::Apply => {
                if let Some(provider) = if self.single_pick {
                    Some(self.target.clone()).filter(|p| !p.is_empty())
                } else {
                    self.provider()
                } {
                    self.settings.validate_provider(&provider)?;
                    self.target = provider;
                    return Ok(true);
                }
            }
            Action::Cancel | Action::Back => self.cancel(),
            Action::Quit => {}
        }
        Ok(false)
    }
}
pub fn run(settings: Settings) -> Result<()> {
    let mut screen = Screen::enter()?;
    if !confirm_recovery(&settings, &mut screen)? {
        return Ok(());
    }
    let mut ui = Ui::new(settings);
    screen.draw(&[line("Reading conversations…")], "")?;
    if let Err(e) = ui.reload(true) {
        ui.fail(e);
    }
    let (tx, rx) = mpsc::channel::<std::result::Result<String, String>>();
    let mut worker: Option<std::thread::JoinHandle<()>> = None;
    loop {
        if ui.view == View::Busy {
            while let Ok(message) = rx.try_recv() {
                match message {
                    Ok(progress) => ui.status = progress,
                    Err(outcome) => {
                        if let Some(handle) = worker.take() {
                            let _ = handle.join();
                        }
                        if outcome.starts_with("Error:") {
                            ui.view = View::Pick;
                            ui.focus = Action::Apply;
                            ui.fail(outcome);
                        } else {
                            ui.parents.clear();
                            ui.view = View::Conversations;
                            ui.focus = Action::List;
                            ui.selected.clear();
                            if let Err(e) = ui.reload(true) {
                                ui.fail(e);
                            } else {
                                ui.note(outcome);
                            }
                        }
                    }
                }
            }
            if worker.as_ref().is_some_and(|w| w.is_finished()) {
                let handle = worker.take().unwrap();
                if handle.join().is_err() {
                    ui.view = View::Pick;
                    ui.fail(
                        "Worker stopped. Keep Codex closed; restart to recover before retrying.",
                    );
                }
            }
        }
        let (w, h) = terminal::size()?;
        let frame = ui.render(w.saturating_sub(2) as usize, h as usize);
        let footer = ui.footer();
        screen.draw_styled(
            &frame.lines,
            footer,
            &frame.muted,
            &frame.providers,
            Some(ui.root()),
        )?;
        if ui.view == View::Busy {
            let _ = event::poll(Duration::from_millis(100))?;
            if event::poll(Duration::ZERO)? {
                let _ = event::read()?;
            }
            continue;
        }
        let event = loop {
            let event = event::read()?;
            if matches!(event, Event::Mouse(m) if matches!(m.kind, MouseEventKind::Moved | MouseEventKind::Drag(_) | MouseEventKind::Up(_)))
                || matches!(event, Event::Key(k) if k.kind == KeyEventKind::Release)
            {
                continue;
            }
            break event;
        };
        let mut action = None;
        match event {
            Event::Resize(_, _) => continue,
            Event::Paste(text) => {
                if matches!(ui.view, View::Conversations | View::Providers | View::Pick) {
                    ui.focus = Action::Search;
                }
                if let Some(edit) = ui.editor() {
                    edit.insert(text.trim());
                    ui.status.clear();
                }
                continue;
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    let x = mouse.column.saturating_sub(1) as usize;
                    action = frame
                        .hits
                        .iter()
                        .rev()
                        .find(|hit| {
                            hit.row == mouse.row as usize && x >= hit.x && x < hit.x + hit.width
                        })
                        .map(|hit| hit.action);
                }
                MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                    let down = mouse.kind == MouseEventKind::ScrollDown;
                    let cursor = if ui.view == View::Conversations {
                        &mut ui.cursor
                    } else {
                        &mut ui.provider_cursor
                    };
                    *cursor = if down {
                        cursor.saturating_add(1)
                    } else {
                        cursor.saturating_sub(1)
                    };
                }
                _ => {}
            },
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c' | 'C'))
                {
                    return Ok(());
                }
                if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                    if matches!(ui.view, View::Conversations | View::Providers) {
                        action = Some(if ui.view == View::Conversations {
                            Action::Providers
                        } else {
                            Action::Conversations
                        });
                    } else {
                        let i = frame
                            .controls
                            .iter()
                            .position(|&a| a == ui.focus)
                            .unwrap_or(0);
                        if !frame.controls.is_empty() {
                            ui.focus = frame.controls[(i + if key.code == KeyCode::BackTab {
                                frame.controls.len() - 1
                            } else {
                                1
                            }) % frame.controls.len()];
                        }
                    }
                } else if key.code == KeyCode::Esc {
                    ui.escape();
                } else if ui.view == View::Conversations
                    && key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
                    && matches!(key.code, KeyCode::Char('a' | 'A' | 'ф' | 'Ф'))
                {
                    if key.kind == KeyEventKind::Press {
                        action = Some(Action::SelectAll);
                    }
                } else if ui.editor().is_some_and(|edit| edit.key(key)) {
                    ui.status.clear();
                } else if matches!(
                    key.code,
                    KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right
                ) {
                    ui.arrow(key.code, &frame.controls);
                } else if key.code == KeyCode::Enter {
                    action = Some(match ui.focus {
                        Action::Search | Action::List => match ui.view {
                            View::Conversations if ui.selected.len() <= 1 => Action::Open,
                            View::Conversations => Action::List,
                            View::Pick => Action::Choose,
                            View::Providers => Action::List,
                            _ => Action::Back,
                        },
                        Action::Field(_) => {
                            let i = frame.controls.iter().position(|&a| a == ui.focus).unwrap();
                            ui.focus = frame.controls[(i + 1) % frame.controls.len()];
                            continue;
                        }
                        _ => ui.focus,
                    });
                } else if key.code == KeyCode::Char(' ')
                    && ui.focus == Action::List
                    && ui.view == View::Conversations
                {
                    action = Some(Action::Check(ui.cursor));
                } else if key.code == KeyCode::Char(' ') && ui.focus == Action::Auth {
                    action = Some(ui.focus);
                } else if matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace)
                    && matches!(ui.view, View::Conversations | View::Providers | View::Pick)
                {
                    ui.focus = Action::Search;
                    if let Some(edit) = ui.editor() {
                        edit.key(key);
                    }
                }
            }
            _ => {}
        }
        if let Some(action) = action {
            if action == Action::Quit {
                return Ok(());
            }
            match ui.activate(action) {
                Err(e) => ui.fail(e),
                Ok(false) => {}
                Ok(true) => {
                    let home = ui.settings.home.clone();
                    let ids = ui.targets.clone();
                    let provider = ui.target.clone();
                    let tx = tx.clone();
                    ui.view = View::Busy;
                    ui.note("Validating selected conversations");
                    worker = Some(std::thread::spawn(move || {
                        let result = (|| {
                            let mut settings = Settings::load(&home)?;
                            settings.update_default = false;
                            migrate::migrate(&settings, &ids, false, &provider, &|s| {
                                let _ = tx.send(Ok(s.replace("task families", "conversations")));
                            })
                        })();
                        let message = match result {
                            Ok(value) => format!(
                                "Switched to {provider} in {:.2}s. You can open Codex.",
                                value["seconds"].as_f64().unwrap_or(0.)
                            ),
                            Err(e) => format!("Error: {e}"),
                        };
                        let _ = tx.send(Err(message));
                    }));
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn provider_is_current(frame: &Frame, name: &str) -> bool {
        frame.providers.iter().any(|&(row, current)| {
            current
                && frame.lines[row]
                    .0
                    .trim_start_matches([' ', '›'])
                    .starts_with(name)
        })
    }
    #[test]
    fn vertical_layout_keeps_actions_and_errors_visible() {
        let settings = Settings {
            home: Default::default(),
            database: Default::default(),
            providers: vec!["openai".into()],
            raw: b"model_provider = \"openai\"".to_vec(),
            new_provider: None,
            update_default: true,
        };
        let mut ui = Ui::new(settings);
        let groups=(0..40).map(|i| {
            let id=format!("conversation-{i}");
            let row:app::Row=serde_json::from_value(serde_json::json!({"id":id,"model_provider":"openai","rollout_path":"unused","archived":0,"history_mode":"paginated","label":format!("A readable conversation {i}"),"cwd":"/example"})).unwrap();
            Group{root:id,forked_from:None,ids:Default::default(),rows:vec![row],files:vec![],issues:vec![],archived:false,updated:0}
        }).collect::<Vec<_>>();
        ui.searchable = search_index(&groups);
        ui.data = Some(app::Catalog {
            groups,
            files: vec![],
            seconds: 0.,
        });
        let empty_selection = ui.render(78, 24);
        assert!(
            empty_selection
                .lines
                .iter()
                .any(|(text, _)| text == "  Change provider (0)")
        );
        assert!(!empty_selection.controls.contains(&Action::Change));
        assert!(
            !empty_selection
                .hits
                .iter()
                .any(|hit| hit.action == Action::Change)
        );
        assert!(!ui.activate(Action::Change).unwrap());
        assert_eq!(ui.view, View::Conversations);
        assert!(ui.targets.is_empty());
        ui.selected.insert("conversation-0".into());
        let selected = ui.render(78, 24);
        assert!(
            selected
                .lines
                .iter()
                .any(|(text, _)| text.ends_with("Change provider (1)"))
        );
        assert!(selected.controls.contains(&Action::Change));
        ui.view = View::Pick;
        ui.focus = Action::List;
        ui.single_pick = true;
        ui.target = "openai".into();
        ui.current_providers.insert("openai".into());
        let picker = ui.render(78, 24);
        let provider = picker
            .lines
            .iter()
            .find(|(text, _)| text == "› openai")
            .unwrap();
        assert!(provider.0.starts_with("› "));
        assert!(provider_is_current(&picker, "openai"));
        ui.settings.providers.push("factory".into());
        ui.provider_cursor = 1;
        let hovered = ui.render(78, 24);
        assert!(hovered.lines.iter().any(|(text, _)| text == "  openai"));
        assert!(hovered.lines.iter().any(|(text, _)| text == "› factory"));
        assert!(provider_is_current(&hovered, "openai"));
        assert!(!provider_is_current(&hovered, "factory"));
        ui.activate(Action::Choose).unwrap();
        assert_eq!(ui.target, "factory");
        let chosen = ui.render(78, 24);
        assert!(provider_is_current(&chosen, "openai"));
        assert!(!provider_is_current(&chosen, "factory"));
        assert!(chosen.lines.iter().any(|(text, _)| text == "› factory"));
        ui.provider_cursor = 0;
        assert!(ui.activate(Action::Apply).unwrap());
        assert_eq!(
            ui.target, "factory",
            "Apply must use the explicit choice, not the cursor"
        );
        ui.focus = Action::List;
        ui.single_pick = false;
        assert!(
            ui.render(78, 24)
                .lines
                .iter()
                .any(|(text, _)| text == "› openai")
        );
        assert!(provider_is_current(&ui.render(78, 24), "openai"));
        ui.current_providers.insert("factory".into());
        assert!(provider_is_current(&ui.render(78, 24), "factory"));
        ui.view = View::Providers;
        let defaults = ui.render(78, 24);
        assert!(provider_is_current(&defaults, "openai"));
        assert!(!provider_is_current(&defaults, "factory"));
        ui.view = View::Conversations;
        for (width, height) in [(58, 20), (78, 24), (118, 40)] {
            for error in [false, true] {
                ui.status = if error {
                    "Could not save. Correct the setting and try again.".into()
                } else {
                    String::new()
                };
                let frame = ui.render(width, height);
                let actions: Vec<_> = frame
                    .hits
                    .iter()
                    .filter(|h| {
                        matches!(
                            h.action,
                            Action::Change | Action::SelectAll | Action::Archive | Action::Refresh
                        )
                    })
                    .collect();
                assert_eq!(actions.len(), 4);
                assert!(actions.windows(2).all(|a| a[0].row < a[1].row));
                assert!(actions.iter().all(|a| a.row < height - 2));
                assert!(frame.lines.len() <= height - 2);
                if error {
                    assert!(
                        frame
                            .lines
                            .iter()
                            .any(|(text, _)| text.contains("Could not save"))
                    );
                }
            }
        }
        let home = std::env::temp_dir().join(format!(
            "provider-color-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join("config.toml"), &ui.settings.raw).unwrap();
        ui.settings.home = home.clone();
        ui.data.as_mut().unwrap().groups[0].rows[0].model_provider = Some("factory".into());
        ui.data.as_mut().unwrap().groups[1].rows[0].model_provider = None;
        ui.searchable = search_index(&ui.data.as_ref().unwrap().groups);
        ui.selected.insert("conversation-1".into());
        ui.activate(Action::Change).unwrap();
        assert_eq!(
            ui.current_providers,
            HashSet::from(["openai".into(), "factory".into()])
        );
        std::fs::remove_file(home.join("config.toml")).unwrap();
        std::fs::remove_dir(home).unwrap();
    }
    #[test]
    fn scrolling_and_navigation_preserve_selection() {
        let settings = Settings {
            home: Default::default(),
            database: Default::default(),
            providers: vec!["openai".into()],
            raw: vec![],
            new_provider: None,
            update_default: false,
        };
        let mut ui = Ui::new(settings);
        ui.query.set("kept search".into());
        ui.selected.insert("selected conversation".into());
        ui.cursor = 8;
        ui.focus = Action::List;
        let controls = [
            Action::Search,
            Action::List,
            Action::Change,
            Action::SelectAll,
            Action::Archive,
            Action::Refresh,
        ];
        ui.arrow(KeyCode::Right, &controls);
        assert_eq!(ui.focus, Action::Change);
        ui.arrow(KeyCode::Down, &controls);
        assert_eq!(ui.focus, Action::SelectAll);
        ui.arrow(KeyCode::Left, &controls);
        assert_eq!(ui.focus, Action::List);
        assert_eq!(ui.cursor, 8);
        let mut start = 0;
        for cursor in 0..20 {
            assert_eq!(
                scroll_start(&mut start, cursor, 5),
                cursor.saturating_sub(4)
            );
        }
        assert_eq!(scroll_start(&mut start, 14, 5), 14);
        ui.enter(View::Pick, Action::Change);
        for _ in 0..100 {
            ui.enter(View::Add, Action::Add);
            ui.focus = Action::Field(0);
            ui.escape();
            assert_eq!(ui.view, View::Pick);
            assert_eq!(ui.parents.len(), 1);
        }
        ui.escape(); // Actions back to the provider list.
        assert_eq!(ui.focus, Action::List);
        ui.escape();
        assert_eq!(ui.view, View::Conversations);
        ui.escape(); // Actions back to the conversation list.
        for _ in 0..100 {
            ui.escape();
        }
        assert_eq!(ui.view, View::Conversations);
        assert!(ui.parents.is_empty());
        assert_eq!(ui.selected.len(), 1);
        assert_eq!(ui.query.text, "kept search");
        assert_eq!(ui.cursor, 8);
        ui.focus = Action::Search;
        ui.escape();
        assert_eq!(ui.focus, Action::List);
        assert_eq!(ui.query.text, "kept search");
    }
    #[test]
    fn adding_provider_shows_full_list_and_selects_new_entry() {
        let home = std::env::temp_dir().join(format!(
            "provider-add-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&home).unwrap();
        for (view, single_pick) in [
            (View::Providers, false),
            (View::Pick, true),
            (View::Pick, false),
        ] {
            std::fs::write(
                home.join("config.toml"),
                "model_provider = \"openai\"\n[model_providers.existing]\nname = \"Existing\"\n",
            )
            .unwrap();
            let mut ui = Ui::new(Settings::load(&home).unwrap());
            ui.view = view;
            ui.single_pick = single_pick;
            ui.target = "openai".into();
            ui.provider_query.set("previous search".into());
            ui.activate(Action::Add).unwrap();
            ui.fields[0].set("myproxy".into());
            ui.fields[1].set("http://127.0.0.1:3000/v1".into());
            ui.authentication = Authentication::None;
            ui.activate(Action::Save).unwrap();
            assert_eq!(ui.view, view);
            assert!(ui.provider_query.text.is_empty());
            assert_eq!(ui.provider().as_deref(), Some("myproxy"));
            assert_eq!(ui.settings.default_provider().unwrap(), "openai");
            let frame = ui.render(80, 24);
            for name in ["openai", "existing", "myproxy"] {
                assert!(frame.lines.iter().any(|(text, _)| text.contains(name)));
            }
            if view == View::Pick {
                assert!(ui.activate(Action::Apply).unwrap());
                assert_eq!(ui.target, "myproxy");
            }
        }
        std::fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn unicode_editor_and_small_form_layout() {
        let mut edit = Edit::default();
        edit.insert("AяB");
        edit.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        edit.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(edit.text, "AB");
        edit.insert("中");
        assert_eq!(edit.text, "A中B");
        edit.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        edit.key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(edit.text, "中B");
        assert!(unicode_width::UnicodeWidthStr::width(edit.display(4, true).as_str()) <= 4);
        let mut secret = Edit::default();
        secret.insert("key-中-value");
        secret.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(secret.masked().display(40, true), "**********▏*");
        assert_eq!(secret.masked().display(40, false), "***********");
    }
    #[test]
    fn long_input_keeps_label_and_editing_end_visible() {
        assert_eq!(input_line("URL: ", "abcdefghijk", 12), "URL: …fghijk");
        assert_eq!(input_line("ID: ", "mine", 20), "ID: mine");
        assert_eq!(input_line("URL: ", "abc", 0), "");
    }
    #[test]
    fn provider_search_preserves_ids_and_prefers_exact_match() {
        let providers = vec!["openai".into(), "MyProxyExtra".into(), "MyProxy".into()];
        assert_eq!(
            provider_matches(&providers, "myproxy"),
            vec![&providers[2], &providers[1]]
        );
        assert_eq!(
            provider_matches(&providers, ""),
            providers.iter().collect::<Vec<_>>()
        );
        assert!(provider_matches(&providers, "missing").is_empty());
    }
    #[test]
    fn select_all_and_clear_partial_selection() {
        let mut selected = HashSet::new();
        let all = || ["a", "b"].into_iter().map(str::to_owned);
        toggle_selection(&mut selected, all());
        assert_eq!(selected.len(), 2);
        toggle_selection(&mut selected, all());
        assert!(selected.is_empty());
        selected.insert("hidden-archived".into());
        toggle_selection(&mut selected, all());
        assert!(selected.is_empty());
    }
    #[test]
    fn cached_search_preserves_family_matches() {
        let row: app::Row = serde_json::from_value(serde_json::json!({
            "id":"child", "model_provider":"openai", "rollout_path":"unused",
            "archived":1, "history_mode":"paginated", "label":"Проверь РУК Atlas",
            "cwd":"D:/Example"
        }))
        .unwrap();
        let group = Group {
            forked_from: None,
            root: "root".into(),
            ids: Default::default(),
            rows: vec![row],
            files: vec![],
            issues: vec![],
            archived: false,
            updated: 0,
        };
        let index = search_index(std::slice::from_ref(&group));
        for (query, expected) in [
            ("", true),
            ("РУК", true),
            ("Atla", true),
            ("example", true),
            ("child", true),
            ("missing", false),
            ("AtlasD:/Example", false),
        ] {
            assert_eq!(
                search_terms(query)
                    .iter()
                    .all(|term| index[0].text.contains(term)),
                expected,
                "{query}"
            );
        }
        let mut family = group;
        let mut root = family.rows[0].clone();
        root.id = "root".into();
        root.label = "Read DOC1 retraining runbook".into();
        root.archived = 0;
        family.rows[0].label = "DOC 1 новое обучение".into();
        family.rows[0].archived = 0;
        family.rows[0].updated = 200;
        family.rows.push(root);
        let mut index = search_index(std::slice::from_ref(&family));
        for query in ["DOC1", "DOC-1", "обучение doc1", "doc 1 новое обучение"]
        {
            let terms = search_terms(query);
            assert!(terms.iter().all(|term| index[0].text.contains(term)));
            index[0].select_label(&family, &terms);
            assert_eq!(family.rows[index[0].row].label, "DOC 1 новое обучение");
        }
        index[0].select_label(&family, &[]);
        assert_eq!(family.rows[index[0].row].id, "root");
    }
    #[test]
    fn unicode_layout() {
        for width in [2, 20, 40, 80, 160] {
            for s in [
                "Русское название задачи 日本語",
                "long".repeat(80).as_str(),
                "title\x1b[31m\r\n",
            ] {
                assert!(unicode_width::UnicodeWidthStr::width(clip(s, width).as_str()) <= width);
                assert!(!clip(s, width).contains('\x1b'));
                for l in wrap(s, width) {
                    assert!(unicode_width::UnicodeWidthStr::width(l.as_str()) <= width);
                }
            }
        }
    }
}
