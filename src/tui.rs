//! The interactive event loop: wires the walker, the content searcher and
//! the terminal to `App`. The UI is drawn on stderr so stdout stays clean for
//! the selected result (e.g. `code $(ff)`).

use std::io::{self, BufWriter, Stderr};
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;

use crate::app::{App, Mode, Outcome, MAX_CONTENT_RESULTS};
use crate::clipboard;
use crate::cli::Config;
use crate::content::ContentSearcher;
use crate::history::{self, History};
use crate::preview::{self, Body, Preview};
use crate::ui;
use crate::walk::{self, WalkMsg, WalkOptions};

const TICK: Duration = Duration::from_millis(16);

/// Restores the terminal even if the loop errors or panics.
struct TerminalGuard;

impl TerminalGuard {
    fn enter(mouse: bool) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = TerminalGuard;
        execute!(io::stderr(), EnterAlternateScreen)?;
        if mouse {
            execute!(io::stderr(), EnableMouseCapture)?;
        }
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stderr(), DisableMouseCapture, Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

/// Loads the preview for the current selection unless it is already loaded.
/// Only the visible lines are read, so this is cheap enough to do inline.
fn refresh_preview(preview: &mut Option<Preview>, app: &mut App, rows: Option<usize>) {
    let (Some(rows), Some((path, focus))) = (rows, app.preview_target()) else {
        *preview = None;
        return;
    };
    let default_start = preview::window_start(focus, rows) as isize;
    let start = (default_start + app.preview_scroll()).max(1) as usize;
    if !preview.as_ref().is_some_and(|p| p.is_for(&path, focus, start, rows)) {
        let loaded = preview::load(&path, focus, start, rows);
        // Keep the scroll in sync when the window was clamped at either end.
        if let Body::Text { first_line, .. } = loaded.body {
            app.set_preview_scroll(first_line as isize - default_start);
        }
        *preview = Some(loaded);
    }
}

/// Saves the query and the picked files; failures are not worth reporting.
fn remember(history: &mut History, app: &App) {
    history.add_query(&app.query());
    let now = history::now();
    for target in app.chosen() {
        if let Some(abs) = history::absolute(&target.path) {
            history.record_pick(&abs, now);
        }
    }
    let _ = history.save();
}

/// Runs the finder until the user accepts (`Accept`), cancels (`Quit`) or
/// asks to open entries in the editor (`Open`). The terminal is restored
/// before this returns.
pub fn run(cfg: &Config) -> io::Result<Outcome> {
    let mode = if cfg.content { Mode::Content } else { Mode::Files };
    let mut app = App::new(mode, &cfg.query);
    if let Some(scope) = &cfg.scope {
        app.set_files_query(scope);
    }
    app.set_regex(cfg.regex);
    app.set_tree_view(cfg.tree);
    app.set_root(&cfg.root.to_string_lossy());
    let walk_rx = walk::spawn(cfg.root.clone(), WalkOptions { hidden: cfg.hidden, no_ignore: cfg.no_ignore });
    let (searcher, content_rx) = ContentSearcher::new();

    let mut history = if cfg.no_history { History::default() } else { History::load(history::default_path()) };
    app.set_history(history.queries().to_vec());
    app.set_boosts(history.boosts_under(&cfg.root, history::now()));

    let _guard = TerminalGuard::enter(!cfg.no_mouse)?;
    let mut terminal: Terminal<CrosstermBackend<BufWriter<Stderr>>> =
        Terminal::new(CrosstermBackend::new(BufWriter::new(io::stderr())))?;
    let mut dirty = true;
    let mut preview: Option<Preview> = None;

    loop {
        searcher.set_generation(app.generation());

        // A query change searches every file known so far...
        if let Some((generation, matcher)) = app.take_content_request() {
            app.search_started();
            searcher.search(generation, matcher, app.content_targets(0), MAX_CONTENT_RESULTS);
        }

        // ...and files discovered afterwards are searched as they arrive.
        loop {
            match walk_rx.try_recv() {
                Ok(WalkMsg::Batch(batch)) => {
                    let base = app.add_files(batch);
                    if let Some((generation, matcher)) = app.content_search() {
                        app.search_started();
                        searcher.search(generation, matcher, app.content_targets(base), MAX_CONTENT_RESULTS);
                    }
                    dirty = true;
                }
                Ok(WalkMsg::Done) | Err(TryRecvError::Disconnected) => {
                    if app.is_walking() {
                        app.finish_walk();
                        dirty = true;
                    }
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }

        while let Ok(batch) = content_rx.try_recv() {
            dirty |= app.add_content(batch);
        }

        if dirty {
            let (w, h) = terminal::size()?;
            // Make sure the selection is on screen before picking what to preview.
            app.adjust_scroll(ui::list_height(h));
            let rows = ui::preview_rows(&app, w, h);
            refresh_preview(&mut preview, &mut app, rows);
            terminal.draw(|frame| ui::render(&mut app, preview.as_ref(), frame))?;
            dirty = false;
        }

        if event::poll(TICK)? {
            // Handle everything that is queued (fast typing, pastes) before
            // redrawing or starting a search.
            loop {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        dirty = true;
                        match app.handle_key(key) {
                            Outcome::Continue => {}
                            Outcome::Copy(text) => match clipboard::copy(&text) {
                                Ok(how) => app.set_flash(format!("copied via {how}: {}", text.replace('\n', " "))),
                                Err(e) => app.set_flash(format!("copy failed: {e}")),
                            },
                            done => {
                                if matches!(done, Outcome::Accept(_) | Outcome::Open(_)) && !cfg.no_history {
                                    remember(&mut history, &app);
                                }
                                return Ok(done);
                            }
                        }
                    }
                    Event::Mouse(m) => {
                        let (w, h) = terminal::size()?;
                        let hit = ui::hit_test(&app, w, h, m.column, m.row);
                        app.handle_mouse(m.kind, hit);
                        dirty = true;
                    }
                    Event::Resize(..) => dirty = true,
                    _ => {}
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
}
