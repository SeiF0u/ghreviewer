//! The PR pane: a PR detail (the GraphQL JSON of `sync.rs`) as lines, like `src/PrView.tsx`.

use ansi_to_tui::IntoText;
use ghreviewer_core::{
    inbox::{parse_ms, rel_time, tail_patch},
    jira::Ticket,
};
use ratatui::{
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
};
use serde_json::Value;
use std::{
    io::Write,
    process::{Command, Stdio},
};

// shortcut: colors fit dark terminals; read the config's theme if light ones need it.
pub const DIM: Color = Color::Indexed(244);
pub const ACCENT: Color = Color::Cyan;
const CODE: Color = Color::Indexed(180);
pub const OK: Color = Color::Green;
pub const WARN: Color = Color::Yellow;
pub const FAIL: Color = Color::Red;

fn s<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or("")
}

fn nodes<'a>(v: &'a Value, ptr: &str) -> impl Iterator<Item = &'a Value> {
    let list = v.pointer(ptr).and_then(|n| n["nodes"].as_array());
    list.into_iter().flatten().filter(|x| !x.is_null())
}

fn login(v: &Value) -> &str {
    v["login"].as_str().unwrap_or("ghost")
}

fn ago(iso: &str, now: u64) -> String {
    parse_ms(iso).map(|t| rel_time(t, now)).unwrap_or_default()
}

/// Glyph and color of a check rollup, CheckRun conclusion or status state.
pub fn check_glyph(state: &str) -> (&'static str, Color) {
    match state {
        "SUCCESS" => ("✓", OK),
        "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => {
            ("✗", FAIL)
        }
        "NEUTRAL" | "SKIPPED" | "STALE" => ("○", DIM),
        "" => ("", DIM),
        _ => ("●", WARN),
    }
}

pub fn review_label(state: &str) -> (&'static str, Color) {
    match state {
        "APPROVED" => ("approved", OK),
        "CHANGES_REQUESTED" => ("changes", FAIL),
        "REVIEW_REQUIRED" => ("review", DIM),
        "COMMENTED" => ("commented", DIM),
        "DISMISSED" => ("dismissed", DIM),
        _ => ("pending", DIM),
    }
}

fn review_verb(state: &str) -> &'static str {
    match state {
        "APPROVED" => "approved",
        "CHANGES_REQUESTED" => "requested changes",
        "DISMISSED" => "review dismissed",
        "PENDING" => "pending review",
        _ => "reviewed",
    }
}

/// GitHub's rendered HTML as wrapped text.
pub fn html(raw: &str, width: usize) -> Vec<Line<'static>> {
    use html2text::render::RichAnnotation as A;
    let Ok(lines) = html2text::config::rich().lines_from_read(raw.as_bytes(), width.max(20)) else {
        return raw.lines().map(|l| Line::raw(l.to_string())).collect();
    };
    let span = |s: &str, tags: &[A]| {
        let style = tags.iter().fold(Style::new(), |st, a| match a {
            A::Strong => st.bold(),
            A::Emphasis => st.italic(),
            A::Strikeout => st.crossed_out(),
            A::Code | A::Preformat(_) => st.fg(CODE),
            A::Link(_) => st.underlined(),
            _ => st,
        });
        Span::styled(s.to_string(), style)
    };
    let mut out: Vec<Line<'static>> = lines
        .iter()
        .map(|l| Line::from_iter(l.tagged_strings().map(|t| span(&t.s, &t.tag))))
        .collect();
    while out.last().is_some_and(|l| l.width() == 0) {
        out.pop();
    }
    out
}

/// A patch through `delta`, or colored by hand when it isn't installed.
fn diff(patch: &str, width: usize) -> Vec<Line<'static>> {
    let delta = Command::new("delta")
        .args(["--paging=never", "--file-style=omit", "--width"])
        .arg(width.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(patch.as_bytes())?;
            child.wait_with_output()
        });
    if let Ok(out) = delta
        && out.status.success()
        && let Ok(text) = out.stdout.into_text()
    {
        return text.lines;
    }
    patch
        .lines()
        .skip(2) // the --- / +++ header
        .map(|l| {
            let color = match l.chars().next() {
                Some('+') => OK,
                Some('-') => FAIL,
                Some('@') => ACCENT,
                _ => Color::Reset,
            };
            Line::styled(l.to_string(), color)
        })
        .collect()
}

fn section(title: String) -> [Line<'static>; 2] {
    [
        Line::raw(""),
        Line::styled(title, Modifier::BOLD).fg(ACCENT),
    ]
}

/// Everything the pane shows for a PR, wrapped to `width`.
pub fn lines(pr: &Value, width: usize, now: u64) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let st = |text: String, color: Color| Span::styled(text, color);
    let dim = |text: &str| Span::styled(text.to_string(), DIM);

    out.push(Line::from(vec![
        Span::styled(s(pr, "/title").to_string(), Modifier::BOLD),
        dim(&format!(" #{}", pr["number"])),
    ]));
    let (badge, color) = match (s(pr, "/state"), pr["isDraft"].as_bool()) {
        ("MERGED", _) => ("merged", ACCENT),
        ("CLOSED", _) => ("closed", FAIL),
        (_, Some(true)) => ("draft", DIM),
        _ => ("open", OK),
    };
    out.push(Line::from(vec![
        Span::styled(
            format!(" {badge} "),
            Style::new().fg(Color::Black).bg(color),
        ),
        Span::raw(" "),
        Span::styled(login(&pr["author"]).to_string(), Modifier::BOLD),
        dim(" wants to merge "),
        Span::raw(s(pr, "/headRefName").to_string()),
        dim(" → "),
        Span::raw(s(pr, "/baseRefName").to_string()),
        dim(" · "),
        st(format!("+{}", pr["additions"]), OK),
        Span::raw(" "),
        st(format!("-{}", pr["deletions"]), FAIL),
        dim(&format!(" · {} files", pr["changedFiles"])),
    ]));
    out.push(Line::styled(s(pr, "/url").to_string(), DIM));
    out.push(Line::raw(""));

    let key = |k: &str| Span::styled(format!("{k:<10}"), DIM);
    let mut reviewers = vec![key("reviewers")];
    for r in nodes(pr, "/reviewRequests") {
        let who = &r["requestedReviewer"];
        let who = who["login"].as_str().map(String::from);
        let who = who.unwrap_or_else(|| format!("@{}", s(r, "/requestedReviewer/name")));
        reviewers.extend([Span::raw(format!("{who} ")), dim("pending  ")]);
    }
    for r in nodes(pr, "/latestReviews") {
        let (label, color) = review_label(s(r, "/state"));
        reviewers.extend([
            Span::raw(format!("{} ", login(&r["author"]))),
            st(format!("{label}  "), color),
        ]);
    }
    if reviewers.len() == 1 {
        reviewers.push(dim("none"));
    }
    out.push(Line::from(reviewers));
    let assignees: Vec<&str> = nodes(pr, "/assignees").map(login).collect();
    if !assignees.is_empty() {
        out.push(Line::from(vec![
            key("assignees"),
            Span::raw(assignees.join(" ")),
        ]));
    }
    let labels: Vec<&str> = nodes(pr, "/labels").map(|l| s(l, "/name")).collect();
    if !labels.is_empty() {
        out.push(Line::from(vec![
            key("labels"),
            Span::raw(labels.join(", ")),
        ]));
    }
    out.push(checks(pr, key("checks")));
    let merge_color = match (s(pr, "/mergeable"), s(pr, "/mergeStateStatus")) {
        ("CONFLICTING", _) => FAIL,
        (_, "CLEAN") => OK,
        _ => Color::Reset,
    };
    out.push(Line::from(vec![
        key("merge"),
        st(
            format!(
                "{} · {}",
                s(pr, "/mergeable").to_lowercase(),
                s(pr, "/mergeStateStatus").to_lowercase()
            ),
            merge_color,
        ),
    ]));
    out.push(Line::from(vec![
        key("opened"),
        Span::raw(format!("{} ago", ago(s(pr, "/createdAt"), now))),
    ]));

    out.extend(section("Description".into()));
    match s(pr, "/bodyHTML") {
        "" => out.push(Line::styled("No description.", DIM)),
        body => out.extend(html(body, width)),
    }

    let timeline: Vec<&Value> = nodes(pr, "/timelineItems").collect();
    out.extend(section(format!("Conversation · {}", timeline.len())));
    for it in timeline {
        entry(&mut out, it, width, now);
    }

    let threads: Vec<&Value> = nodes(pr, "/reviewThreads").collect();
    if !threads.is_empty() {
        let open = threads.iter().filter(|t| t["isResolved"] != true).count();
        out.extend(section(format!(
            "Review threads · {open} open / {}",
            threads.len()
        )));
        for t in threads {
            thread(&mut out, t, width, now);
        }
    }
    out
}

fn checks(pr: &Value, key: Span<'static>) -> Line<'static> {
    let rollup = nodes(pr, "/commits")
        .next()
        .map(|c| &c["commit"]["statusCheckRollup"])
        .filter(|r| !r.is_null());
    let Some(rollup) = rollup else {
        return Line::from(vec![key, Span::styled("none", DIM)]);
    };
    let state = |c: &Value| -> String {
        match (s(c, "/__typename"), s(c, "/status")) {
            ("CheckRun", "COMPLETED") => s(c, "/conclusion").into(),
            ("CheckRun", _) => "PENDING".into(),
            _ => s(c, "/state").into(),
        }
    };
    let mut tally: Vec<(&str, usize)> = Vec::new();
    let mut failing = Vec::new();
    for c in nodes(rollup, "/contexts") {
        let (glyph, color) = check_glyph(&state(c));
        match tally.iter_mut().find(|(g, _)| *g == glyph) {
            Some((_, n)) => *n += 1,
            None => tally.push((glyph, 1)),
        }
        if color == FAIL {
            failing.push(c["name"].as_str().or(c["context"].as_str()).unwrap_or("?"));
        }
    }
    let (glyph, color) = check_glyph(s(rollup, "/state"));
    let tally: Vec<String> = tally.iter().map(|(g, n)| format!("{n}{g}")).collect();
    let mut line = vec![
        key,
        Span::styled(glyph, color),
        Span::raw(format!(" {} ", s(rollup, "/state").to_lowercase())),
        Span::styled(format!("({})", tally.join(" ")), DIM),
    ];
    if !failing.is_empty() {
        line.push(Span::styled(format!(" {}", failing.join(", ")), FAIL));
    }
    Line::from(line)
}

fn entry(out: &mut Vec<Line<'static>>, it: &Value, width: usize, now: u64) {
    let kind = s(it, "/__typename");
    let who = it["author"]["login"]
        .as_str()
        .or(it["actor"]["login"].as_str())
        .unwrap_or("ghost")
        .to_string();
    let when = Span::styled(format!(" · {}", ago(s(it, "/createdAt"), now)), DIM);
    let bold = |t: String| Span::styled(t, Modifier::BOLD);
    if kind == "PullRequestCommit" {
        let c = &it["commit"];
        let by = c["author"]["user"]["login"]
            .as_str()
            .or(c["author"]["name"].as_str())
            .unwrap_or("");
        out.push(Line::from(vec![
            Span::styled(format!("● {} ", s(c, "/abbreviatedOid")), DIM),
            Span::raw(s(c, "/messageHeadline").to_string()),
            Span::styled(
                format!(" · {by} · {}", ago(s(c, "/committedDate"), now)),
                DIM,
            ),
        ]));
        return;
    }
    let review = kind == "PullRequestReview";
    if kind == "IssueComment" || review {
        let (_, color) = review_label(s(it, "/state"));
        let verb = if review {
            Span::styled(review_verb(s(it, "/state")), color)
        } else {
            Span::raw("commented")
        };
        let body = s(it, "/bodyHTML");
        let n = it["comments"]["totalCount"].as_u64().unwrap_or(0);
        let inline = if review && body.is_empty() && n > 0 {
            format!(" with {n} inline comment{}", if n > 1 { "s" } else { "" })
        } else {
            String::new()
        };
        if !body.is_empty() {
            out.push(Line::raw(""));
        }
        out.push(Line::from(vec![
            bold(who),
            Span::raw(" "),
            verb,
            Span::raw(inline),
            when,
        ]));
        if !body.is_empty() {
            out.extend(html(body, width));
        }
        return;
    }
    let reviewer = &it["requestedReviewer"];
    let reviewer = reviewer["login"].as_str().map(String::from);
    let reviewer = reviewer.unwrap_or_else(|| format!("@{}", s(it, "/requestedReviewer/name")));
    let text = match kind {
        "ReviewRequestedEvent" => format!("requested review from {reviewer}"),
        "ReadyForReviewEvent" => "marked ready for review".into(),
        "ConvertToDraftEvent" => "converted to draft".into(),
        "HeadRefForcePushedEvent" => "force-pushed".into(),
        "MergedEvent" => "merged".into(),
        "ClosedEvent" => "closed".into(),
        "ReopenedEvent" => "reopened".into(),
        "LabeledEvent" => format!("added label {}", s(it, "/label/name")),
        other => other.into(),
    };
    out.push(Line::from(vec![
        Span::styled(who, DIM),
        Span::styled(format!(" {text}"), DIM),
        when,
    ]));
}

fn thread(out: &mut Vec<Line<'static>>, t: &Value, width: usize, now: u64) {
    let comments: Vec<&Value> = nodes(t, "/comments").collect();
    let line = t["line"].as_u64().or(t["originalLine"].as_u64());
    let path = s(t, "/path");
    let (resolved, outdated) = (t["isResolved"] == true, t["isOutdated"] == true);
    let mut head = vec![
        Span::raw(""),
        Span::styled(
            format!(
                "{path}{}",
                line.map(|l| format!(":{l}")).unwrap_or_default()
            ),
            Modifier::BOLD,
        ),
    ];
    if resolved {
        head.push(Span::styled(" resolved", OK));
    }
    if outdated {
        head.push(Span::styled(" outdated", DIM));
    }
    let n = comments.len();
    head.push(Span::styled(
        format!(" · {n} comment{}", if n == 1 { "" } else { "s" }),
        DIM,
    ));
    out.push(Line::raw(""));
    out.push(Line::from(head));
    // Like the app, resolved and outdated threads stay folded to their header.
    if resolved || outdated {
        return;
    }
    if let Some(first) = comments.first() {
        out.extend(diff(&tail_patch(path, s(first, "/diffHunk"), 8), width));
    }
    for c in comments {
        out.push(Line::from(vec![
            Span::styled(login(&c["author"]).to_string(), Modifier::BOLD),
            Span::styled(format!(" · {}", ago(s(c, "/createdAt"), now)), DIM),
        ]));
        out.extend(html(s(c, "/bodyHTML"), width));
    }
}

/// Color of a Jira status category: to do, in progress, done.
pub fn jira_color(category: &str) -> Color {
    match category {
        "indeterminate" => ACCENT,
        "done" => OK,
        _ => DIM,
    }
}

/// The Jira tab: ticket `t`, among the `keys` named in the PR title. Like `src/JiraView.tsx`.
pub fn jira_lines(t: &Ticket, keys: &[String], width: usize, now: u64) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    if keys.len() > 1 {
        let mut tabs = vec![];
        for (i, k) in keys.iter().enumerate() {
            if i > 0 {
                tabs.push(Span::styled(" │ ", DIM));
            }
            let style = if *k == t.key {
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(DIM)
            };
            tabs.push(Span::styled(k.clone(), style));
        }
        tabs.push(Span::styled("  { } tickets", DIM));
        out.push(Line::from(tabs));
        out.push(Line::raw(""));
    }
    out.push(Line::from(vec![
        Span::styled(t.summary.clone(), Modifier::BOLD),
        Span::styled(format!(" {}", t.key), DIM),
    ]));
    out.push(Line::from(vec![
        Span::styled(
            format!(" {} ", t.status.to_lowercase()),
            Style::new().fg(Color::Black).bg(jira_color(&t.category)),
        ),
        Span::raw(format!(" {}", t.kind)),
    ]));
    out.push(Line::styled(t.url.clone(), DIM));
    out.push(Line::raw(""));
    let key = |k: &str| Span::styled(format!("{k:<10}"), DIM);
    let assignee = Some(t.assignee.clone().unwrap_or_else(|| "unassigned".into()));
    for (k, v) in [
        ("assignee", assignee),
        ("reporter", t.reporter.clone()),
        ("priority", t.priority.clone()),
    ] {
        if let Some(v) = v {
            out.push(Line::from(vec![key(k), Span::raw(v)]));
        }
    }
    if let Some(p) = &t.parent {
        out.push(Line::from(vec![
            key("parent"),
            Span::raw(format!("{} {} ", p.key, p.summary)),
            Span::styled(p.kind.clone(), DIM),
        ]));
    }
    if !t.labels.is_empty() {
        out.push(Line::from(vec![
            key("labels"),
            Span::raw(t.labels.join(", ")),
        ]));
    }
    out.extend(section("Description".into()));
    match t.description_html.as_str() {
        "" => out.push(Line::styled("No description.", DIM)),
        body => out.extend(html(body, width)),
    }
    out.extend(section(format!("Comments · {}", t.comments.len())));
    for c in &t.comments {
        out.push(Line::raw(""));
        out.push(Line::from(vec![
            Span::styled(c.author.clone(), Modifier::BOLD),
            Span::styled(format!(" · {}", ago(&c.created, now)), DIM),
        ]));
        out.extend(html(&c.body_html, width));
    }
    out
}
