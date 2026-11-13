//! ghreviewer in a terminal: the inbox and the PR, over the same core as the desktop app.

mod pr;
mod run;

use ghreviewer_core::{
    self as backend, AppState, Event, FIX, Status, Target,
    agent::Finding,
    agent::{Run, RunStatus},
    config::Prompt,
    err,
    inbox::{
        BotReview, PrRow, bot_reviews, by_next, can_approve, can_wait, matches_filter, parse_ms,
        picked_by_default, rel_time, stack_leads, stack_rows,
    },
    local::Worktree,
    sync::PrSummary,
};
use pr::{ACCENT, DIM, FAIL, OK, WARN, check_glyph, review_label};
use ratatui::{
    Frame,
    crossterm::event::{self, Event as Input, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::Value;
use std::{
    collections::HashMap,
    io::Write,
    sync::{Arc, mpsc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

enum Msg {
    Input(Input),
    Core(Event),
    Detail(String, Result<Value, String>),
    /// The worktree of a PR (by id).
    Worktree(String, Result<Worktree, String>),
    /// An action finished: what to say when it worked.
    Done(String, Result<(), String>),
    Tick,
}

#[derive(PartialEq)]
enum Mode {
    Normal,
    Filter,
    Snooze,
    /// Typing the approval's message.
    Approve,
    /// Asking before posting the tab's review: approving or commenting.
    Post {
        approve: bool,
    },
    /// Picking an `open_with` entry for the worktree.
    Opener,
    Help,
}

const HELP: &[(&str, &str)] = &[
    (
        "j / k, gg / G",
        "next / previous (findings in a prompt tab), top / bottom",
    ),
    ("Tab, h / l, Enter", "inbox / PR pane"),
    ("Ctrl-d / Ctrl-u", "scroll the PR pane half a page"),
    ("[ / ]", "previous / next tab: the PR, then each prompt"),
    ("/", "filter the inbox; Esc clears it"),
    ("t", "next rule (all, then each rule)"),
    ("o", "open the PR in the browser (over SSH: copy its link)"),
    ("u", "toggle unread"),
    (
        "z",
        "snooze under \"can wait\" for 1, 3 or 7 days, or wake it",
    ),
    (
        "a",
        "approve, with an optional message (PR tab); post approving (prompt tab)",
    ),
    (
        "w / W",
        "open the worktree (first open_with / pick one), or create it",
    ),
    (
        "r",
        "run or re-run the tab's prompt (creates the worktree first)",
    ),
    ("X", "cancel the tab's run"),
    ("c", "continue the tab's agent session in a terminal"),
    ("Space", "pick a finding"),
    ("p", "post the picked findings as a review"),
    ("f", "send the picked findings to the [fix] agent"),
    ("Ctrl-r", "sync now"),
    ("q, Ctrl-c", "quit"),
];

struct App {
    st: Arc<AppState>,
    rt: tokio::runtime::Handle,
    tx: mpsc::Sender<Msg>,
    status: Status,
    smart: bool,
    rules: Vec<String>,
    /// Rule the inbox shows, empty for all.
    rule: String,
    filter: String,
    mode: Mode,
    /// Newest first, or ranked by `by_next` in the smart inbox.
    inbox: Vec<PrSummary>,
    rows: Vec<PrRow>,
    leads: Vec<Option<usize>>,
    /// Row where "can wait" starts (smart inbox).
    wait_at: Option<usize>,
    sel: usize,
    /// The PR you moved to: kept across syncs, and marked read.
    picked: Option<String>,
    pr_focus: bool,
    /// What the shown detail was fetched for: id, updatedAt, checks.
    detail_key: Option<(String, String, Option<String>)>,
    detail: Option<(String, Value)>,
    /// `detail` rendered at a width.
    lines: Option<(u16, Vec<Line<'static>>)>,
    scroll: u16,
    pane_height: u16,
    pending_g: bool,
    flash: Option<(String, Color)>,
    /// Tab of the PR pane: "" for the PR, or a prompt name. Kept across PRs.
    tab_name: String,
    /// Latest run of each prompt on the selected PR.
    runs: Vec<Run>,
    wt: Option<Worktree>,
    creating_wt: bool,
    /// Picked findings by run (`review_key`), until you quit.
    picks: HashMap<String, Vec<bool>>,
    cursor: usize,
    /// Scroll the findings cursor into view on the next draw.
    follow: bool,
    /// Lines in the PR pane, to stop scrolling past them.
    pane_lines: usize,
    /// The approval's message being typed.
    input: String,
}

fn review_key(run: &Run) -> String {
    format!("{}/{}/{}", run.pr_id, run.prompt, run.started_at)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn next_color(next: &str) -> Color {
    match next {
        "back" | "updated" => ACCENT,
        "feedback" => WARN,
        "failing" => FAIL,
        "ready" => OK,
        "waiting" | "done" => DIM,
        _ => Color::Reset,
    }
}

/// `s` cut to `n` columns, with an ellipsis when cut.
fn fit(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// `left` then `right` against the far edge of `width`; `left` gets cut when they don't fit.
fn spread(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let right_w: usize = right.iter().map(Span::width).sum();
    let mut room = width.saturating_sub(right_w + 1);
    let mut spans = Vec::new();
    for s in left {
        if room == 0 {
            break;
        }
        let w = s.width();
        let content = if w > room {
            fit(&s.content, room)
        } else {
            s.content.to_string()
        };
        room = room.saturating_sub(w);
        spans.push(Span::styled(content, s.style));
    }
    let used: usize = spans.iter().map(Span::width).sum();
    spans.push(Span::raw(" ".repeat(width.saturating_sub(used + right_w))));
    spans.extend(right);
    Line::from(spans)
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() {
                T[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

impl App {
    fn selected(&self) -> Option<&PrSummary> {
        self.rows.get(self.sel).map(|r| &self.inbox[r.pr])
    }

    fn say(&mut self, res: anyhow::Result<()>) {
        if let Err(e) = res {
            self.flash = Some((err(e), FAIL));
        }
    }

    /// Re-reads the inbox and settings after a sync or a change.
    fn reload(&mut self) {
        let cfg = self.st.cfg.read().unwrap().clone();
        self.smart = cfg.smart_inbox;
        self.rules = cfg.rules.iter().map(|r| r.name.clone()).collect();
        if !self.rules.contains(&self.rule) {
            self.rule.clear(); // a renamed or removed rule falls back to all
        }
        self.status = self.st.status.lock().unwrap().clone();
        match backend::list_inbox(&self.st) {
            Ok(inbox) => self.inbox = inbox,
            Err(e) => self.flash = Some((err(e), FAIL)),
        }
        if self.smart {
            by_next(&mut self.inbox);
        }
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let (rule, filter) = (&self.rule, &self.filter);
        self.rows = stack_rows(&self.inbox, |p| {
            !p.context && (rule.is_empty() || p.rules.contains(rule)) && matches_filter(p, filter)
        });
        self.leads = if self.smart {
            stack_leads(&self.inbox, &self.rows)
        } else {
            Vec::new()
        };
        self.wait_at = self
            .leads
            .iter()
            .position(|l| l.is_some_and(|i| can_wait(&self.inbox[i])));
        let picked = self.picked.as_ref();
        let at = picked.and_then(|id| self.rows.iter().position(|r| &self.inbox[r.pr].id == id));
        self.sel = at.unwrap_or(0).min(self.rows.len().saturating_sub(1));
        self.load_detail();
    }

    /// Fetches the selected PR when it's another one, or the inbox says it changed.
    fn load_detail(&mut self) {
        let Some(pr) = self.selected() else {
            (self.detail, self.detail_key, self.lines) = (None, None, None);
            return;
        };
        let key = (pr.id.clone(), pr.updated_at.clone(), pr.checks.clone());
        if self.detail_key.as_ref() == Some(&key) {
            return;
        }
        if self.detail.as_ref().is_none_or(|(id, _)| *id != key.0) {
            (self.detail, self.lines, self.scroll) = (None, None, 0);
            (self.wt, self.cursor) = (None, 0);
            self.runs = self.st.db.runs(&key.0).unwrap_or_default();
        }
        let (st, tx, id) = (self.st.clone(), self.tx.clone(), key.0.clone());
        self.detail_key = Some(key);
        self.rt.spawn(async move {
            let res = backend::get_pr(&st, id.clone()).await.map_err(err);
            let _ = tx.send(Msg::Detail(id, res));
        });
    }

    fn set_detail(&mut self, id: String, detail: Value) {
        if self.selected().is_some_and(|p| p.id == id) {
            self.detail = Some((id, detail));
            self.lines = None;
            self.load_wt();
        }
    }

    /// The detail of the selected PR, once loaded.
    fn pr(&self) -> Option<&Value> {
        let (id, detail) = self.detail.as_ref()?;
        self.selected().filter(|p| p.id == *id).map(|_| detail)
    }

    /// Looks up the PR branch's worktree: cheap (`git worktree list`), so done on every change.
    fn load_wt(&self) {
        let Some(pr) = self.pr() else { return };
        let id = pr["id"].as_str().unwrap_or_default().to_string();
        let repo = pr["repository"]["nameWithOwner"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let branch = pr["headRefName"].as_str().unwrap_or_default().to_string();
        let (st, tx) = (self.st.clone(), self.tx.clone());
        self.rt.spawn(async move {
            let res = backend::get_worktree(&st, &repo, &branch)
                .await
                .map_err(err);
            let _ = tx.send(Msg::Worktree(id, res));
        });
    }

    /// Runs `fut` in the background; `done` is what to say when it worked.
    fn spawn(&self, done: &str, fut: impl Future<Output = anyhow::Result<()>> + Send + 'static) {
        let (tx, done) = (self.tx.clone(), done.to_string());
        self.rt.spawn(async move {
            let _ = tx.send(Msg::Done(done, fut.await.map_err(err)));
        });
    }

    /// The prompt tabs: the config's prompts, and the fix agent once it ran on the PR.
    fn prompts(&self) -> Vec<Prompt> {
        let cfg = self.st.cfg.read().unwrap();
        let mut prompts = cfg.prompts.clone();
        if let Some(fix) = &cfg.fix
            && self.runs.iter().any(|r| r.prompt == FIX)
        {
            prompts.push(Prompt {
                name: FIX.into(),
                harness: fix.harness,
                prompt: fix.prompt.clone(),
                args: fix.args.clone(),
            });
        }
        prompts
    }

    /// The tab's prompt, `None` on the PR tab (or when the prompt left the config).
    fn tab_prompt(&self) -> Option<Prompt> {
        self.prompts().into_iter().find(|p| p.name == self.tab_name)
    }

    fn tab_run(&self) -> Option<&Run> {
        self.runs.iter().find(|r| r.prompt == self.tab_name)
    }

    /// Review bots' open threads on the PR, a tab each (named `<bot>`).
    fn bots(&self) -> Vec<BotReview> {
        self.pr().map(bot_reviews).unwrap_or_default()
    }

    fn tab_names(&self) -> Vec<String> {
        let bots = self.bots().into_iter().map(|b| format!("<{}>", b.bot));
        let prompts = self.prompts().into_iter().map(|p| p.name);
        std::iter::once(String::new())
            .chain(bots)
            .chain(prompts)
            .collect()
    }

    /// The tab on screen: the PR tab when the one you were on isn't here (like the fix tab
    /// before it ran on this PR).
    fn tab(&self) -> String {
        let names = self.tab_names();
        names
            .into_iter()
            .find(|n| *n == self.tab_name)
            .unwrap_or_default()
    }

    fn tab_bot(&self) -> Option<BotReview> {
        self.bots()
            .into_iter()
            .find(|b| format!("<{}>", b.bot) == self.tab_name)
    }

    /// The tab's findings, with the key their picks are kept under: a bot's open threads, or the
    /// review of the tab's run.
    fn tab_review(&self) -> Option<(String, Vec<Finding>)> {
        if let Some(bot) = self.tab_bot() {
            let id = self.pr()?["id"].as_str()?;
            return Some((format!("{id}/{}", bot.key), bot.findings));
        }
        let run = self.tab_run()?;
        Some((review_key(run), run.review.as_ref()?.findings.clone()))
    }

    fn picked(&self) -> Vec<bool> {
        let Some((key, findings)) = self.tab_review() else {
            return Vec::new();
        };
        self.picks
            .get(&key)
            .cloned()
            .unwrap_or_else(|| findings.iter().map(picked_by_default).collect())
    }

    /// Whether the tab's findings can be picked: to post, or to fix.
    fn pickable(&self) -> bool {
        let can_fix = self.st.cfg.read().unwrap().fix.is_some();
        let unposted = self
            .tab_run()
            .and_then(|r| r.review.as_ref())
            .is_some_and(|r| r.posted.is_none());
        let has = self.tab_review().is_some_and(|(_, f)| !f.is_empty());
        has && (can_fix || unposted || self.tab_bot().is_some())
    }

    fn switch_tab(&mut self, delta: isize) {
        let names = self.tab_names();
        let at = names.iter().position(|n| *n == self.tab_name).unwrap_or(0) as isize;
        let len = names.len() as isize;
        self.tab_name = names[((at + delta) % len + len) as usize % names.len()].clone();
        (self.scroll, self.cursor) = (0, 0);
    }

    /// A finished run counts as read once its tab is on screen.
    fn mark_seen(&mut self) {
        let Some(run) = self.tab_run() else { return };
        if self.pr().is_some() && !run.seen && run.status != RunStatus::Running {
            let res = backend::mark_run_seen(&self.st, &run.pr_id.clone(), &run.prompt.clone());
            self.say(res);
        }
    }

    fn worktree(&mut self, opener: usize) {
        let (Some(wt), Some(pr)) = (&self.wt, self.selected()) else {
            return;
        };
        let (repo, number) = (pr.repo.clone(), pr.number);
        let st = self.st.clone();
        if let Some(path) = wt.path.clone() {
            let branch = self
                .pr()
                .and_then(|p| p["headRefName"].as_str())
                .unwrap_or_default();
            let branch = branch.to_string();
            self.spawn("", async move {
                backend::open_worktree(&st, opener, &path, &repo, &branch, number).await
            });
        } else if wt.clone.is_some() && wt.wt && !self.creating_wt {
            self.creating_wt = true;
            self.spawn("worktree created", async move {
                backend::create_worktree(&st, &repo, number).await
            });
        } else if wt.clone.is_none() {
            self.flash = Some((format!("no clone of {repo} under clone_roots"), FAIL));
        } else if !wt.wt {
            self.flash = Some(("creating worktrees needs worktrunk (wt)".into(), FAIL));
        }
    }

    /// Posts the tab run's picked findings as one review.
    fn post(&mut self, approve: bool) {
        let Some(run) = self.tab_run() else { return };
        let picked: Vec<usize> = self
            .picked()
            .iter()
            .enumerate()
            .filter(|(_, p)| **p)
            .map(|(i, _)| i)
            .collect();
        let (st, pr_id, prompt) = (self.st.clone(), run.pr_id.clone(), run.prompt.clone());
        self.spawn("review posted", async move {
            backend::post_review(&st, &pr_id, &prompt, &picked, approve).await
        });
    }

    fn fix(&mut self) {
        let (Some((_, findings)), Some(target)) =
            (self.tab_review(), self.pr().and_then(Target::from_detail))
        else {
            return;
        };
        let picked = self.picked();
        let findings: Vec<_> = findings
            .iter()
            .zip(picked)
            .filter(|(_, p)| *p)
            .map(|(f, _)| f.clone())
            .collect();
        let res = backend::fix_findings(&self.st, target, &findings);
        if res.is_ok() {
            (self.tab_name, self.scroll) = (FIX.into(), 0);
        }
        self.say(res);
    }

    /// Moves the selection, picking the PR you land on (and so marking it read).
    fn go(&mut self, to: usize) {
        if self.rows.is_empty() {
            return;
        }
        self.sel = to.min(self.rows.len() - 1);
        let pr = &self.inbox[self.rows[self.sel].pr];
        let (id, unread) = (pr.id.clone(), pr.unread);
        self.picked = Some(id.clone());
        if unread {
            let res = backend::set_unread(&self.st, &id, false);
            self.say(res);
        }
        self.load_detail();
    }

    fn scroll_by(&mut self, delta: i32) {
        let max = self.pane_lines as i32;
        self.scroll = (self.scroll as i32 + delta).clamp(0, (max - 1).max(0)) as u16;
    }

    fn open(&mut self) {
        let Some(url) = self.selected().map(|p| p.url.clone()) else {
            return;
        };
        // Over SSH, `open` would run on the server: copy the link to your clipboard instead (OSC 52).
        if std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some() {
            let mut out = std::io::stdout();
            let _ = write!(out, "\x1b]52;c;{}\x07", base64(url.as_bytes()));
            let _ = out.flush();
            self.flash = Some(("link copied".into(), OK));
            return;
        }
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let res = std::process::Command::new(opener).arg(&url).spawn();
        self.say(res.map(drop).map_err(Into::into));
    }

    /// Handles a key; false to quit.
    fn key(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            return false;
        }
        self.flash = None;
        match self.mode {
            Mode::Filter => {
                match k.code {
                    KeyCode::Esc => {
                        self.filter.clear();
                        self.mode = Mode::Normal;
                    }
                    KeyCode::Enter => self.mode = Mode::Normal,
                    KeyCode::Backspace => {
                        self.filter.pop();
                    }
                    KeyCode::Char(c) => self.filter.push(c),
                    _ => return true,
                }
                self.rebuild();
                return true;
            }
            Mode::Snooze => {
                self.mode = Mode::Normal;
                let days = match k.code {
                    KeyCode::Char('1') => 1,
                    KeyCode::Char('3') => 3,
                    KeyCode::Char('7') => 7,
                    _ => return true,
                };
                if let Some(id) = self.selected().map(|p| p.id.clone()) {
                    let res = backend::snooze(&self.st, &id, Some(days));
                    self.say(res);
                }
                return true;
            }
            Mode::Approve => {
                match k.code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Enter => {
                        self.mode = Mode::Normal;
                        let pr = self.pr();
                        let id = pr
                            .and_then(|p| p["id"].as_str())
                            .unwrap_or_default()
                            .to_string();
                        let oid = pr
                            .and_then(|p| p["headRefOid"].as_str())
                            .unwrap_or_default()
                            .to_string();
                        let (st, body) = (self.st.clone(), std::mem::take(&mut self.input));
                        self.spawn("approved", async move {
                            backend::approve_pr(&st, &id, &oid, body).await
                        });
                    }
                    KeyCode::Backspace => {
                        self.input.pop();
                    }
                    KeyCode::Char(c) => self.input.push(c),
                    _ => {}
                }
                return true;
            }
            Mode::Post { approve } => {
                self.mode = Mode::Normal;
                if k.code == KeyCode::Char('y') {
                    self.post(approve);
                }
                return true;
            }
            Mode::Opener => {
                self.mode = Mode::Normal;
                if let KeyCode::Char(c @ '1'..='9') = k.code {
                    self.worktree(c as usize - '1' as usize);
                }
                return true;
            }
            Mode::Help => {
                self.mode = Mode::Normal;
                return true;
            }
            Mode::Normal => {}
        }
        let was_g = std::mem::take(&mut self.pending_g);
        let half = (self.pane_height / 2).max(1) as i32;
        let last = self.rows.len().saturating_sub(1);
        match (k.code, ctrl) {
            (KeyCode::Char('q'), _) => return false,
            (KeyCode::Char('d'), true) => self.scroll_by(half),
            (KeyCode::Char('u'), true) => self.scroll_by(-half),
            (KeyCode::Char('r'), true) => backend::refresh(&self.st),
            (_, true) => {}
            // In a prompt tab, j/k step through findings; past either end they scroll again.
            (KeyCode::Char('j') | KeyCode::Down, _)
                if self.pr_focus && self.pickable() && self.cursor + 1 < self.findings_len() =>
            {
                (self.cursor, self.follow) = (self.cursor + 1, true)
            }
            (KeyCode::Char('k') | KeyCode::Up, _)
                if self.pr_focus && self.pickable() && self.cursor > 0 =>
            {
                (self.cursor, self.follow) = (self.cursor - 1, true)
            }
            (KeyCode::Char('j') | KeyCode::Down, _) if self.pr_focus => self.scroll_by(1),
            (KeyCode::Char('k') | KeyCode::Up, _) if self.pr_focus => self.scroll_by(-1),
            (KeyCode::Char('j') | KeyCode::Down, _) => self.go(self.sel + 1),
            (KeyCode::Char('k') | KeyCode::Up, _) => self.go(self.sel.saturating_sub(1)),
            (KeyCode::Char('g'), _) if was_g => {
                if self.pr_focus {
                    self.scroll = 0
                } else {
                    self.go(0)
                }
            }
            (KeyCode::Char('g'), _) => self.pending_g = true,
            (KeyCode::Char('G'), _) if self.pr_focus => self.scroll_by(i32::MAX / 2),
            (KeyCode::Char('G'), _) => self.go(last),
            (KeyCode::Tab | KeyCode::BackTab, _) => self.pr_focus = !self.pr_focus,
            (KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter, _) => self.pr_focus = true,
            (KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc, _) if self.pr_focus => {
                self.pr_focus = false
            }
            (KeyCode::Esc, _) if !self.filter.is_empty() => {
                self.filter.clear();
                self.rebuild();
            }
            (KeyCode::Char('/'), _) => {
                self.pr_focus = false;
                self.mode = Mode::Filter;
            }
            (KeyCode::Char('o'), _) => self.open(),
            (KeyCode::Char('u'), _) => {
                if let Some((id, unread)) = self.selected().map(|p| (p.id.clone(), p.unread)) {
                    let res = backend::set_unread(&self.st, &id, !unread);
                    self.say(res);
                }
            }
            (KeyCode::Char('z'), _) if self.smart => match self.selected() {
                Some(p) if p.snoozed_until.is_some() => {
                    let id = p.id.clone();
                    let res = backend::snooze(&self.st, &id, None);
                    self.say(res);
                }
                Some(_) => self.mode = Mode::Snooze,
                None => {}
            },
            (KeyCode::Char('?'), _) => self.mode = Mode::Help,
            (KeyCode::Char('['), _) => self.switch_tab(-1),
            (KeyCode::Char(']'), _) => self.switch_tab(1),
            (KeyCode::Char('w'), _) => self.worktree(0),
            (KeyCode::Char('W'), _) => {
                let openers = self.st.cfg.read().unwrap().open_with.len();
                if self.wt.as_ref().is_some_and(|w| w.path.is_some()) && openers > 0 {
                    self.mode = Mode::Opener;
                }
            }
            (KeyCode::Char('a'), _) if self.tab().is_empty() => {
                let viewer = self.status.viewer.as_deref();
                if self.pr().is_some_and(|p| can_approve(p, viewer)) {
                    (self.mode, self.input) = (Mode::Approve, String::new());
                }
            }
            (KeyCode::Char('a' | 'p'), _) => {
                let unposted = self
                    .tab_run()
                    .and_then(|r| r.review.as_ref())
                    .is_some_and(|r| r.posted.is_none());
                let any = self.picked().contains(&true);
                let approve = k.code == KeyCode::Char('a');
                if unposted && (approve || any) {
                    self.mode = Mode::Post { approve };
                }
            }
            (KeyCode::Char(' '), _) if self.pickable() => {
                let (key, _) = self.tab_review().unwrap();
                let mut picked = self.picked();
                if let Some(p) = picked.get_mut(self.cursor) {
                    *p = !*p;
                }
                self.picks.insert(key, picked);
            }
            (KeyCode::Char('f'), _) if self.pickable() => self.fix(),
            (KeyCode::Char('r'), _) => {
                let target = self.pr().and_then(Target::from_detail);
                if let (Some(p), Some(target)) = (self.tab_prompt(), target)
                    && p.name != FIX
                {
                    let res = backend::run_prompt(&self.st, &p.name, target);
                    self.say(res);
                }
            }
            (KeyCode::Char('X'), _) => {
                if let Some((id, prompt)) =
                    self.tab_run().map(|r| (r.pr_id.clone(), r.prompt.clone()))
                {
                    let res = backend::cancel_run(&self.st, &id, &prompt);
                    self.say(res);
                }
            }
            (KeyCode::Char('c'), _) => {
                if let Some((id, prompt)) =
                    self.tab_run().map(|r| (r.pr_id.clone(), r.prompt.clone()))
                {
                    let st = self.st.clone();
                    self.spawn(
                        "",
                        async move { backend::continue_run(&st, &id, &prompt).await },
                    );
                }
            }
            (KeyCode::Char('t'), _) => {
                let all: Vec<&str> = std::iter::once("")
                    .chain(self.rules.iter().map(String::as_str))
                    .collect();
                let at = all.iter().position(|r| *r == self.rule).unwrap_or(0);
                self.rule = all[(at + 1) % all.len()].to_string();
                self.rebuild();
            }
            _ => {}
        }
        true
    }

    fn findings_len(&self) -> usize {
        self.tab_review().map_or(0, |(_, f)| f.len())
    }

    fn update(&mut self, msg: Msg) -> bool {
        match msg {
            Msg::Input(Input::Key(k)) if k.kind == KeyEventKind::Press => {
                if !self.key(k) {
                    return false;
                }
            }
            Msg::Input(_) | Msg::Tick => {}
            Msg::Core(Event::Changed) => self.reload(),
            Msg::Core(Event::Status) => self.status = self.st.status.lock().unwrap().clone(),
            Msg::Core(Event::PrUpdated { id, detail }) => self.set_detail(id, detail),
            Msg::Core(Event::RunUpdated(id)) => {
                if self.selected().is_some_and(|p| p.id == id) {
                    self.runs = self.st.db.runs(&id).unwrap_or_default();
                }
            }
            Msg::Core(Event::WorktreeCreated) => self.load_wt(),
            Msg::Detail(id, Ok(detail)) => self.set_detail(id, detail),
            Msg::Detail(_, Err(e)) => self.flash = Some((e, FAIL)),
            Msg::Worktree(id, res) => {
                if self.selected().is_some_and(|p| p.id == id) {
                    match res {
                        Ok(wt) => self.wt = Some(wt),
                        Err(e) => self.flash = Some((e, FAIL)),
                    }
                }
            }
            Msg::Done(done, res) => {
                self.creating_wt = false;
                self.load_wt();
                match res {
                    Ok(()) if done.is_empty() => {}
                    Ok(()) => self.flash = Some((done, OK)),
                    Err(e) => self.flash = Some((e, FAIL)),
                }
            }
        }
        self.mark_seen();
        true
    }

    fn draw(&mut self, f: &mut Frame) {
        let [main, bar] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(f.area());
        // 40%, but room for a repo#number and its age on narrow screens.
        let inbox_w = (main.width * 2 / 5).max(main.width.min(96) / 2);
        let [left, right] =
            Layout::horizontal([Constraint::Length(inbox_w), Constraint::Fill(1)]).areas(main);
        self.draw_inbox(f, left);
        self.draw_pr(f, right);
        self.draw_bar(f, bar);
        if self.mode == Mode::Help {
            self.draw_help(f);
        }
    }

    fn draw_inbox(&self, f: &mut Frame, area: Rect) {
        let now = now_ms();
        let rule = if self.rules.is_empty() {
            String::new()
        } else {
            format!(
                " · t: {}",
                if self.rule.is_empty() {
                    "all"
                } else {
                    &self.rule
                }
            )
        };
        let shown = self.rows.iter().filter(|r| !r.dim).count();
        let title = format!(" Inbox {shown}{rule} ");
        let block = Block::bordered()
            .title(title)
            .border_style(if self.pr_focus { DIM } else { ACCENT });
        let width = block.inner(area).width as usize;
        let mut items = Vec::new();
        let mut at = None;
        for (i, row) in self.rows.iter().enumerate() {
            if Some(i) == self.wait_at {
                let waiting = self.rows[i..].iter().filter(|r| !r.dim).count();
                let lead = if i == 0 { "nothing needs you · " } else { "" };
                items.push(ListItem::new(Line::styled(
                    format!("── {lead}can wait · {waiting}"),
                    DIM,
                )));
            }
            if i == self.sel {
                at = Some(items.len());
            }
            let later = self.wait_at.is_some_and(|w| i >= w);
            let lead = self
                .leads
                .get(i)
                .copied()
                .flatten()
                .filter(|&l| l != row.pr);
            items.push(self.inbox_item(row, later, lead.map(|l| &self.inbox[l]), width, now));
        }
        if items.is_empty() {
            let empty = if self.filter.is_empty() {
                "Inbox zero."
            } else {
                "No match."
            };
            items.push(ListItem::new(Line::styled(empty, DIM)));
        }
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().bg(Color::Indexed(236)));
        let mut state = ListState::default().with_selected(at);
        f.render_stateful_widget(list, area, &mut state);
    }

    fn inbox_item(
        &self,
        row: &PrRow,
        later: bool,
        lead: Option<&PrSummary>,
        width: usize,
        now: u64,
    ) -> ListItem<'static> {
        let pr = &self.inbox[row.pr];
        let unread = pr.unread && !row.dim;
        // Tree lines: `first` for the row's own line, the others for the lines under it.
        let (first, rest) = match &row.graph {
            None => (String::new(), "  ".to_string()),
            Some(g) => {
                let col = |l: usize, last: &'static str, mid: &'static str| match () {
                    _ if l + 1 == g.depth => {
                        if g.last {
                            last
                        } else {
                            mid
                        }
                    }
                    _ if g.through.contains(&l) => "│ ",
                    _ => "  ",
                };
                let first: String = (0..g.depth).map(|l| col(l, "└ ", "├ ")).collect();
                let mut rest: String = (0..g.depth).map(|l| col(l, "  ", "│ ")).collect();
                rest += if g.down { "│ " } else { "  " };
                (first, rest)
            }
        };
        let tree = |s: &str| Span::styled(s.to_string(), DIM);
        let fg = if row.dim || later { DIM } else { Color::Reset };

        let mut right = vec![];
        if let Some(lead) = lead.filter(|l| l.next.is_some()) {
            let next = format!("{:?}", lead.next.unwrap()).to_lowercase();
            right.push(Span::styled(
                format!("{next} ↓#{} ", lead.number),
                next_color(&next),
            ));
        }
        let age = parse_ms(&pr.updated_at)
            .map(|t| rel_time(t, now))
            .unwrap_or_default();
        right.push(Span::styled(age, DIM));
        let dot = Span::styled(if unread { "● " } else { "  " }, ACCENT);
        let name = Span::styled(format!("{}#{}", pr.repo, pr.number), DIM);
        let line1 = spread(vec![tree(&first), dot, name], right, width);

        let mut title = Span::styled(pr.title.clone(), fg);
        if unread {
            title = title.add_modifier(Modifier::BOLD);
        }
        let line2 = vec![tree(&rest), title];

        let mut line3 = vec![tree(&rest)];
        if self.smart && !row.dim {
            if pr.snoozed_until.is_some() {
                line3.push(Span::styled("snoozed ", DIM));
            } else if let Some(next) = pr.next {
                let next = format!("{next:?}").to_lowercase();
                let color = if later { DIM } else { next_color(&next) };
                line3.push(Span::styled(format!("{next} "), color));
            }
        }
        line3.push(Span::styled(pr.author.clone(), DIM));
        for r in &pr.rules {
            line3.push(Span::styled(format!(" [{r}]"), fg));
        }
        if pr.is_draft {
            line3.push(Span::styled(" draft", DIM));
        }
        let mut tail = vec![];
        // `ready` already says approved.
        if let Some(rd) = pr.review_decision.as_deref()
            && !(self.smart && pr.next == Some(ghreviewer_core::sync::Next::Ready))
        {
            let (label, color) = review_label(rd);
            tail.push(Span::styled(
                format!("{label} "),
                if later { DIM } else { color },
            ));
        }
        let (glyph, color) = check_glyph(pr.checks.as_deref().unwrap_or(""));
        tail.push(Span::styled(glyph, color));
        let line3 = spread(line3, tail, width);
        ListItem::new(Text::from(vec![line1, Line::from(line2), line3]))
    }

    /// Tab names with their run's mark, and what the keys can do on the right.
    fn pr_titles(&self, width: usize) -> (Line<'static>, Line<'static>) {
        let head_oid = self.pr().and_then(|p| p["headRefOid"].as_str());
        let mut tabs = vec![Span::raw(" ")];
        // The PR tab shows when the tab's prompt isn't here (like the fix tab before it ran).
        let active = self.tab();
        for (i, name) in self.tab_names().into_iter().enumerate() {
            if i > 0 {
                tabs.push(Span::styled(" │ ", DIM));
            }
            let style = if name == active {
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(DIM)
            };
            let run = self.runs.iter().find(|r| r.prompt == name);
            let stale = run.is_some_and(|r| {
                r.status != RunStatus::Running
                    && r.head_oid.is_some()
                    && r.head_oid.as_deref() != head_oid
            });
            let mark = match run.map(|r| r.status) {
                Some(RunStatus::Running) => Span::styled(" …", WARN),
                Some(RunStatus::Error) => Span::styled(" !", FAIL),
                _ if stale => Span::styled(" *", WARN),
                _ => Span::raw(""),
            };
            let label = match name.as_str() {
                "" => "PR".to_string(),
                bot if bot.starts_with('<') => bot.trim_matches(['<', '>']).to_string(),
                _ => name,
            };
            tabs.push(Span::styled(label, style));
            tabs.push(mark);
        }
        tabs.push(Span::raw(" "));
        let mut right = vec![];
        let viewer = self.status.viewer.as_deref();
        if self.tab().is_empty() && self.pr().is_some_and(|p| can_approve(p, viewer)) {
            right.push(Span::styled(" a approve ", ACCENT));
        }
        match &self.wt {
            _ if self.creating_wt => right.push(Span::styled(" creating worktree… ", WARN)),
            Some(Worktree {
                path: Some(path), ..
            }) => {
                // As much of the folder name as fits beside the tabs.
                let used =
                    Line::from(tabs.clone()).width() + right.iter().map(Span::width).sum::<usize>();
                let room = width.saturating_sub(used + 6);
                let name = path.rsplit('/').next().unwrap_or_default();
                if room > 3 {
                    right.push(Span::styled(format!(" w {} ", fit(name, room)), DIM));
                }
            }

            Some(Worktree {
                clone: Some(_),
                wt: true,
                ..
            }) => right.push(Span::styled(" w + worktree ", DIM)),
            _ => {}
        }
        (Line::from(tabs), Line::from(right).right_aligned())
    }

    fn draw_pr(&mut self, f: &mut Frame, area: Rect) {
        let (tabs, right) = self.pr_titles(area.width as usize);
        let block = Block::bordered()
            .title(tabs)
            .title(right)
            .border_style(if self.pr_focus { ACCENT } else { DIM });
        let inner = block.inner(area);
        self.pane_height = inner.height;
        let Some(detail) = self.pr() else {
            let text = if self.selected().is_some() {
                "Loading…"
            } else {
                ""
            };
            f.render_widget(Paragraph::new(Line::styled(text, DIM)).block(block), area);
            return;
        };
        let can_fix = self.st.cfg.read().unwrap().fix.is_some();
        let bot_view = self.tab_bot().map(|bot| {
            run::bot_view(
                &bot,
                &self.picked(),
                self.cursor,
                can_fix,
                inner.width as usize,
            )
        });
        let prompt_view = self.tab_prompt().map(|prompt| {
            let run = self.tab_run();
            let head_oid = detail["headRefOid"].as_str();
            run::view(run::Args {
                prompt: &prompt,
                run,
                stale: run.is_some_and(|r| {
                    r.status != RunStatus::Running
                        && r.head_oid.is_some()
                        && r.head_oid.as_deref() != head_oid
                }),
                has_worktree: self.wt.as_ref().is_some_and(|w| w.path.is_some()),
                picked: &self.picked(),
                cursor: self.cursor,
                can_fix,
                width: inner.width as usize,
                now: now_ms(),
            })
        });
        if let Some(view) = bot_view.or(prompt_view) {
            self.pane_lines = view.lines.len();
            if std::mem::take(&mut self.follow)
                && let Some(&at) = view.findings.get(self.cursor)
            {
                let (at, h) = (at as u16, inner.height.max(4));
                if at < self.scroll {
                    self.scroll = at;
                } else if at + 3 > self.scroll + h {
                    self.scroll = at + 3 - h;
                }
            }
            let para = Paragraph::new(view.lines)
                .block(block)
                .scroll((self.scroll, 0));
            f.render_widget(para, area);
            return;
        }
        if self.lines.as_ref().is_none_or(|(w, _)| *w != inner.width) {
            let lines = pr::lines(detail, inner.width as usize, now_ms());
            self.lines = Some((inner.width, lines));
        }
        let lines = self.lines.as_ref().unwrap().1.clone();
        self.pane_lines = lines.len();
        let para = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0));
        f.render_widget(para, area);
    }

    fn draw_help(&self, f: &mut Frame) {
        let w = HELP.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        let lines: Vec<Line> = HELP
            .iter()
            .map(|(k, v)| {
                Line::from(vec![
                    Span::styled(format!("{k:<w$}  "), ACCENT),
                    Span::raw(*v),
                ])
            })
            .collect();
        let area = f.area();
        let width = (area.width.saturating_sub(4)).min(96);
        let height = (lines.len() as u16 + 2).min(area.height);
        let at = Rect::new(
            (area.width - width) / 2,
            (area.height - height) / 2,
            width,
            height,
        );
        f.render_widget(Clear, at);
        let block = Block::bordered().title(" Keys ").border_style(ACCENT);
        f.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            at,
        );
    }

    fn draw_bar(&self, f: &mut Frame, area: Rect) {
        let dim = |s: String| Span::styled(s, DIM);
        let left: Vec<Span> = match self.mode {
            Mode::Filter => vec![
                Span::styled("/", ACCENT),
                Span::raw(self.filter.clone()),
                Span::raw("█"),
            ],
            Mode::Snooze => vec![
                Span::styled("snooze: ", ACCENT),
                dim("1 day · 3 days · 7 a week · any key cancels".into()),
            ],
            Mode::Approve => vec![
                Span::styled("approve, message (optional): ", ACCENT),
                Span::raw(self.input.clone()),
                Span::raw("█"),
                dim("  Enter approve · Esc cancel".into()),
            ],
            Mode::Post { approve } => {
                let n = self
                    .tab_run()
                    .map_or(0, |_| self.picked().iter().filter(|p| **p).count());
                let what = match (approve, n) {
                    (true, 0) => "approve".to_string(),
                    (true, n) => format!("approve with {n} comments"),
                    (false, n) => format!("comment with {n} findings"),
                };
                vec![
                    Span::styled(format!("post review: {what}? "), ACCENT),
                    dim("y / any key cancels".into()),
                ]
            }
            Mode::Opener => {
                let cfg = self.st.cfg.read().unwrap();
                let names: Vec<String> = cfg
                    .open_with
                    .iter()
                    .take(9)
                    .enumerate()
                    .map(|(i, o)| format!("{} {}", i + 1, o.name))
                    .collect();
                vec![
                    Span::styled("open worktree in: ", ACCENT),
                    dim(names.join(" · ") + " · any other key cancels"),
                ]
            }
            Mode::Help => vec![dim("any key closes".into())],
            Mode::Normal => {
                let mut spans = vec![];
                if let Some((text, color)) = &self.flash {
                    spans.push(Span::styled(text.clone(), *color));
                } else if let Some(e) = [
                    &self.status.config_error,
                    &self.status.error,
                    &self.status.jira_error,
                ]
                .into_iter()
                .flatten()
                .next()
                {
                    spans.push(Span::styled(e.clone(), FAIL));
                } else {
                    let who = self.status.viewer.clone().unwrap_or_default();
                    let sync = if self.status.syncing {
                        "syncing…".to_string()
                    } else {
                        self.status
                            .last_sync_ms
                            .map(|t| format!("synced {}", rel_time(t, now_ms())))
                            .unwrap_or_default()
                    };
                    spans.push(dim(format!("{who} · {sync}")));
                    if !self.filter.is_empty() {
                        spans.push(Span::styled(format!(" · /{}", self.filter), ACCENT));
                    }
                }
                spans
            }
        };
        let keys = "j/k move · Tab pane · [ ] tabs · / filter · ? keys · q quit";
        let keys_w = keys.chars().count() as u16;
        // The hints only when they leave room for the status.
        let keys_w = if area.width >= keys_w + 40 { keys_w } else { 0 };
        let [l, r] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(keys_w)]).areas(area);
        f.render_widget(Line::from(left), l);
        f.render_widget(Line::styled(keys, DIM).right_aligned(), r);
    }
}

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let (tx, rx) = mpsc::channel();
    let events = tx.clone();
    let st = AppState::open(
        move |e| drop(events.send(Msg::Core(e))),
        rt.handle().clone(),
    )?;
    let _watcher = backend::start(&st)?;
    let input = tx.clone();
    std::thread::spawn(move || {
        while let Ok(e) = event::read() {
            if input.send(Msg::Input(e)).is_err() {
                break;
            }
        }
    });
    let mut app = App {
        st,
        rt: rt.handle().clone(),
        tx,
        status: Status::default(),
        smart: false,
        rules: Vec::new(),
        rule: String::new(),
        filter: String::new(),
        mode: Mode::Normal,
        inbox: Vec::new(),
        rows: Vec::new(),
        leads: Vec::new(),
        wait_at: None,
        sel: 0,
        picked: None,
        pr_focus: false,
        detail_key: None,
        detail: None,
        lines: None,
        scroll: 0,
        pane_height: 0,
        pending_g: false,
        flash: None,
        tab_name: String::new(),
        runs: Vec::new(),
        wt: None,
        creating_wt: false,
        picks: HashMap::new(),
        cursor: 0,
        follow: false,
        pane_lines: 0,
        input: String::new(),
    };
    app.reload(); // the cache shows right away, the first sync follows
    let mut term = ratatui::init();
    let res = (|| -> anyhow::Result<()> {
        loop {
            term.draw(|f| app.draw(f))?;
            // A tick now and then keeps the relative times fresh; each second for a run's timer.
            let running = app.runs.iter().any(|r| r.status == RunStatus::Running);
            let tick = Duration::from_secs(if running { 1 } else { 30 });
            let msg = match rx.recv_timeout(tick) {
                Ok(m) => m,
                Err(mpsc::RecvTimeoutError::Timeout) => Msg::Tick,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            };
            if !app.update(msg) {
                return Ok(());
            }
            // Catch up before drawing again: a sync sends a burst.
            while let Ok(msg) = rx.try_recv() {
                if !app.update(msg) {
                    return Ok(());
                }
            }
        }
    })();
    ratatui::restore();
    res
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_pads() {
        assert_eq!(super::base64(b"hi"), "aGk=");
        assert_eq!(super::base64(b"hey"), "aGV5");
        assert_eq!(super::base64(b"h"), "aA==");
    }

    #[test]
    fn spread_keeps_the_right_side_whole() {
        use ratatui::text::Span;
        let text = |l: ratatui::text::Line| l.to_string();
        let l = super::spread(
            vec![Span::raw("ab"), Span::raw("cdef")],
            vec![Span::raw("1d")],
            8,
        );
        assert_eq!(text(l), "abcd… 1d");
        let l = super::spread(vec![Span::raw("ab")], vec![Span::raw("1d")], 8);
        assert_eq!(text(l), "ab    1d");
    }

    #[test]
    fn fit_cuts_with_an_ellipsis() {
        assert_eq!(super::fit("acme/web#12", 20), "acme/web#12");
        assert_eq!(super::fit("acme/web#12", 6), "acme/…");
        assert_eq!(super::fit("abc", 0), "…");
    }
}
