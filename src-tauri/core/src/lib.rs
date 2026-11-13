//! Everything but the UI: config, cache, GitHub/Jira sync and agent runs, shared by the desktop
//! app and the TUI. A frontend opens an `AppState` with a callback for `Event`s, then `start`s it.

// `Github::new()`/`Jira::new()` build HTTP clients: not a cheap `Default`.
#![allow(clippy::new_without_default)]

pub mod agent;
pub mod config;
pub mod db;
pub mod github;
pub mod inbox;
pub mod jira;
pub mod local;
pub mod sync;

use agent::{Finding, Review, Run, RunStatus};
use anyhow::{Context, bail, ensure};
use config::{Config, Prompt, Settings};
use db::Db;
use github::Github;
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use sync::{PrSummary, Repo};
use tokio::{runtime::Handle, sync::Notify, task::JoinHandle};

const REPOS_EVERY: Duration = Duration::from_secs(3600);

/// What changed, for the frontend to re-read.
#[derive(Debug, Clone)]
pub enum Event {
    /// Local data changed: inbox, repos, tickets, config.
    Changed,
    Status,
    PrUpdated {
        id: String,
        detail: Value,
    },
    /// Runs of this PR id changed.
    RunUpdated(String),
    WorktreeCreated,
}

pub struct AppState {
    pub cfg_path: PathBuf,
    pub cfg: RwLock<Config>,
    pub db: Db,
    pub gh: Github,
    pub jira: jira::Jira,
    pub status: Mutex<Status>,
    wake: Notify,
    force_repos: AtomicBool,
    pub data_dir: PathBuf,
    runs: Mutex<HashMap<(String, String), RunningTask>>,
    run_seq: AtomicU64,
    on_event: Box<dyn Fn(Event) + Send + Sync>,
    /// Background work is spawned here: some callers aren't on a runtime thread.
    rt: Handle,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub viewer: Option<String>,
    pub syncing: bool,
    /// The Jira step of a sync, after GitHub's.
    pub syncing_jira: bool,
    pub last_sync_ms: Option<u64>,
    pub rate_remaining: Option<u64>,
    pub error: Option<String>,
    pub jira_error: Option<String>,
    pub config_error: Option<String>,
}

/// A prompt run in progress (keyed by PR id and prompt name), with a token telling runs apart.
type RunningTask = (u64, JoinHandle<()>);

/// An error as one line, causes included.
pub fn err(e: impl Into<anyhow::Error>) -> String {
    format!("{:#}", e.into())
}

impl AppState {
    /// Loads the config and opens the cache. A broken config still opens, with `config_error`.
    pub fn open(
        on_event: impl Fn(Event) + Send + Sync + 'static,
        rt: Handle,
    ) -> anyhow::Result<Arc<Self>> {
        let data_dir = dirs::data_dir().context("no data dir")?.join("ghreviewer");
        std::fs::create_dir_all(&data_dir)?;
        let cfg_path = config::path();
        config::ensure(&cfg_path)?;
        let (cfg, config_error) = match config::load(&cfg_path) {
            Ok(c) => (c.unwrap_or_default(), None),
            Err(e) => (Config::default(), Some(err(e))),
        };
        let st = Arc::new(AppState {
            cfg: RwLock::new(cfg),
            db: Db::open(&data_dir.join("cache.db"))?,
            gh: Github::new(),
            jira: jira::Jira::new(),
            status: Mutex::new(Status {
                config_error,
                ..Default::default()
            }),
            wake: Notify::new(),
            force_repos: AtomicBool::new(false),
            cfg_path,
            data_dir,
            runs: Mutex::default(),
            run_seq: AtomicU64::new(0),
            on_event: Box::new(on_event),
            rt,
        });
        st.db.interrupt_runs()?;
        Ok(st)
    }

    fn emit(&self, e: Event) {
        (self.on_event)(e);
    }

    /// Tells the frontend that local data changed and should be re-read.
    fn changed(&self) {
        self.emit(Event::Changed);
    }
}

/// Starts syncing and watching the config. Keep the watcher: dropping it stops the watch.
pub fn start(st: &Arc<AppState>) -> anyhow::Result<notify::RecommendedWatcher> {
    // Watch the directory: editors often save by replacing the file.
    let watched = st.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok_and(|e| e.paths.iter().any(|p| p.ends_with("config.toml"))) {
            reload_config(&watched);
        }
    })?;
    watcher.watch(
        st.cfg_path.parent().context("bad config path")?,
        RecursiveMode::NonRecursive,
    )?;
    st.rt.spawn(poller(st.clone()));
    Ok(watcher)
}

fn update_status(st: &AppState, f: impl FnOnce(&mut Status)) {
    f(&mut st.status.lock().unwrap());
    st.emit(Event::Status);
}

fn reload_config(st: &AppState) {
    match config::load(&st.cfg_path) {
        Ok(None) => {} // missing or empty mid-save: keep the current config
        Ok(Some(new)) => {
            let resync = {
                let mut cfg = st.cfg.write().unwrap();
                let resync = cfg.rules != new.rules || cfg.jira != new.jira;
                *cfg = new;
                resync
            };
            if resync {
                st.wake.notify_one();
            }
            update_status(st, |s| s.config_error = None);
            st.changed();
        }
        Err(e) => update_status(st, |s| s.config_error = Some(err(e))),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

async fn sync_once(st: &AppState, with_repos: bool) -> anyhow::Result<Option<u64>> {
    let cached = st.status.lock().unwrap().viewer.clone();
    let viewer = match cached {
        Some(v) => v,
        None => sync::viewer(&st.gh).await?,
    };
    st.status.lock().unwrap().viewer = Some(viewer.clone());
    if with_repos {
        st.db.replace_repos(&sync::fetch_repos(&st.gh).await?)?;
    }
    let rules = st.cfg.read().unwrap().rules.clone();
    let (prs, remaining) = sync::fetch_inbox(&st.gh, &rules, &viewer).await?;
    st.db.replace_prs(&prs)?;
    Ok(remaining)
}

/// Fetches the Jira tickets named in the inbox's PR titles. A ticket that fails to load keeps
/// its cached copy.
async fn sync_jira(st: &Arc<AppState>) -> anyhow::Result<()> {
    let (cfg, filters) = {
        let cfg = st.cfg.read().unwrap();
        (cfg.jira.clone(), cfg.filters.clone())
    };
    let mut keys: Vec<String> = st
        .db
        .inbox(&filters)?
        .iter()
        .filter(|pr| !pr.context)
        .flat_map(|pr| jira::keys(&pr.title, &cfg.prefixes))
        .collect();
    keys.sort();
    keys.dedup();
    let (mut found, mut gone, mut error) = (Vec::new(), Vec::new(), None);
    for chunk in keys.chunks(4) {
        let mut set = tokio::task::JoinSet::new();
        for key in chunk {
            let (st, cfg, key) = (st.clone(), cfg.clone(), key.clone());
            set.spawn(async move { (st.jira.ticket(&cfg, &key).await, key) });
        }
        for (res, key) in set.join_all().await {
            match res {
                Ok(Some(ticket)) => found.push(ticket),
                Ok(None) => gone.push(key),
                Err(e) => error = Some(e),
            }
        }
        // Most likely the sign-in or the network: the other tickets would fail the same way.
        if error.is_some() {
            break;
        }
    }
    keys.retain(|k| !gone.contains(k));
    st.db.put_tickets(&found, &keys)?;
    error.map_or(Ok(()), Err)
}

async fn refresh_detail(st: &AppState, id: &str) -> anyhow::Result<Value> {
    let detail = sync::fetch_detail(&st.gh, id).await?;
    st.db.put_detail(id, &detail)?;
    Ok(detail)
}

const PREFETCH_MAX: usize = 20;

/// Caches details of the newest unread PRs that changed, so opening them is instant.
async fn prefetch_details(st: &Arc<AppState>) {
    let filters = st.cfg.read().unwrap().filters.clone();
    let (Ok(prs), Ok(cached)) = (st.db.inbox(&filters), st.db.detail_versions()) else {
        return;
    };
    let stale: Vec<String> = prs
        .into_iter()
        .filter(|pr| pr.unread && !pr.context && cached.get(&pr.id) != Some(&pr.updated_at))
        .take(PREFETCH_MAX)
        .map(|pr| pr.id)
        .collect();
    for chunk in stale.chunks(4) {
        let mut set = tokio::task::JoinSet::new();
        for id in chunk {
            let (st, id) = (st.clone(), id.clone());
            set.spawn(async move { refresh_detail(&st, &id).await });
        }
        set.join_all().await;
    }
}

async fn poller(st: Arc<AppState>) {
    let mut repos_at: Option<Instant> = None;
    loop {
        let with_repos = st.force_repos.swap(false, Ordering::Relaxed)
            || repos_at.is_none_or(|t| t.elapsed() > REPOS_EVERY);
        update_status(&st, |s| s.syncing = true);
        let res = sync_once(&st, with_repos).await;
        if res.is_ok() && with_repos {
            repos_at = Some(Instant::now());
        }
        let jira = if st.cfg.read().unwrap().jira.enabled() {
            update_status(&st, |s| s.syncing_jira = true);
            st.changed(); // GitHub's part shows while Jira loads
            sync_jira(&st).await
        } else {
            Ok(())
        };
        update_status(&st, |s| {
            s.syncing = false;
            s.syncing_jira = false;
            s.jira_error = jira.err().map(err);
            match res {
                Ok(remaining) => {
                    s.error = None;
                    s.last_sync_ms = Some(now_ms());
                    s.rate_remaining = remaining.or(s.rate_remaining);
                }
                Err(e) => s.error = Some(err(e)),
            }
        });
        st.changed();
        prefetch_details(&st).await;
        let secs = st.cfg.read().unwrap().poll_interval_secs.max(30);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
            _ = st.wake.notified() => {}
        }
    }
}

pub fn list_repos(st: &AppState) -> anyhow::Result<Vec<Repo>> {
    let filters = st.cfg.read().unwrap().filters.clone();
    let mut repos = st.db.repos()?;
    repos.retain(|r| db::repo_passes(&filters, &r.name_with_owner));
    Ok(repos)
}

pub fn list_inbox(st: &AppState) -> anyhow::Result<Vec<PrSummary>> {
    let (filters, prefixes) = {
        let cfg = st.cfg.read().unwrap();
        let jira = cfg.jira.enabled().then(|| cfg.jira.prefixes.clone());
        (cfg.filters.clone(), jira)
    };
    let mut prs = st.db.inbox(&filters)?;
    if let Some(prefixes) = prefixes {
        let categories = st.db.ticket_categories()?;
        for pr in &mut prs {
            pr.jira = jira::refs(&pr.title, &prefixes, &categories);
        }
    }
    Ok(prs)
}

/// Returns the cached PR right away and refreshes it in the background (`Event::PrUpdated`).
pub async fn get_pr(st: &Arc<AppState>, id: String) -> anyhow::Result<Value> {
    if let Some(cached) = st.db.detail(&id)? {
        let st = st.clone();
        st.rt.clone().spawn(async move {
            if let Ok(detail) = refresh_detail(&st, &id).await {
                st.emit(Event::PrUpdated { id, detail });
            }
        });
        return Ok(cached);
    }
    refresh_detail(st, &id).await
}

pub fn set_unread(st: &AppState, id: &str, unread: bool) -> anyhow::Result<()> {
    st.db.set_unread(id, unread)?;
    st.changed();
    Ok(())
}

/// Snoozes a PR for `days`, or wakes it with `None`.
pub fn snooze(st: &AppState, id: &str, days: Option<u64>) -> anyhow::Result<()> {
    let until = days.map(|d| now_ms() + d * 86_400_000);
    st.db.snooze(id, until)?;
    st.changed();
    Ok(())
}

pub fn refresh(st: &AppState) {
    st.force_repos.store(true, Ordering::Relaxed);
    st.wake.notify_one();
}

fn edit_config(
    st: &AppState,
    f: impl FnOnce(&str) -> anyhow::Result<String>,
) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(&st.cfg_path)?;
    config::write(&st.cfg_path, &f(&text)?)?;
    reload_config(st);
    Ok(())
}

pub fn save_settings(st: &AppState, settings: &Settings) -> anyhow::Result<()> {
    edit_config(st, |text| config::apply_settings(text, settings))
}

pub fn set_repo_flag(st: &AppState, repo: &str, flag: &str, on: bool) -> anyhow::Result<()> {
    edit_config(st, |text| config::set_repo_flag(text, repo, flag, on))
}

fn clone_roots(st: &AppState) -> Vec<String> {
    st.cfg.read().unwrap().clone_roots.clone()
}

pub async fn get_worktree(
    st: &AppState,
    repo: &str,
    branch: &str,
) -> anyhow::Result<local::Worktree> {
    local::lookup(&clone_roots(st), repo, branch).await
}

pub async fn create_worktree(st: &AppState, repo: &str, number: u64) -> anyhow::Result<()> {
    local::create(&clone_roots(st), repo, number).await
}

/// Runs the `opener`-th `open_with` command on a PR's worktree.
pub async fn open_worktree(
    st: &AppState,
    opener: usize,
    path: &str,
    repo: &str,
    branch: &str,
    number: u64,
) -> anyhow::Result<()> {
    let command = st
        .cfg
        .read()
        .unwrap()
        .open_with
        .get(opener)
        .map(|o| o.command.clone());
    let command = command.context("no such `open_with` entry")?;
    let name = path.rsplit('/').next().unwrap_or_default().to_string();
    let number = number.to_string();
    let vars = [
        ("path", path),
        ("name", &name),
        ("branch", branch),
        ("repo", repo),
        ("number", &number),
    ];
    local::open_with(&command, &vars).await
}

pub fn mark_run_seen(st: &AppState, pr_id: &str, prompt: &str) -> anyhow::Result<()> {
    st.db.mark_run_seen(pr_id, prompt)?;
    st.emit(Event::RunUpdated(pr_id.to_string()));
    Ok(())
}

/// The PR a prompt runs on, as the frontend knows it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub id: String,
    pub repo: String,
    pub number: u64,
    pub url: String,
    pub branch: String,
    pub base: String,
    pub head_oid: Option<String>,
}

/// Whether the run with `token` is still the live one for `key`: not cancelled, not replaced.
fn is_current<T>(
    running: &HashMap<(String, String), (u64, T)>,
    key: &(String, String),
    token: u64,
) -> bool {
    running.get(key).map(|r| r.0) == Some(token)
}

/// Saves a run, unless it was cancelled or replaced by a newer run of the same prompt.
fn save_run(st: &AppState, token: u64, run: &Run, finished: bool) {
    let key = (run.pr_id.clone(), run.prompt.clone());
    let mut running = st.runs.lock().unwrap();
    if !is_current(&running, &key, token) {
        return;
    }
    if finished {
        running.remove(&key);
    }
    let _ = st.db.put_run(run);
    drop(running);
    st.emit(Event::RunUpdated(run.pr_id.clone()));
}

/// `{out}` of a prompt on a PR, unique per PR (node id) and prompt name.
fn run_dir(pr_id: &str, prompt: &str) -> anyhow::Result<PathBuf> {
    let safe = |s: &str| s.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "_");
    // An empty name would make `{out}` the PR's folder, and each run empties `{out}`.
    ensure!(
        !safe(prompt).trim_matches('_').is_empty(),
        "prompt names need a letter or digit"
    );
    Ok(Path::new(&safe(pr_id)).join(safe(prompt)))
}

async fn run_task(
    st: &AppState,
    token: u64,
    spec: &Prompt,
    pr: &Target,
    findings: &str,
    run: &mut Run,
) -> anyhow::Result<()> {
    let roots = clone_roots(st);
    let mut wt = local::lookup(&roots, &pr.repo, &pr.branch).await?;
    if wt.path.is_none() {
        run.activity
            .push(format!("creating worktree: wt switch pr:{}", pr.number));
        save_run(st, token, run, false);
        let created = local::create(&roots, &pr.repo, pr.number).await;
        st.emit(Event::WorktreeCreated);
        wt = local::lookup(&roots, &pr.repo, &pr.branch).await?;
        // Another prompt started at the same time may have made it first.
        if wt.path.is_none() {
            created?;
        }
    }
    let path = wt.path.context("no worktree on the PR branch")?;
    let name = path.rsplit('/').next().unwrap_or_default();
    ensure!(
        local::rebasing_branch(&path).is_none(),
        "{name} is in the middle of a rebase: finish it, then re-run"
    );
    // The commit the agent will read: an existing worktree may be behind the PR's head.
    if let Ok(oid) = local::head(&path).await {
        run.head_oid = Some(oid);
    }
    let out = st.data_dir.join("runs").join(run_dir(&pr.id, &spec.name)?);
    // Fresh for each run, so it only holds this run's files.
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out)?;
    let (number, out_str) = (pr.number.to_string(), out.display().to_string());
    let vars = [
        ("path", path.as_str()),
        ("name", name),
        ("branch", &pr.branch),
        ("base", &pr.base),
        ("repo", &pr.repo),
        ("number", &number),
        ("url", &pr.url),
        ("out", &out_str),
        ("review_format", agent::REVIEW_FORMAT),
        // Last, so placeholders written in the findings aren't replaced.
        ("findings", findings),
    ];
    let prompt = local::fill(std::slice::from_ref(&spec.prompt), &vars).remove(0);
    run.path = Some(path.clone());
    let title = format!("{}#{} {}", pr.repo, pr.number, spec.name);
    agent::run(
        spec,
        Path::new(&path),
        &out,
        &title,
        &pr.base,
        &prompt,
        run,
        |r| save_run(st, token, r, false),
    )
    .await
}

/// Runs a configured prompt on a PR in the background, replacing its previous run.
pub fn run_prompt(st: &Arc<AppState>, prompt: &str, pr: Target) -> anyhow::Result<()> {
    let spec = st
        .cfg
        .read()
        .unwrap()
        .prompts
        .iter()
        .find(|p| p.name == prompt)
        .cloned();
    let spec = spec.context("no such prompt in the config")?;
    start_run(st, spec, pr, String::new())
}

/// Name of the fix agent's runs and tab.
const FIX: &str = "fix";

/// Hands findings picked in a review to the `[fix]` agent, as a run of the `fix` prompt.
pub fn fix_findings(st: &Arc<AppState>, pr: Target, findings: &[Finding]) -> anyhow::Result<()> {
    let fix = st.cfg.read().unwrap().fix.clone();
    let fix = fix.context("no [fix] section in the config")?;
    ensure!(!findings.is_empty(), "pick findings to fix first");
    if fix.harness == config::Harness::Greptile {
        bail!("[fix] needs an agent harness: opencode or claude");
    }
    let spec = Prompt {
        name: FIX.into(),
        harness: fix.harness,
        prompt: fix.prompt,
        args: fix.args,
    };
    start_run(st, spec, pr, agent::findings_text(findings))
}

fn start_run(st: &Arc<AppState>, spec: Prompt, pr: Target, findings: String) -> anyhow::Result<()> {
    let prompt = spec.name.clone();
    let run = Run {
        pr_id: pr.id.clone(),
        repo: pr.repo.clone(),
        number: pr.number,
        prompt: prompt.clone(),
        harness: spec.harness,
        started_at: now_ms(),
        head_oid: pr.head_oid.clone(),
        ..Default::default()
    };
    let token = st.run_seq.fetch_add(1, Ordering::Relaxed);
    let key = (pr.id.clone(), prompt);
    // Locked until the run is registered, so the task's first save can't be rejected.
    let mut running = st.runs.lock().unwrap();
    st.db.put_run(&run)?;
    let task = {
        let st = st.clone();
        st.rt.clone().spawn(async move {
            let mut run = run;
            let res = run_task(&st, token, &spec, &pr, &findings, &mut run).await;
            if let Err(e) = res {
                run.error.get_or_insert_with(|| err(e));
            }
            run.status = if run.error.is_some() {
                RunStatus::Error
            } else {
                RunStatus::Done
            };
            run.finished_at = Some(now_ms());
            save_run(&st, token, &run, true);
        })
    };
    if let Some((_, old)) = running.insert(key.clone(), (token, task)) {
        old.abort();
    }
    drop(running);
    st.emit(Event::RunUpdated(key.0));
    Ok(())
}

pub fn cancel_run(st: &AppState, pr_id: &str, prompt: &str) -> anyhow::Result<()> {
    let removed = st
        .runs
        .lock()
        .unwrap()
        .remove(&(pr_id.to_string(), prompt.to_string()));
    let Some((_, task)) = removed else {
        return Ok(());
    };
    task.abort(); // drops the harness process with it
    if let Some(mut run) = st.db.run(pr_id, prompt)? {
        run.status = RunStatus::Cancelled;
        run.finished_at = Some(now_ms());
        run.seen = true; // you cancelled it, nothing to catch up on
        st.db.put_run(&run)?;
    }
    st.emit(Event::RunUpdated(pr_id.to_string()));
    Ok(())
}

/// Reopens a run's agent session interactively with the `terminal` command.
pub async fn continue_run(st: &AppState, pr_id: &str, prompt: &str) -> anyhow::Result<()> {
    let run = st.db.run(pr_id, prompt)?;
    let run = run.context("this prompt hasn't run on this PR")?;
    let command = agent::resume_command(&run).context("no agent session to continue yet")?;
    let path = run.path.unwrap_or_default();
    let name = path.rsplit('/').next().unwrap_or_default();
    let vars = [
        ("command", command.as_str()),
        ("path", &path),
        ("name", name),
    ];
    let terminal = st.cfg.read().unwrap().terminal.clone();
    local::open_with(&terminal, &vars).await
}

/// Posts the picked findings of a run's review to GitHub as one review.
pub async fn post_review(
    st: &AppState,
    pr_id: &str,
    prompt: &str,
    picked: &[usize],
    approve: bool,
) -> anyhow::Result<()> {
    let latest = || -> anyhow::Result<Run> {
        st.db
            .run(pr_id, prompt)?
            .context("this prompt hasn't run on this PR")
    };
    let run = latest()?;
    let review = run
        .review
        .as_ref()
        .context("this run has no findings to post")?;
    ensure!(review.posted.is_none(), "this review was already posted");
    ensure!(
        !picked.is_empty() || approve,
        "pick at least one finding to comment"
    );
    let (url, all_in_body) = sync::post_review(
        &st.gh,
        pr_id,
        run.head_oid.as_deref(),
        review,
        picked,
        approve,
    )
    .await?;
    // Re-read: the prompt may have been re-run while posting.
    let mut now = latest()?;
    if now.started_at == run.started_at
        && let Some(review) = now.review.as_mut()
    {
        review.posted = Some(url);
        review.all_in_body = all_in_body;
        st.db.put_run(&now)?;
    }
    st.emit(Event::RunUpdated(pr_id.to_string()));
    Ok(())
}

/// Approves the commit of a PR that was on screen (`head_oid`), with an optional message, then
/// refreshes the PR to show the approval.
pub async fn approve_pr(
    st: &AppState,
    id: &str,
    head_oid: &str,
    body: String,
) -> anyhow::Result<()> {
    let review = Review {
        summary: body,
        ..Default::default()
    };
    sync::post_review(&st.gh, id, Some(head_oid), &review, &[], true).await?;
    if let Ok(detail) = refresh_detail(st, id).await {
        st.emit(Event::PrUpdated {
            id: id.to_string(),
            detail,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_live_run_saves() {
        let key = ("PR_1".to_string(), "review".to_string());
        let mut running = HashMap::new();
        running.insert(key.clone(), (1, ()));
        assert!(is_current(&running, &key, 1));
        running.insert(key.clone(), (2, ())); // re-run replaces it
        assert!(!is_current(&running, &key, 1));
        assert!(is_current(&running, &key, 2));
        running.remove(&key); // cancelled
        assert!(!is_current(&running, &key, 2));
    }

    #[test]
    fn run_dirs_are_per_pr_and_never_the_pr_folder() {
        assert_eq!(
            run_dir("PR_kw1", "pr review").unwrap(),
            Path::new("PR_kw1/pr_review")
        );
        assert_ne!(run_dir("PR_a", "x").unwrap(), run_dir("PR_b", "x").unwrap());
        assert!(run_dir("PR_1", "").is_err());
        assert!(run_dir("PR_1", " / ").is_err());
    }
}
