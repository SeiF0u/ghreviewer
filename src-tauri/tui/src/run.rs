//! A prompt tab: the latest run of a prompt on the PR, like `src/RunView.tsx`.

use crate::pr::{ACCENT, DIM, FAIL, OK, WARN};
use ghreviewer_core::{
    agent::{Run, RunStatus},
    config::Prompt,
    inbox::rel_time,
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
            text(&mut lines, &review.summary, Style::new());
        }
        for (i, f) in review.findings.iter().enumerate() {
            findings.push(lines.len() + 1);
            lines.push(Line::raw(""));
            let on = a.picked.get(i).copied().unwrap_or(false);
            let cursor = i == a.cursor;
            let mut head = vec![
                Span::styled(if cursor { "▸ " } else { "  " }, ACCENT),
                Span::raw(if on { "[x] " } else { "[ ] " }),
                Span::styled(
                    format!(
                        "{} ",
                        if f.severity.is_empty() {
                            "note"
                        } else {
                            &f.severity
                        }
                    ),
                    severity_color(&f.severity),
                ),
                Span::styled(f.title.clone(), Modifier::BOLD),
            ];
            if let Some(path) = &f.path {
                let line = f.line.map(|l| format!(":{l}")).unwrap_or_default();
                head.push(Span::styled(format!(" {path}{line}"), DIM));
            }
            let style = if on {
                Style::new()
            } else {
                Style::new().fg(DIM)
            };
            let head = Line::from(head).style(if cursor {
                style.bg(Color::Indexed(236))
            } else {
                style
            });
            lines.push(head);
            for l in wrap(&f.body, a.width.saturating_sub(4)) {
                lines.push(Line::styled(format!("    {l}"), style));
            }
        }
    }

    if !run.output.is_empty() {
        lines.push(Line::raw(""));
        text(&mut lines, &run.output, Style::new());
    }
    if !run.files.is_empty() {
        lines.push(Line::raw(""));
        lines.push(dim("Files".into()));
        for f in &run.files {
            text(&mut lines, f, Style::new().fg(DIM));
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
    fn durations() {
        assert_eq!(duration(4_000), "4s");
        assert_eq!(duration(62_000), "1m02s");
    }
}
