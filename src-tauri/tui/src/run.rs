//! A prompt tab: the latest run of a prompt on the PR, like `src/RunView.tsx`.

use crate::pr::{ACCENT, DIM, FAIL, OK, SEL, WARN, html};
use ghreviewer_core::{
    agent::{Finding, Run, RunStatus},
    config::Prompt,
    inbox::{BotReview, rel_time},
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Words of `text` in lines of at most `width` columns; a longer word gets cut.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut out = Vec::new();
    for para in text.lines() {
        let indent: String = para.chars().take_while(|c| *c == ' ').collect();
        let mut line = String::new();
        for word in para.split(' ').filter(|w| !w.is_empty()) {
            let len = line.chars().count();
            if len > 0 && len + 1 + word.chars().count() > width {
                out.push(std::mem::take(&mut line));
            }
            if line.is_empty() {
                line.push_str(&indent);
            } else {
                line.push(' ');
            }
            line.push_str(word);
            while line.chars().count() > width {
                let rest: String = line.chars().skip(width).collect();
                out.push(line.chars().take(width).collect());
                line = rest;
            }
        }
        out.push(line);
    }
    out
}

/// Markdown (GitHub's flavour) in lines of `width`, like the desktop's `marked`.
pub fn markdown(text: &str, width: usize) -> Vec<Line<'static>> {
    use pulldown_cmark::{Options, Parser};
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, Parser::new_ext(text, opts));
    html(&out, width)
}

/// `items` joined by ` · `, onto as many lines of `width` as they need.
fn pieces(items: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    for item in items {
        let line = lines.last_mut().unwrap();
        if line.width() == 0 {
            line.push_span(item);
        } else if line.width() + 3 + item.width() <= width {
            line.push_span(Span::styled(" · ", DIM));
            line.push_span(item);
        } else {
            lines.push(Line::from(item));
        }
    }
    lines
}

/// A run's file by its name: they all sit in the run's folder.
pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn duration(ms: u64) -> String {
    let s = (ms + 500) / 1000;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

fn severity_color(severity: &str) -> Color {
    match severity.split(' ').next() {
        Some("blocker" | "P0") => FAIL,
        Some("should-fix" | "P1") => WARN,
        _ => DIM,
    }
}

/// Findings to pick, added to `lines`; returns the line where each one starts.
fn finding_lines(
    lines: &mut Vec<Line<'static>>,
    findings: &[Finding],
    picked: &[bool],
    cursor: usize,
    width: usize,
) -> Vec<usize> {
    let mut starts = Vec::new();
    for (i, f) in findings.iter().enumerate() {
        lines.push(Line::raw(""));
        starts.push(lines.len());
        let on = picked.get(i).copied().unwrap_or(false);
        let style = if on {
            Style::new()
        } else {
            Style::new().fg(DIM)
        };
        let severity = if f.severity.is_empty() {
            "note"
        } else {
            &f.severity
        };
        let head = format!("{} {} ", if on { "[x]" } else { "[ ]" }, severity);
        // The title on the head line, wrapped under it when long.
        let room = width.saturating_sub(2 + head.chars().count());
        let mut title = wrap(&f.title, room).into_iter();
        let mut first = Line::from(vec![
            Span::styled(if i == cursor { "▸ " } else { "  " }, ACCENT),
            Span::raw(head.clone()).patch_style(severity_color(severity)),
            Span::styled(title.next().unwrap_or_default(), Modifier::BOLD),
        ])
        .style(style);
        if i == cursor {
            first = first.patch_style(Style::new().bg(SEL));
        }
        lines.push(first);
        let indent = " ".repeat(2 + head.chars().count());
        for rest in title {
            lines.push(Line::styled(
                format!("{indent}{rest}"),
                style.add_modifier(Modifier::BOLD),
            ));
        }
        if let Some(path) = &f.path {
            let line = f.line.map(|l| format!(":{l}")).unwrap_or_default();
            for l in wrap(&format!("{path}{line}"), width.saturating_sub(4)) {
                lines.push(Line::styled(format!("    {l}"), DIM));
            }
        }
        for mut l in markdown(&f.body, width.saturating_sub(4)) {
            l.spans.insert(0, Span::raw("    "));
            lines.push(l.patch_style(style));
        }
    }
    starts
}

/// A review bot's open threads on the PR, to pick and hand to the fix agent.
pub fn bot_view(
    bot: &BotReview,
    picked: &[bool],
    cursor: usize,
    can_fix: bool,
    width: usize,
) -> View {
    let n = picked.iter().filter(|p| **p).count();
    let mut lines = vec![
        Line::from(vec![
            Span::styled(bot.bot.clone(), Modifier::BOLD),
            Span::styled(" open review threads", DIM),
        ]),
        Line::styled(
            format!("Findings · {n}/{} picked", bot.findings.len()),
            ACCENT,
        ),
    ];
    let actions = if can_fix {
        vec![format!("f fix ({n})"), "j/k Space pick".into()]
    } else {
        vec!["add [fix] to the config to fix these".into()]
    };
    lines.extend(pieces(
        actions.into_iter().map(|t| Span::styled(t, DIM)).collect(),
        width,
    ));
    let findings = finding_lines(&mut lines, &bot.findings, picked, cursor, width);
    View { lines, findings }
}

/// What a prompt tab shows, and the line where each finding starts (to scroll to the cursor).
pub struct View {
    pub lines: Vec<Line<'static>>,
    pub findings: Vec<usize>,
}

pub struct Args<'a> {
    pub prompt: &'a Prompt,
    pub run: Option<&'a Run>,
    pub stale: bool,
    pub has_worktree: bool,
    pub picked: &'a [bool],
    pub cursor: usize,
    /// Whether picked findings can go to the fix agent.
    pub can_fix: bool,
    pub width: usize,
    pub now: u64,
}

pub fn view(a: Args) -> View {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut findings = Vec::new();
    let dim = |t: String| Line::styled(t, DIM);
    let text = |lines: &mut Vec<Line<'static>>, t: &str, style: Style| {
        lines.extend(wrap(t, a.width).into_iter().map(|l| Line::styled(l, style)));
    };
    let fix = a.prompt.name == ghreviewer_core::FIX;

    lines.push(Line::from(vec![
        Span::styled(a.prompt.name.clone(), Modifier::BOLD),
        Span::styled(format!(" {:?}", a.prompt.harness).to_lowercase(), DIM),
    ]));
    let Some(run) = a.run else {
        let how = if fix {
            "f from picked findings"
        } else {
            "r to run"
        };
        let wt = if a.has_worktree || fix {
            ""
        } else {
            " · creates the worktree first"
        };
        lines.push(Line::styled(format!("{how}{wt}"), ACCENT));
        if !a.prompt.prompt.is_empty() {
            lines.push(Line::raw(""));
            text(&mut lines, &a.prompt.prompt, Style::new().fg(DIM));
        }
        return View { lines, findings };
    };

    let mut status = vec![match run.status {
        RunStatus::Running => Span::styled(
            format!("running {}", duration(a.now.saturating_sub(run.started_at))),
            WARN,
        ),
        RunStatus::Done => Span::styled("done", OK),
        RunStatus::Error => Span::styled("failed", FAIL),
        RunStatus::Cancelled => Span::styled("cancelled", WARN),
        RunStatus::Interrupted => Span::styled("interrupted (app quit)", WARN),
    }];
    if let Some(end) = run.finished_at {
        let ago = match rel_time(end, a.now).as_str() {
            "now" => "just now".to_string(),
            rel => format!("{rel} ago"),
        };
        let took = duration(end.saturating_sub(run.started_at));
        status.extend([
            Span::styled(ago, DIM),
            Span::styled(format!("took {took}"), DIM),
        ]);
    }
    if a.stale {
        status.push(Span::styled("stale: new commits since this run", WARN));
    }
    lines.extend(pieces(status, a.width));
    let mut keys = vec![];
    if !fix {
        keys.push("r re-run");
    }
    if run.session_id.is_some() {
        keys.push("c continue in terminal");
    }
    if run.status == RunStatus::Running {
        keys.push("X cancel");
    }
    lines.extend(pieces(
        keys.into_iter().map(|k| Span::styled(k, DIM)).collect(),
        a.width,
    ));
    if let Some(e) = &run.error {
        lines.push(Line::raw(""));
        text(&mut lines, e, Style::new().fg(FAIL));
    }

    if let Some(review) = &run.review {
        let n = a.picked.iter().filter(|p| **p).count();
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("Findings · {n}/{} picked", review.findings.len()),
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        let mut actions = vec![];
        if let Some(url) = &review.posted {
            actions.push(format!("posted: {url}"));
            if review.all_in_body {
                actions.push("GitHub rejected the inline comments: all are in the body".into());
            }
        } else {
            actions.push(format!("p comment ({n})"));
            actions.push(match n {
                0 => "a approve".into(),
                n => format!("a approve with {n}"),
            });
        }
        if a.can_fix {
            actions.push(format!("f fix ({n})"));
        }
        actions.push("j/k Space pick".into());
        let actions = actions.into_iter().map(|t| Span::styled(t, DIM)).collect();
        lines.extend(pieces(actions, a.width));
        if !review.summary.is_empty() {
            lines.push(Line::raw(""));
            lines.extend(markdown(&review.summary, a.width));
        }
        findings = finding_lines(&mut lines, &review.findings, a.picked, a.cursor, a.width);
    }

    if !run.output.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(markdown(&run.output, a.width));
    }
    if !run.files.is_empty() {
        lines.push(Line::raw(""));
        lines.push(dim("Files · O open".into()));
        for (i, f) in run.files.iter().enumerate() {
            let n = if run.files.len() > 1 {
                format!("{} ", i + 1)
            } else {
                String::new()
            };
            text(&mut lines, &format!("{n}{}", file_name(f)), Style::new());
        }
    }
    if !run.activity.is_empty() {
        let running = run.status == RunStatus::Running;
        let n = run.activity.len();
        let session = run
            .session_id
            .as_ref()
            .map(|s| format!(" · session {s}"))
            .unwrap_or_default();
        lines.push(Line::raw(""));
        lines.push(dim(format!(
            "{n} step{}{session}",
            if n == 1 { "" } else { "s" }
        )));
        // While it runs, the latest steps; once done, all of them.
        let shown = if running {
            &run.activity[n.saturating_sub(15)..]
        } else {
            &run.activity[..]
        };
        for step in shown {
            text(&mut lines, step, Style::new().fg(DIM));
        }
    }
    View { lines, findings }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_on_words_and_cuts_long_ones() {
        assert_eq!(wrap("aaa bbb ccc", 10), ["aaa bbb", "ccc"]);
        assert_eq!(wrap("a\n\nb", 10), ["a", "", "b"]);
        assert_eq!(wrap("  - item one two", 10), ["  - item", "  one two"]);
        assert_eq!(wrap("abcdefghijklmn", 10), ["abcdefghij", "klmn"]);
    }

    #[test]
    fn pieces_wrap_between_items() {
        let items = ["done", "1d ago", "took 6m40s"].map(Span::raw).to_vec();
        let text: Vec<String> = pieces(items, 16).iter().map(Line::to_string).collect();
        assert_eq!(text, ["done · 1d ago", "took 6m40s"]);
    }

    #[test]
    fn markdown_renders_lists_and_code() {
        let lines = markdown("# Title\n\n- **one**\n- `two`", 40);
        let text: Vec<String> = lines.iter().map(Line::to_string).collect();
        assert_eq!(text, ["Title", "• one", "• two"]);
        let one = lines[1].spans.iter().find(|s| s.content == "one").unwrap();
        assert!(one.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn durations() {
        assert_eq!(duration(4_000), "4s");
        assert_eq!(duration(62_000), "1m02s");
    }
}
