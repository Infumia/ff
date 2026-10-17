//! Interactive finder state, independent of the terminal so it can be tested.

use std::collections::HashMap;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use regex::Regex;

use crate::content::{ContentBatch, ContentMatch};
use crate::editor::Target;
use crate::fuzzy::{extension_narrows, Match, Pattern};
use crate::matcher::{build_regex, char_range, Matcher};
use crate::tree::{self, Tree};

/// Content results beyond this are dropped to keep the UI responsive.
pub const MAX_CONTENT_RESULTS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Match on file paths (fuzzy, or regex).
    Files,
    /// Search inside files (literal, or regex).
    Content,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Quit,
    /// The user picked entries; the string is what gets printed, one per line.
    Accept(String),
    /// Open these in the editor (Ctrl+O).
    Open(Vec<Target>),
    /// Copy this text to the clipboard (Ctrl+Y) and keep going.
    Copy(String),
}

/// Where a mouse event happened (see `ui::hit_test`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// A visible row of the result list.
    List(usize),
    Preview,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatch {
    pub idx: usize,
    pub score: i64,
    pub positions: Vec<usize>,
}

/// How file paths are matched: fuzzy (ranked by score) or regex.
#[derive(Debug, Clone)]
pub enum FileFilter {
    Fuzzy(Pattern),
    Regex(Regex),
}

impl FileFilter {
    /// A blank query matches every file, even in regex mode.
    pub fn new(query: &str, regex: bool) -> Result<Self, String> {
        if regex && !query.trim().is_empty() {
            build_regex(query).map(FileFilter::Regex)
        } else {
            Ok(FileFilter::Fuzzy(Pattern::new(query)))
        }
    }

    pub fn matches(&self, path: &str) -> Option<Match> {
        match self {
            FileFilter::Fuzzy(p) => p.matches(path),
            FileFilter::Regex(r) => {
                // Match against `/`-separated paths so `src/.*\.rs` also works
                // on Windows. Swapping one char for another keeps char indices.
                let normalized = path.replace('\\', "/");
                let m = r.find(&normalized)?;
                let (start, end) = char_range(&normalized, m.start(), m.end());
                Some(Match { score: 0, positions: (start..end).collect() })
            }
        }
    }

    /// Whether results are ordered by score; otherwise walk order is kept.
    pub fn is_ranked(&self) -> bool {
        matches!(self, FileFilter::Fuzzy(p) if !p.is_empty())
    }
}

/// Matches the files at `candidates`, best first for a fuzzy query and in
/// walk order otherwise.
pub fn rank_files(
    files: &[Arc<str>],
    filter: &FileFilter,
    candidates: impl IntoIterator<Item = usize>,
) -> Vec<FileMatch> {
    let mut out: Vec<FileMatch> = candidates
        .into_iter()
        .filter_map(|idx| {
            filter
                .matches(&files[idx])
                .map(|m| FileMatch { idx, score: m.score, positions: m.positions })
        })
        .collect();
    sort_file_matches(&mut out, files, filter.is_ranked());
    out
}

fn sort_file_matches(matches: &mut [FileMatch], files: &[Arc<str>], ranked: bool) {
    if ranked {
        // Higher score first, then shorter path, then walk order.
        matches.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| files[a.idx].len().cmp(&files[b.idx].len()))
                .then(a.idx.cmp(&b.idx))
        });
    } else {
        matches.sort_by_key(|m| m.idx);
    }
}

/// Adds frecency bonuses to fuzzy scores (the caller re-sorts).
fn apply_boosts(boosts: &HashMap<String, i64>, files: &[Arc<str>], matches: &mut [FileMatch]) {
    if boosts.is_empty() {
        return;
    }
    for m in matches {
        m.score += boosts.get(files[m.idx].as_ref()).copied().unwrap_or(0);
    }
}

pub struct App {
    files: Vec<Arc<str>>,
    walking: bool,
    mode: Mode,
    /// The active mode's query; each mode keeps its own.
    query: Vec<char>,
    cursor: usize,
    /// The other mode's query and cursor, swapped in on Tab.
    other_query: (Vec<char>, usize),
    regex: bool,
    /// Why the current query matches nothing (an invalid regex).
    query_error: Option<String>,
    /// The (query, regex) `file_matches` was computed for. File matches are
    /// refreshed lazily, so this may lag behind while in content mode.
    file_key: (String, bool),
    file_filter: Option<FileFilter>,
    file_matches: Vec<FileMatch>,
    /// The current content matcher; `None` for a blank or invalid query.
    matcher: Option<Arc<Matcher>>,
    content_matches: Vec<ContentMatch>,
    content_truncated: bool,
    /// Bumped whenever content results become stale; old results are dropped.
    generation: u64,
    content_pending: bool,
    /// Tree view: results in walk order, grouped by directory.
    tree_view: bool,
    tree: Tree,
    tree_dirty: bool,
    /// The searched directory, shown as the tree's top row.
    root: String,
    selected: usize,
    /// First visible list row (a tree row in tree view).
    offset: usize,
    page: usize,
    show_preview: bool,
    /// Entries marked with Ctrl+S, in marking order; kept across queries.
    marked: Vec<Target>,
    /// A one-off message for the status bar, cleared by the next key.
    flash: Option<String>,
    /// Lines the preview is scrolled away from its default position.
    preview_scroll: isize,
    /// Earlier queries, oldest first, for Alt/Ctrl+Up/Down.
    history: Vec<String>,
    /// Index into `history` while browsing it, and the query typed before.
    history_pos: Option<usize>,
    history_draft: Vec<char>,
    /// Ranking bonus per displayed path for often/recently picked files.
    boosts: HashMap<String, i64>,
    /// The (query, regex) of the current content matcher.
    content_key: Option<(String, bool)>,
    /// Content searches started for the current generation and not done yet.
    searches_running: usize,
    /// The generation whose content search covered every file.
    completed_generation: Option<u64>,
    /// When a literal content query was extended, only these files (walk
    /// indices, sorted) can still match.
    narrow_to: Option<Vec<usize>>,
}

impl App {
    pub fn new(mode: Mode, query: &str) -> Self {
        let query: Vec<char> = query.chars().collect();
        let mut app = App {
            files: Vec::new(),
            walking: true,
            mode,
            cursor: query.len(),
            query,
            other_query: (Vec::new(), 0),
            regex: false,
            query_error: None,
            file_key: (String::new(), false),
            file_filter: None,
            file_matches: Vec::new(),
            matcher: None,
            content_matches: Vec::new(),
            content_truncated: false,
            generation: 0,
            content_pending: false,
            tree_view: false,
            tree: Tree::default(),
            tree_dirty: true,
            root: ".".to_string(),
            selected: 0,
            offset: 0,
            page: 10,
            show_preview: true,
            marked: Vec::new(),
            flash: None,
            preview_scroll: 0,
            history: Vec::new(),
            history_pos: None,
            history_draft: Vec::new(),
            boosts: HashMap::new(),
            content_key: None,
            searches_running: 0,
            completed_generation: None,
            narrow_to: None,
        };
        app.on_query_changed();
        app
    }

    pub fn files(&self) -> &[Arc<str>] {
        &self.files
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn query(&self) -> String {
        self.query.iter().collect()
    }

    /// The file-mode query, whichever mode is active.
    pub fn files_query(&self) -> String {
        match self.mode {
            Mode::Files => self.query(),
            Mode::Content => self.other_query.0.iter().collect(),
        }
    }

    /// In content mode, only files matching the file-mode query are
    /// searched. Returns that query when it narrows anything.
    pub fn content_scope(&self) -> Option<String> {
        let q = self.files_query();
        (!q.trim().is_empty()).then_some(q)
    }

    /// Files a content search should read, from walk index `from` on: all of
    /// them, or only those in the scope. Sorted by walk index.
    pub fn content_targets(&self, from: usize) -> Vec<(usize, Arc<str>)> {
        if let Some(narrow) = &self.narrow_to {
            return narrow
                .iter()
                .filter(|&&i| i >= from && i < self.files.len())
                .map(|&i| (i, self.files[i].clone()))
                .collect();
        }
        if self.content_scope().is_none() {
            return self.files[from.min(self.files.len())..]
                .iter()
                .enumerate()
                .map(|(i, f)| (from + i, f.clone()))
                .collect();
        }
        let mut targets: Vec<(usize, Arc<str>)> = self
            .file_matches
            .iter()
            .filter(|m| m.idx >= from)
            .map(|m| (m.idx, self.files[m.idx].clone()))
            .collect();
        targets.sort_unstable_by_key(|t| t.0);
        targets
    }

    /// How many files a content search covers.
    pub fn content_target_count(&self) -> usize {
        match self.content_scope() {
            Some(_) => self.file_matches.len(),
            None => self.files.len(),
        }
    }

    /// Cursor position in chars.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_walking(&self) -> bool {
        self.walking
    }

    pub fn finish_walk(&mut self) {
        self.walking = false;
        self.check_content_complete();
    }

    pub fn set_history(&mut self, queries: Vec<String>) {
        self.history = queries;
        self.history_pos = None;
    }

    /// Sets ranking bonuses (see `History::boosts_under`) and re-ranks.
    pub fn set_boosts(&mut self, boosts: HashMap<String, i64>) {
        self.boosts = boosts;
        self.file_filter = None;
        self.on_query_changed();
    }

    /// The caller started a content search for the current generation.
    pub fn search_started(&mut self) {
        self.searches_running += 1;
    }

    /// Whether the content search for the current query has finished.
    pub fn content_complete(&self) -> bool {
        self.completed_generation == Some(self.generation)
    }

    fn check_content_complete(&mut self) {
        if !self.walking && self.searches_running == 0 && self.matcher.is_some() && self.mode == Mode::Content {
            self.completed_generation = Some(self.generation);
        }
    }

    pub fn file_matches(&self) -> &[FileMatch] {
        &self.file_matches
    }

    pub fn content_matches(&self) -> &[ContentMatch] {
        &self.content_matches
    }

    pub fn content_truncated(&self) -> bool {
        self.content_truncated
    }

    pub fn show_preview(&self) -> bool {
        self.show_preview
    }

    pub fn regex(&self) -> bool {
        self.regex
    }

    pub fn query_error(&self) -> Option<&str> {
        self.query_error.as_deref()
    }

    /// The content matcher, used to highlight matches in the preview.
    pub fn matcher(&self) -> Option<&Matcher> {
        self.matcher.as_deref()
    }

    pub fn tree_view(&self) -> bool {
        self.tree_view
    }

    /// Rows of the tree view; up to date after `adjust_scroll`.
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Sets the file-mode query (the content scope when in content mode).
    pub fn set_files_query(&mut self, query: &str) {
        let chars: Vec<char> = query.chars().collect();
        let len = chars.len();
        match self.mode {
            Mode::Files => (self.query, self.cursor) = (chars, len),
            Mode::Content => self.other_query = (chars, len),
        }
        self.on_query_changed();
    }

    pub fn set_root(&mut self, root: &str) {
        self.root = root.to_string();
        self.tree_dirty = true;
    }

    pub fn set_regex(&mut self, on: bool) {
        if self.regex != on {
            self.regex = on;
            self.on_query_changed();
        }
    }

    pub fn set_tree_view(&mut self, on: bool) {
        if self.tree_view != on {
            self.tree_view = on;
            // The order changes between ranked and walk order.
            self.sort_file_view();
            self.selected = 0;
            self.offset = 0;
            self.preview_scroll = 0;
            self.tree_dirty = true;
        }
    }

    /// The file (and, in content mode, the line) the preview pane should show.
    pub fn preview_target(&self) -> Option<(Arc<str>, Option<usize>)> {
        match self.mode {
            Mode::Files => self.file_matches.get(self.selected).map(|m| (self.files[m.idx].clone(), None)),
            Mode::Content => self.content_matches.get(self.selected).map(|m| (m.path.clone(), Some(m.line))),
        }
    }

    /// The file/line that result `i` stands for.
    pub fn target_of(&self, i: usize) -> Option<Target> {
        match self.mode {
            Mode::Files => self.file_matches.get(i).map(|m| Target { path: self.files[m.idx].to_string(), line: None }),
            Mode::Content => self.content_matches.get(i).map(|m| Target { path: m.path.to_string(), line: Some(m.line) }),
        }
    }

    pub fn is_marked(&self, i: usize) -> bool {
        self.target_of(i).is_some_and(|t| self.marked.contains(&t))
    }

    pub fn marked_count(&self) -> usize {
        self.marked.len()
    }

    /// What Enter, Ctrl+O and Ctrl+Y act on: the marked entries if any,
    /// otherwise the selected one.
    pub fn chosen(&self) -> Vec<Target> {
        if self.marked.is_empty() {
            self.target_of(self.selected).into_iter().collect()
        } else {
            self.marked.clone()
        }
    }

    pub fn preview_scroll(&self) -> isize {
        self.preview_scroll
    }

    /// Corrects the scroll after the preview hit the start or end of a file.
    pub fn set_preview_scroll(&mut self, scroll: isize) {
        self.preview_scroll = scroll;
    }

    fn scroll_preview(&mut self, delta: isize) {
        self.preview_scroll += delta;
    }

    /// Mouse: the wheel moves the selection or scrolls the preview; a left
    /// click selects a row.
    pub fn handle_mouse(&mut self, kind: MouseEventKind, hit: Hit) {
        self.flash = None;
        match (kind, hit) {
            (MouseEventKind::ScrollUp, Hit::Preview) => self.scroll_preview(-3),
            (MouseEventKind::ScrollDown, Hit::Preview) => self.scroll_preview(3),
            (MouseEventKind::ScrollUp, _) => self.move_selection(-1),
            (MouseEventKind::ScrollDown, _) => self.move_selection(1),
            (MouseEventKind::Down(MouseButton::Left), Hit::List(row)) => self.click_row(row),
            _ => {}
        }
    }

    /// Selects the result shown on visible list row `row`, if any.
    fn click_row(&mut self, row: usize) {
        let i = self.offset + row;
        let result = if self.tree_view {
            self.tree.rows.get(i).and_then(|r| r.result)
        } else {
            (i < self.result_count()).then_some(i)
        };
        if let Some(i) = result {
            self.select(i);
        }
    }

    fn select(&mut self, i: usize) {
        if i != self.selected {
            self.selected = i;
            self.preview_scroll = 0;
        }
    }

    pub fn flash(&self) -> Option<&str> {
        self.flash.as_deref()
    }

    pub fn set_flash(&mut self, message: impl Into<String>) {
        self.flash = Some(message.into());
    }

    pub fn result_count(&self) -> usize {
        match self.mode {
            Mode::Files => self.file_matches.len(),
            Mode::Content => self.content_matches.len(),
        }
    }

    /// Adds newly discovered files and returns the index of the first one.
    pub fn add_files(&mut self, batch: Vec<Arc<str>>) -> usize {
        let base = self.files.len();
        self.files.extend(batch);
        if let Some(filter) = &self.file_filter {
            let mut new = rank_files(&self.files, filter, base..self.files.len());
            if filter.is_ranked() {
                apply_boosts(&self.boosts, &self.files, &mut new);
            }
            if !new.is_empty() {
                self.file_matches.extend(new);
                self.sort_file_view();
                self.tree_dirty = true;
            }
        }
        base
    }

    /// The content search that should currently be running, if any.
    pub fn content_search(&self) -> Option<(u64, Arc<Matcher>)> {
        match (&self.matcher, self.mode) {
            (Some(m), Mode::Content) => Some((self.generation, m.clone())),
            _ => None,
        }
    }

    /// Returns a search to start over all known files, once per query change.
    pub fn take_content_request(&mut self) -> Option<(u64, Arc<Matcher>)> {
        if !std::mem::take(&mut self.content_pending) {
            return None;
        }
        self.content_search()
    }

    /// Merges search results; returns whether anything changed.
    pub fn add_content(&mut self, batch: ContentBatch) -> bool {
        if batch.generation != self.generation || self.mode != Mode::Content {
            return false;
        }
        if batch.done {
            self.searches_running = self.searches_running.saturating_sub(1);
            self.check_content_complete();
            return false;
        }
        let mut changed = false;
        for m in batch.matches {
            if self.content_matches.len() >= MAX_CONTENT_RESULTS {
                self.content_truncated = true;
                break;
            }
            let key = (m.file_idx, m.line);
            let pos = self.content_matches.partition_point(|x| (x.file_idx, x.line) < key);
            self.content_matches.insert(pos, m);
            changed = true;
        }
        self.tree_dirty |= changed;
        changed
    }

    /// What Enter would print: `path` or `path:line`, one per chosen entry.
    pub fn selected_output(&self) -> Option<String> {
        let chosen = self.chosen();
        (!chosen.is_empty()).then(|| chosen.iter().map(Target::to_string).collect::<Vec<_>>().join("\n"))
    }

    /// Keeps the selection inside the visible window of `height` rows and
    /// rebuilds the tree view if results changed.
    pub fn adjust_scroll(&mut self, height: usize) {
        self.page = height.max(1);
        let len = self.result_count();
        self.selected = self.selected.min(len.saturating_sub(1));
        if !self.tree_view {
            self.scroll_to(self.selected, len, height);
            return;
        }
        if self.tree_dirty {
            self.tree = match self.mode {
                Mode::Files => {
                    tree::build(&self.root, self.file_matches.iter().map(|m| (self.files[m.idx].as_ref(), false)))
                }
                Mode::Content => tree::build(&self.root, self.content_matches.iter().map(|m| (m.path.as_ref(), true))),
            };
            self.tree_dirty = false;
        }
        let row = self.tree.row_of_result.get(self.selected).copied().unwrap_or(0);
        let scrolling_up = row < self.offset;
        self.scroll_to(row, self.tree.rows.len(), height);
        if scrolling_up {
            // Also show the directory/file headers right above the selection.
            while self.offset > 0
                && self.tree.rows[self.offset - 1].result.is_none()
                && row + 2 - self.offset <= height
            {
                self.offset -= 1;
            }
        }
    }

    fn scroll_to(&mut self, row: usize, rows: usize, height: usize) {
        if row < self.offset {
            self.offset = row;
        } else if height > 0 && row >= self.offset + height {
            self.offset = row + 1 - height;
        }
        self.offset = self.offset.min(rows.saturating_sub(height));
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        // AltGr arrives as Ctrl+Alt on Windows, so Ctrl+Alt+char is text.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT);
        let page = self.page as isize;
        self.flash = None;
        match key.code {
            KeyCode::Esc => return Outcome::Quit,
            KeyCode::Enter => return self.selected_output().map_or(Outcome::Continue, Outcome::Accept),
            KeyCode::Tab | KeyCode::BackTab => self.toggle_mode(),
            KeyCode::Up if key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) => self.history_prev(),
            KeyCode::Down if key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) => self.history_next(),
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => self.scroll_preview(-3),
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => self.scroll_preview(3),
            KeyCode::PageUp if key.modifiers.contains(KeyModifiers::SHIFT) => self.scroll_preview(-page),
            KeyCode::PageDown if key.modifiers.contains(KeyModifiers::SHIFT) => self.scroll_preview(page),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-page),
            KeyCode::PageDown => self.move_selection(page),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.query.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.query.len(),
            KeyCode::Backspace if ctrl => self.delete_word(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
                'c' | 'g' | 'q' => return Outcome::Quit,
                'f' => self.toggle_mode(),
                't' => self.show_preview = !self.show_preview,
                'r' => self.set_regex(!self.regex),
                'l' => self.set_tree_view(!self.tree_view),
                's' | ' ' => self.toggle_mark(),
                'o' => {
                    let chosen = self.chosen();
                    if !chosen.is_empty() {
                        return Outcome::Open(chosen);
                    }
                }
                'y' => {
                    if let Some(text) = self.selected_output() {
                        return Outcome::Copy(text);
                    }
                }
                'p' | 'k' => self.move_selection(-1),
                'n' | 'j' => self.move_selection(1),
                'a' => self.cursor = 0,
                'e' => self.cursor = self.query.len(),
                'b' => self.cursor = self.cursor.saturating_sub(1),
                'h' => self.backspace(),
                'u' => self.clear_to_start(),
                'w' => self.delete_word(),
                _ => {}
            },
            KeyCode::Char(c) => self.insert(c),
            _ => {}
        }
        Outcome::Continue
    }

    /// Marks or unmarks the selected entry and moves to the next one.
    fn toggle_mark(&mut self) {
        let Some(target) = self.target_of(self.selected) else { return };
        match self.marked.iter().position(|t| *t == target) {
            Some(pos) => {
                self.marked.remove(pos);
            }
            None => self.marked.push(target),
        }
        self.move_selection(1);
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.result_count();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.select(self.selected.saturating_add_signed(delta).min(len - 1));
    }

    fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            Mode::Files => Mode::Content,
            Mode::Content => Mode::Files,
        };
        std::mem::swap(&mut self.query, &mut self.other_query.0);
        std::mem::swap(&mut self.cursor, &mut self.other_query.1);
        self.on_query_changed();
    }

    /// Recalls the previous (older) query from the history.
    fn history_prev(&mut self) {
        let pos = match self.history_pos {
            _ if self.history.is_empty() => return,
            None => {
                self.history_draft = self.query.clone();
                self.history.len() - 1
            }
            Some(p) => p.saturating_sub(1),
        };
        self.history_pos = Some(pos);
        self.replace_query(self.history[pos].chars().collect());
    }

    /// Goes forward in the history, back to the draft after the newest.
    fn history_next(&mut self) {
        let Some(pos) = self.history_pos else { return };
        if pos + 1 < self.history.len() {
            self.history_pos = Some(pos + 1);
            self.replace_query(self.history[pos + 1].chars().collect());
        } else {
            self.history_pos = None;
            let draft = std::mem::take(&mut self.history_draft);
            self.replace_query(draft);
        }
    }

    fn replace_query(&mut self, query: Vec<char>) {
        self.cursor = query.len();
        self.query = query;
        self.on_query_changed();
    }

    /// The user edited the query: leave history browsing.
    fn edited(&mut self) {
        self.history_pos = None;
        self.on_query_changed();
    }

    fn insert(&mut self, c: char) {
        self.query.insert(self.cursor, c);
        self.cursor += 1;
        self.edited();
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.query.remove(self.cursor);
            self.edited();
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.query.len() {
            self.query.remove(self.cursor);
            self.edited();
        }
    }

    fn clear_to_start(&mut self) {
        if self.cursor > 0 {
            self.query.drain(..self.cursor);
            self.cursor = 0;
            self.edited();
        }
    }

    /// Deletes the word before the cursor (and the whitespace after it).
    fn delete_word(&mut self) {
        let mut start = self.cursor;
        while start > 0 && self.query[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !self.query[start - 1].is_whitespace() {
            start -= 1;
        }
        if start < self.cursor {
            self.query.drain(start..self.cursor);
            self.cursor = start;
            self.edited();
        }
    }

    /// Files that can still match after a literal content query was
    /// extended (`foo` -> `foob`): those that matched `foo`, provided that
    /// search finished and was not cut off.
    fn narrowing_candidates(&self) -> Option<Vec<usize>> {
        if self.mode != Mode::Content || self.regex || self.content_truncated || !self.content_complete() {
            return None;
        }
        let (old, old_regex) = self.content_key.as_ref()?;
        let query = self.query();
        if *old_regex || old.trim().is_empty() || query == *old || !query.starts_with(old.as_str()) {
            return None;
        }
        let mut files: Vec<usize> = self.content_matches.iter().map(|m| m.file_idx).collect();
        files.dedup();
        Some(files)
    }

    fn on_query_changed(&mut self) {
        let narrow = self.narrowing_candidates();
        self.narrow_to = None;
        self.searches_running = 0;
        self.selected = 0;
        self.offset = 0;
        self.preview_scroll = 0;
        // Any change invalidates running content searches.
        self.generation += 1;
        self.content_matches.clear();
        self.content_truncated = false;
        self.matcher = None;
        self.query_error = None;
        self.tree_dirty = true;
        // File matches double as the content scope, so keep them current.
        let file_error = self.refilter_files().err();
        match self.mode {
            Mode::Files => self.query_error = file_error,
            Mode::Content => match Matcher::new(&self.query(), self.regex) {
                Ok(matcher) => {
                    self.content_key = matcher.as_ref().map(|_| (self.query(), self.regex));
                    self.narrow_to = narrow.filter(|_| matcher.is_some());
                    self.matcher = matcher.map(Arc::new);
                    self.content_pending = true;
                    self.query_error = file_error.map(|e| format!("{e} (in file scope)"));
                }
                Err(e) => self.query_error = Some(e),
            },
        }
    }

    /// Walk order in tree view or for unranked filters, best first otherwise.
    fn sort_file_view(&mut self) {
        let ranked = !self.tree_view && self.file_filter.as_ref().is_some_and(FileFilter::is_ranked);
        sort_file_matches(&mut self.file_matches, &self.files, ranked);
    }

    /// Recomputes file matches for the file-mode query; `Err` for an
    /// invalid regex (then nothing matches).
    fn refilter_files(&mut self) -> Result<(), String> {
        let key = (self.files_query(), self.regex);
        // `add_files` keeps the matches of a valid filter current.
        if key == self.file_key && self.file_filter.is_some() {
            return Ok(());
        }
        let filter = match FileFilter::new(&key.0, key.1) {
            Ok(filter) => filter,
            Err(e) => {
                self.file_filter = None;
                self.file_matches.clear();
                self.file_key = key;
                return Err(e);
            }
        };
        // Extending a fuzzy query usually only narrows the results, so only
        // previous matches need rescoring. (Not for regexes, `a` -> `a|b`, or
        // for exclusions and alternatives.)
        let (old_query, old_regex) = &self.file_key;
        let narrowing = !key.1
            && !old_regex
            && self.file_filter.is_some()
            && !old_query.trim().is_empty()
            && key.0.starts_with(old_query.as_str())
            && extension_narrows(&key.0);
        self.file_matches = if narrowing {
            let candidates: Vec<usize> = self.file_matches.iter().map(|m| m.idx).collect();
            rank_files(&self.files, &filter, candidates)
        } else {
            rank_files(&self.files, &filter, 0..self.files.len())
        };
        if filter.is_ranked() {
            apply_boosts(&self.boosts, &self.files, &mut self.file_matches);
        }
        self.file_filter = Some(filter);
        self.file_key = key;
        self.sort_file_view();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            assert_eq!(app.handle_key(key(KeyCode::Char(c))), Outcome::Continue);
        }
    }

    fn files(names: &[&str]) -> Vec<Arc<str>> {
        names.iter().map(|&n| Arc::from(n)).collect()
    }

    fn app_with(names: &[&str]) -> App {
        let mut app = App::new(Mode::Files, "");
        app.add_files(files(names));
        app
    }

    fn shown(app: &App) -> Vec<&str> {
        app.file_matches().iter().map(|m| app.files()[m.idx].as_ref()).collect()
    }

    fn cm(file_idx: usize, line: usize) -> ContentMatch {
        ContentMatch {
            file_idx,
            path: Arc::from(format!("f{file_idx}").as_str()),
            line,
            text: "text".into(),
            range: (0, 1),
        }
    }

    #[test]
    fn empty_query_lists_files_in_walk_order() {
        let app = app_with(&["b.rs", "a.rs", "c/d.rs"]);
        assert_eq!(shown(&app), vec!["b.rs", "a.rs", "c/d.rs"]);
    }

    #[test]
    fn typing_filters_and_ranks() {
        let mut app = app_with(&["docs/readme.md", "src/main.rs", "src/domain/model.rs", "main_test.go"]);
        type_str(&mut app, "main");
        assert_eq!(shown(&app), vec!["src/main.rs", "main_test.go", "src/domain/model.rs"]);
        type_str(&mut app, ".rs");
        assert_eq!(shown(&app), vec!["src/main.rs", "src/domain/model.rs"]);
    }

    #[test]
    fn narrowing_and_widening_give_same_results_as_fresh_search() {
        let names = ["abc", "axbxc", "ab", "bca", "a/b/c", "cab"];
        let mut app = app_with(&names);
        for q in ["a", "ab", "abc", "ab", "a", "", "c", "ca"] {
            // Drive the app to `q` by clearing and typing.
            app.handle_key(key(KeyCode::End));
            app.handle_key(ctrl('u'));
            type_str(&mut app, q);
            let fresh = rank_files(app.files(), &FileFilter::new(q, false).unwrap(), 0..names.len());
            assert_eq!(app.file_matches(), fresh.as_slice(), "query {q:?}");
        }
    }

    #[test]
    fn extended_syntax_queries_match_fresh_search_while_typing() {
        let names = ["src/main.rs", "src/test.rs", "tests/main.rs", "README.md", "a.md"];
        let mut app = app_with(&names);
        let mut typed = String::new();
        for c in "main !t | .md$".chars() {
            typed.push(c);
            type_str(&mut app, &c.to_string());
            let fresh = rank_files(app.files(), &FileFilter::new(&typed, false).unwrap(), 0..names.len());
            assert_eq!(app.file_matches(), fresh.as_slice(), "query {typed:?}");
        }
        assert_eq!(shown(&app), vec!["src/main.rs"]);
    }

    #[test]
    fn files_added_later_are_filtered_with_current_query() {
        let mut app = App::new(Mode::Files, "rs");
        app.add_files(files(&["a.txt", "b.rs"]));
        let base = app.add_files(files(&["c.rs", "d.md"]));
        assert_eq!(base, 2);
        assert_eq!(shown(&app), vec!["b.rs", "c.rs"]);
    }

    #[test]
    fn initial_query_is_applied() {
        let app = App::new(Mode::Files, "abc");
        assert_eq!(app.query(), "abc");
        assert_eq!(app.cursor(), 3);
    }

    #[test]
    fn cursor_editing() {
        let mut app = app_with(&[]);
        type_str(&mut app, "held");
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Left));
        type_str(&mut app, "l");
        assert_eq!(app.query(), "helld");
        assert_eq!(app.cursor(), 3);
        app.handle_key(key(KeyCode::Right));
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(app.query(), "hell");
        app.handle_key(key(KeyCode::End));
        type_str(&mut app, "o");
        assert_eq!(app.query(), "hello");
        app.handle_key(key(KeyCode::Home));
        app.handle_key(key(KeyCode::Backspace)); // no-op at start
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(app.query(), "ello");
        app.handle_key(ctrl('e'));
        assert_eq!(app.cursor(), 4);
        app.handle_key(ctrl('a'));
        assert_eq!(app.cursor(), 0);
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.cursor(), 0);
        for _ in 0..10 {
            app.handle_key(key(KeyCode::Right));
        }
        assert_eq!(app.cursor(), 4);
    }

    #[test]
    fn unicode_editing() {
        let mut app = app_with(&[]);
        type_str(&mut app, "çğış");
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.query(), "çğı");
        assert_eq!(app.cursor(), 3);
    }

    #[test]
    fn delete_word_and_clear() {
        let mut app = app_with(&[]);
        type_str(&mut app, "foo bar  ");
        app.handle_key(ctrl('w'));
        assert_eq!(app.query(), "foo ");
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL));
        assert_eq!(app.query(), "");
        type_str(&mut app, "abc def");
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Left));
        app.handle_key(ctrl('u'));
        assert_eq!(app.query(), "ef");
        assert_eq!(app.cursor(), 0);
    }

    #[test]
    fn altgr_characters_are_typed_not_treated_as_shortcuts() {
        // Turkish keyboard: AltGr+Q = '@' arrives as Ctrl+Alt+'@'.
        let mut app = app_with(&[]);
        app.handle_key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::CONTROL | KeyModifiers::ALT));
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL | KeyModifiers::ALT));
        assert_eq!(app.query(), "@q");
        // Shift is fine too.
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT));
        assert_eq!(app.query(), "@qA");
    }

    #[test]
    fn quit_keys() {
        let mut app = app_with(&["a"]);
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Outcome::Quit);
        assert_eq!(app.handle_key(ctrl('c')), Outcome::Quit);
        assert_eq!(app.handle_key(ctrl('g')), Outcome::Quit);
        assert_eq!(app.handle_key(ctrl('q')), Outcome::Quit);
    }

    #[test]
    fn enter_accepts_selected_file() {
        let mut app = app_with(&["one.txt", "two.txt", "three.txt"]);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Outcome::Accept("two.txt".into()));
    }

    #[test]
    fn enter_with_no_results_does_nothing() {
        let mut app = app_with(&["a.txt"]);
        type_str(&mut app, "zzz");
        assert_eq!(app.result_count(), 0);
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Outcome::Continue);
    }

    #[test]
    fn selection_is_clamped_and_reset_on_query_change() {
        let mut app = app_with(&["a1", "a2", "a3"]);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.selected(), 0);
        for _ in 0..10 {
            app.handle_key(ctrl('n'));
        }
        assert_eq!(app.selected(), 2);
        app.handle_key(ctrl('p'));
        assert_eq!(app.selected(), 1);
        type_str(&mut app, "a");
        assert_eq!(app.selected(), 0);
    }

    #[test]
    fn paging_and_scrolling() {
        let names: Vec<String> = (0..100).map(|i| format!("f{i:03}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut app = app_with(&refs);
        app.adjust_scroll(10);
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.selected(), 10);
        app.adjust_scroll(10);
        assert_eq!(app.offset(), 1);
        for _ in 0..20 {
            app.handle_key(key(KeyCode::PageDown));
        }
        assert_eq!(app.selected(), 99);
        app.adjust_scroll(10);
        assert_eq!(app.offset(), 90);
        app.handle_key(key(KeyCode::PageUp));
        app.adjust_scroll(10);
        assert_eq!((app.selected(), app.offset()), (89, 89));
    }

    #[test]
    fn scroll_offset_shrinks_with_results() {
        let names: Vec<String> = (0..50).map(|i| format!("f{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut app = app_with(&refs);
        for _ in 0..40 {
            app.handle_key(key(KeyCode::Down));
        }
        app.adjust_scroll(10);
        assert_eq!(app.offset(), 31);
        type_str(&mut app, "f0");
        app.adjust_scroll(10);
        assert_eq!((app.selected(), app.offset()), (0, 0));
    }

    #[test]
    fn toggle_mode_with_tab_and_ctrl_f() {
        let mut app = app_with(&["a"]);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.mode(), Mode::Content);
        app.handle_key(ctrl('f'));
        assert_eq!(app.mode(), Mode::Files);
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.mode(), Mode::Content);
    }

    /// Generation of a request and whether its matcher finds `probe`.
    fn req(r: Option<(u64, Arc<Matcher>)>, probe: &str) -> Option<(u64, bool)> {
        r.map(|(g, m)| (g, m.find(probe).is_some()))
    }

    #[test]
    fn content_request_issued_once_per_change() {
        let mut app = App::new(Mode::Content, "");
        assert!(app.take_content_request().is_none(), "blank query never searches");
        type_str(&mut app, "fo");
        let (g1, found) = req(app.take_content_request(), "xfoy").unwrap();
        assert!(found);
        assert!(app.take_content_request().is_none());
        type_str(&mut app, "o");
        let (g2, found) = req(app.take_content_request(), "fo").unwrap();
        assert!(!found, "matcher is for the new query");
        assert!(g2 > g1);
        assert_eq!(req(app.content_search(), "foo"), Some((g2, true)));
    }

    #[test]
    fn content_mode_from_start_requests_search() {
        let mut app = App::new(Mode::Content, "needle");
        assert_eq!(req(app.take_content_request(), "a needle").map(|r| r.1), Some(true));
    }

    #[test]
    fn no_content_search_in_files_mode() {
        let mut app = App::new(Mode::Files, "x");
        assert!(app.take_content_request().is_none());
        assert!(app.content_search().is_none());
    }

    #[test]
    fn regex_toggle_in_content_mode() {
        let mut app = App::new(Mode::Content, "a.c");
        assert_eq!(req(app.take_content_request(), "abc").map(|r| r.1), Some(false), "literal dot");
        app.handle_key(ctrl('r'));
        assert!(app.regex());
        assert_eq!(req(app.take_content_request(), "abc").map(|r| r.1), Some(true), "regex dot");
        app.handle_key(ctrl('r'));
        assert!(!app.regex());
    }

    #[test]
    fn invalid_regex_sets_error_and_stops_search() {
        let mut app = App::new(Mode::Content, "foo(");
        app.set_regex(true);
        assert!(app.query_error().unwrap().contains("unclosed group"));
        assert!(app.take_content_request().is_none());
        assert!(app.matcher().is_none());
        type_str(&mut app, ")");
        assert_eq!(app.query_error(), None);
        assert!(app.take_content_request().is_some());
    }

    #[test]
    fn regex_file_filter_keeps_walk_order_and_highlights() {
        let mut app = app_with(&[r"src\main.rs", "docs/a.md", "src/lib.rs", "test.rs"]);
        app.set_regex(true);
        type_str(&mut app, r"^src/.*\.rs$");
        assert_eq!(shown(&app), vec![r"src\main.rs", "src/lib.rs"], "backslashes match `/`");
        assert_eq!(app.file_matches()[1].positions, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn invalid_regex_in_file_mode_shows_nothing_until_fixed() {
        let mut app = app_with(&["a(b", "ab"]);
        app.set_regex(true);
        type_str(&mut app, "a(");
        assert!(app.query_error().is_some());
        assert_eq!(app.result_count(), 0);
        app.add_files(files(&["a(c"]));
        assert_eq!(app.result_count(), 0);
        app.set_regex(false);
        assert_eq!(app.query_error(), None);
        assert_eq!(shown(&app), vec!["a(b", "a(c"]);
    }

    #[test]
    fn regex_query_is_never_narrowed_incrementally() {
        let mut app = app_with(&["a", "b", "c"]);
        app.set_regex(true);
        type_str(&mut app, "a");
        assert_eq!(shown(&app), vec!["a"]);
        type_str(&mut app, "|b");
        assert_eq!(shown(&app), vec!["a", "b"]);
    }

    #[test]
    fn tree_view_uses_walk_order_and_restores_ranking() {
        let mut app = app_with(&["src/zmain.rs", "main.rs", "x/main_test.rs"]);
        type_str(&mut app, "main");
        let ranked = shown(&app).into_iter().map(String::from).collect::<Vec<_>>();
        assert_eq!(ranked[0], "main.rs");
        app.handle_key(ctrl('l'));
        assert!(app.tree_view());
        assert_eq!(shown(&app), vec!["src/zmain.rs", "main.rs", "x/main_test.rs"]);
        app.adjust_scroll(10);
        let labels: Vec<_> = app.tree().rows.iter().map(|r| format!("{}{}", r.guide, r.label)).collect();
        assert_eq!(labels, vec!["src/", "└─ zmain.rs", "main.rs", "x/", "└─ main_test.rs"]);
        app.handle_key(ctrl('l'));
        assert_eq!(shown(&app), ranked);
    }

    #[test]
    fn tree_scroll_tracks_selected_row_and_shows_headers() {
        let names: Vec<String> = (0..10).map(|i| format!("d{i}/f.rs")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut app = app_with(&refs);
        app.set_tree_view(true);
        app.adjust_scroll(4);
        assert_eq!(app.tree().rows.len(), 20);
        for _ in 0..5 {
            app.handle_key(key(KeyCode::Down));
        }
        app.adjust_scroll(4);
        // Result 5 is row 11; it must be the last visible row.
        assert_eq!(app.offset(), 8);
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Up));
        app.adjust_scroll(4);
        // Result 2 is row 5; its directory header (row 4) is shown too.
        assert_eq!(app.offset(), 4);
    }

    #[test]
    fn tree_rebuilds_when_results_change() {
        let mut app = App::new(Mode::Content, "x");
        app.set_tree_view(true);
        let g = app.generation();
        app.add_content(ContentBatch { done: false, generation: g, matches: vec![cm(0, 1)] });
        app.adjust_scroll(10);
        assert_eq!(app.tree().rows.len(), 2);
        app.add_content(ContentBatch { done: false, generation: g, matches: vec![cm(1, 1)] });
        app.adjust_scroll(10);
        assert_eq!(app.tree().rows.len(), 4);
    }

    #[test]
    fn content_results_are_sorted_and_stale_ones_dropped() {
        let mut app = App::new(Mode::Content, "x");
        let g = app.generation();
        assert!(app.add_content(ContentBatch { done: false, generation: g, matches: vec![cm(3, 1)] }));
        assert!(app.add_content(ContentBatch { done: false, generation: g, matches: vec![cm(1, 5), cm(1, 2)] }));
        assert!(!app.add_content(ContentBatch { done: false, generation: g - 1, matches: vec![cm(0, 1)] }));
        let got: Vec<_> = app.content_matches().iter().map(|m| (m.file_idx, m.line)).collect();
        assert_eq!(got, vec![(1, 2), (1, 5), (3, 1)]);
        assert_eq!(app.selected_output(), Some("f1:2".into()));

        type_str(&mut app, "y");
        assert!(app.content_matches().is_empty());
        assert!(!app.add_content(ContentBatch { done: false, generation: g, matches: vec![cm(0, 1)] }));
    }

    #[test]
    fn content_results_ignored_after_switching_to_files() {
        let mut app = App::new(Mode::Content, "x");
        let g = app.generation();
        app.handle_key(key(KeyCode::Tab));
        assert!(!app.add_content(ContentBatch { done: false, generation: g, matches: vec![cm(0, 1)] }));
        assert!(app.content_matches().is_empty());
    }

    #[test]
    fn content_results_are_capped() {
        let mut app = App::new(Mode::Content, "x");
        let g = app.generation();
        let many: Vec<_> = (0..MAX_CONTENT_RESULTS + 5).map(|i| cm(i, 1)).collect();
        app.add_content(ContentBatch { done: false, generation: g, matches: many });
        assert_eq!(app.content_matches().len(), MAX_CONTENT_RESULTS);
        assert!(app.content_truncated());
    }

    #[test]
    fn enter_in_content_mode_prints_path_and_line() {
        let mut app = App::new(Mode::Content, "x");
        let g = app.generation();
        app.add_content(ContentBatch { done: false, generation: g, matches: vec![cm(0, 7), cm(2, 3)] });
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Outcome::Accept("f2:3".into()));
    }

    #[test]
    fn each_mode_keeps_its_own_query() {
        let mut app = app_with(&["alpha.rs", "beta.rs"]);
        type_str(&mut app, "bet");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.query(), "", "content mode starts empty");
        type_str(&mut app, "todo");
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Tab));
        assert_eq!((app.query().as_str(), app.cursor()), ("bet", 3));
        assert_eq!(shown(&app), vec!["beta.rs"]);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!((app.query().as_str(), app.cursor()), ("todo", 3));
    }

    fn targets(app: &App, from: usize) -> Vec<usize> {
        app.content_targets(from).into_iter().map(|t| t.0).collect()
    }

    #[test]
    fn content_search_is_scoped_by_file_query() {
        let mut app = app_with(&["a.rs", "b.md", "c.rs", "d.txt"]);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.content_scope(), None);
        assert_eq!(targets(&app, 0), vec![0, 1, 2, 3]);
        assert_eq!(targets(&app, 2), vec![2, 3]);
        assert_eq!(app.content_target_count(), 4);

        app.handle_key(key(KeyCode::Tab));
        type_str(&mut app, ".rs$");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.content_scope(), Some(".rs$".into()));
        assert_eq!(targets(&app, 0), vec![0, 2]);
        assert_eq!(app.content_target_count(), 2);

        // Files found later are filtered by the scope too.
        let base = app.add_files(files(&["e.rs", "f.md"]));
        assert_eq!(targets(&app, base), vec![4]);
    }

    #[test]
    fn regex_toggle_in_content_mode_updates_scope() {
        let mut app = app_with(&["a.rs", "ab.rs", "b.rs"]);
        type_str(&mut app, "^a");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(targets(&app, 0), vec![0, 1]);
        app.set_regex(true);
        assert_eq!(targets(&app, 0), vec![0, 1]);
        type_str(&mut app, "x");
        // The scope is a file query; an invalid one is reported.
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::End));
        type_str(&mut app, "(");
        app.handle_key(key(KeyCode::Tab));
        assert!(app.query_error().unwrap().contains("in file scope"));
        assert!(targets(&app, 0).is_empty());
    }

    #[test]
    fn ctrl_t_toggles_preview() {
        let mut app = app_with(&["a"]);
        assert!(app.show_preview());
        assert_eq!(app.handle_key(ctrl('t')), Outcome::Continue);
        assert!(!app.show_preview());
        app.handle_key(ctrl('t'));
        assert!(app.show_preview());
        assert_eq!(app.query(), "", "toggle must not type anything");
    }

    #[test]
    fn preview_target_follows_selection() {
        let mut app = app_with(&["a.txt", "b.txt"]);
        assert_eq!(app.preview_target(), Some((Arc::from("a.txt"), None)));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.preview_target(), Some((Arc::from("b.txt"), None)));
        type_str(&mut app, "zzz");
        assert_eq!(app.preview_target(), None);

        let mut app = App::new(Mode::Content, "x");
        let g = app.generation();
        app.add_content(ContentBatch { done: false, generation: g, matches: vec![cm(0, 7), cm(1, 3)] });
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.preview_target(), Some((Arc::from("f1"), Some(3))));
    }

    fn t(path: &str, line: Option<usize>) -> Target {
        Target { path: path.into(), line }
    }

    #[test]
    fn marking_and_accepting_several_entries() {
        let mut app = app_with(&["a", "b", "c"]);
        app.handle_key(ctrl('s'));
        assert_eq!(app.selected(), 1, "marking moves down");
        app.handle_key(key(KeyCode::Down));
        app.handle_key(ctrl(' '));
        assert!(app.is_marked(0) && !app.is_marked(1) && app.is_marked(2));
        assert_eq!(app.marked_count(), 2);
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Outcome::Accept("a\nc".into()));
    }

    #[test]
    fn marks_toggle_and_survive_query_changes() {
        let mut app = app_with(&["apple", "banana"]);
        app.handle_key(ctrl('s'));
        type_str(&mut app, "ban");
        assert!(!app.is_marked(0), "banana is not marked");
        app.handle_key(ctrl('s'));
        assert_eq!(app.chosen(), vec![t("apple", None), t("banana", None)]);
        app.handle_key(ctrl('u'));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(ctrl('s'));
        assert_eq!(app.chosen(), vec![t("apple", None)], "second toggle unmarks");
    }

    #[test]
    fn content_marks_include_line() {
        let mut app = App::new(Mode::Content, "x");
        let g = app.generation();
        app.add_content(ContentBatch { done: false, generation: g, matches: vec![cm(0, 7), cm(1, 2)] });
        app.handle_key(ctrl('s'));
        app.handle_key(ctrl('s'));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Outcome::Accept("f0:7\nf1:2".into()));
    }

    #[test]
    fn open_and_copy_outcomes() {
        let mut app = app_with(&["a.rs", "b.rs"]);
        assert_eq!(app.handle_key(ctrl('o')), Outcome::Open(vec![t("a.rs", None)]));
        assert_eq!(app.handle_key(ctrl('y')), Outcome::Copy("a.rs".into()));
        app.handle_key(ctrl('s'));
        app.handle_key(ctrl('s'));
        assert_eq!(app.handle_key(ctrl('o')), Outcome::Open(vec![t("a.rs", None), t("b.rs", None)]));
        assert_eq!(app.handle_key(ctrl('y')), Outcome::Copy("a.rs\nb.rs".into()));

        let mut empty = app_with(&[]);
        assert_eq!(empty.handle_key(ctrl('o')), Outcome::Continue);
        assert_eq!(empty.handle_key(ctrl('y')), Outcome::Continue);
    }

    #[test]
    fn flash_message_clears_on_next_key() {
        let mut app = app_with(&["a"]);
        app.set_flash("copied");
        assert_eq!(app.flash(), Some("copied"));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.flash(), None);
    }

    #[test]
    fn shift_arrows_scroll_preview_and_selection_resets_it() {
        let mut app = app_with(&["a", "b"]);
        app.adjust_scroll(10);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
        assert_eq!(app.preview_scroll(), 6);
        assert_eq!(app.selected(), 0, "shift+arrow does not move the selection");
        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT));
        assert_eq!(app.preview_scroll(), -4);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.preview_scroll(), 0);
    }

    #[test]
    fn mouse_wheel_and_click() {
        let names: Vec<String> = (0..20).map(|i| format!("f{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut app = app_with(&refs);
        app.adjust_scroll(5);
        app.handle_mouse(MouseEventKind::ScrollDown, Hit::List(0));
        app.handle_mouse(MouseEventKind::ScrollDown, Hit::Other);
        assert_eq!(app.selected(), 2);
        app.handle_mouse(MouseEventKind::ScrollDown, Hit::Preview);
        assert_eq!((app.selected(), app.preview_scroll()), (2, 3));
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), Hit::List(4));
        assert_eq!((app.selected(), app.preview_scroll()), (4, 0));
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), Hit::Preview);
        assert_eq!(app.selected(), 4);
    }

    #[test]
    fn click_in_tree_view_ignores_header_rows() {
        let mut app = app_with(&["d/a", "d/b"]);
        app.set_tree_view(true);
        app.adjust_scroll(5);
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), Hit::List(2));
        assert_eq!(app.selected(), 1);
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), Hit::List(0));
        assert_eq!(app.selected(), 1, "directory row is not selectable");
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), Hit::List(9));
        assert_eq!(app.selected(), 1, "below the last row");
    }

    #[test]
    fn query_history_navigation() {
        let mut app = app_with(&[]);
        app.set_history(vec!["old".into(), "new".into()]);
        type_str(&mut app, "draft");
        let alt = |code| KeyEvent::new(code, KeyModifiers::ALT);
        app.handle_key(alt(KeyCode::Up));
        assert_eq!(app.query(), "new");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL));
        assert_eq!(app.query(), "old");
        app.handle_key(alt(KeyCode::Up));
        assert_eq!(app.query(), "old", "stays at the oldest");
        app.handle_key(alt(KeyCode::Down));
        assert_eq!(app.query(), "new");
        app.handle_key(alt(KeyCode::Down));
        assert_eq!((app.query().as_str(), app.cursor()), ("draft", 5), "back to the draft");
        app.handle_key(alt(KeyCode::Down));
        assert_eq!(app.query(), "draft");
    }

    #[test]
    fn editing_a_recalled_query_leaves_history() {
        let mut app = app_with(&[]);
        app.set_history(vec!["a".into(), "b".into()]);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        type_str(&mut app, "x");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        assert_eq!(app.query(), "b", "starts again from the newest");
        let mut empty = app_with(&[]);
        empty.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        assert_eq!(empty.query(), "");
    }

    #[test]
    fn boosts_reorder_fuzzy_results_only() {
        let mut app = app_with(&["src/main.rs", "old/main.rs"]);
        type_str(&mut app, "main");
        assert_eq!(shown(&app)[0], "src/main.rs");
        app.set_boosts(HashMap::from([("old/main.rs".to_string(), 40)]));
        assert_eq!(shown(&app), vec!["old/main.rs", "src/main.rs"]);
        // Boosts also apply to files found later.
        app.add_files(files(&["new/main.rs"]));
        assert_eq!(shown(&app)[0], "old/main.rs");
        // An empty query keeps walk order.
        app.handle_key(ctrl('u'));
        assert_eq!(shown(&app), vec!["src/main.rs", "old/main.rs", "new/main.rs"]);
    }

    fn done(app: &App) -> ContentBatch {
        ContentBatch { done: true, generation: app.generation(), matches: Vec::new() }
    }

    #[test]
    fn extending_a_complete_literal_search_narrows_files() {
        let mut app = app_with(&["a", "b", "c", "d"]);
        app.finish_walk();
        app.handle_key(key(KeyCode::Tab));
        type_str(&mut app, "fo");
        app.take_content_request();
        app.search_started();
        let g = app.generation();
        app.add_content(ContentBatch { done: false, generation: g, matches: vec![cm(1, 1), cm(1, 4), cm(3, 2)] });
        assert!(!app.content_complete());
        app.add_content(done(&app));
        assert!(app.content_complete());

        type_str(&mut app, "o");
        assert_eq!(targets(&app, 0), vec![1, 3]);
        // Shortening the query searches everything again.
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(targets(&app, 0), vec![0, 1, 2, 3]);
    }

    #[test]
    fn no_narrowing_when_search_incomplete_truncated_or_regex() {
        let mut app = app_with(&["a", "b"]);
        app.finish_walk();
        app.handle_key(key(KeyCode::Tab));
        type_str(&mut app, "fo");
        app.search_started();
        app.search_started();
        app.add_content(done(&app));
        assert!(!app.content_complete(), "one of two searches still running");
        type_str(&mut app, "o");
        assert_eq!(targets(&app, 0), vec![0, 1]);

        app.search_started();
        app.add_content(done(&app));
        assert!(app.content_complete());
        app.set_regex(true);
        assert!(!app.content_complete(), "regex change starts a new search");
        app.search_started();
        app.add_content(done(&app));
        type_str(&mut app, "x");
        assert_eq!(targets(&app, 0), vec![0, 1], "regexes are never narrowed");
    }

    #[test]
    fn content_completes_when_walk_ends_last() {
        let mut app = app_with(&["a"]);
        app.handle_key(key(KeyCode::Tab));
        type_str(&mut app, "x");
        app.search_started();
        app.add_content(done(&app));
        assert!(!app.content_complete(), "files may still be coming");
        app.finish_walk();
        assert!(app.content_complete());
    }

    #[test]
    fn walk_state() {
        let mut app = app_with(&[]);
        assert!(app.is_walking());
        app.finish_walk();
        assert!(!app.is_walking());
    }
}
