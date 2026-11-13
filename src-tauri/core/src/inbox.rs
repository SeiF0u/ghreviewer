//! Inbox logic for the TUI, ported from `src/model.ts`: keep the two in step, with the same tests.

use crate::agent::Finding;
use crate::sync::{Next, PrSummary};
use serde_json::Value;
use std::collections::HashMap;

/// Smart inbox ranks, most urgent first.
const NEXT_ORDER: [Next; 8] = [
    Next::Back,
    Next::Feedback,
    Next::Updated,
    Next::Failing,
    Next::New,
    Next::Ready,
    Next::Waiting,
    Next::Done,
];

/// Nothing to do on it for now: below the smart inbox's "can wait" line.
pub fn can_wait(pr: &PrSummary) -> bool {
    matches!(pr.next, Some(Next::Waiting | Next::Done)) || pr.snoozed_until.is_some()
}

// Snoozed PRs come last among those that can wait.
fn rank(p: &PrSummary) -> usize {
    match (p.snoozed_until, p.next) {
        (Some(_), _) => NEXT_ORDER.len(),
        (None, Some(n)) => NEXT_ORDER.iter().position(|&o| o == n).unwrap(),
        (None, None) => NEXT_ORDER.len() + 1,
    }
}

/// Most urgent first, newest first within a rank (the sort is stable).
pub fn by_next(prs: &mut [PrSummary]) {
    prs.sort_by_key(rank);
}

pub fn matches_filter(pr: &PrSummary, filter: &str) -> bool {
    let f = filter.to_lowercase();
    f.is_empty()
        || format!("{}#{} {} {}", pr.repo, pr.number, pr.title, pr.author)
            .to_lowercase()
            .contains(&f)
}

/// Tree lines of a stacked row at `depth`. `last`: no sibling below it, so its `└` doesn't carry
/// on. `down`: a line drops from it to its children. `through`: shallower levels whose line passes.
#[derive(Debug, Clone, PartialEq)]
pub struct Graph {
    pub depth: usize,
    pub last: bool,
    pub down: bool,
    pub through: Vec<usize>,
}

/// A row of the inbox: `pr` indexes the list given to `stack_rows`.
#[derive(Debug, Clone, PartialEq)]
pub struct PrRow {
    pub pr: usize,
    pub dim: bool,
    pub graph: Option<Graph>,
}

/// `all` (newest first) grouped into stacks, a PR sitting under the one whose head branch it merges
/// into. Stacks come in order of their newest shown PR. Ancestors of a shown PR come along dimmed.
pub fn stack_rows(all: &[PrSummary], shown: impl Fn(&PrSummary) -> bool) -> Vec<PrRow> {
    let n = all.len();
    let mut by_head = HashMap::new();
    for (i, p) in all.iter().enumerate() {
        if let Some(head) = &p.head_ref {
            by_head.insert((p.repo.as_str(), head.as_str()), i);
        }
    }
    let parent_of = |i: usize| {
        let p = &all[i];
        by_head
            .get(&(p.repo.as_str(), p.base_ref.as_str()))
            .copied()
            .filter(|&q| q != i)
    };
    let visible: Vec<usize> = (0..n).filter(|&i| shown(&all[i])).collect();
    let mut in_tree = vec![false; n];
    let mut dim = vec![false; n];
    for &p in &visible {
        in_tree[p] = true;
    }
    for &p in &visible {
        let mut q = parent_of(p);
        while let Some(x) = q.filter(|&x| !in_tree[x]) {
            in_tree[x] = true;
            dim[x] = true;
            q = parent_of(x);
        }
    }
    let mut kids = vec![Vec::new(); n];
    for p in (0..n).filter(|&p| in_tree[p]) {
        if let Some(q) = parent_of(p).filter(|&q| in_tree[q]) {
            kids[q].push(p);
        }
    }

    struct Tree<'a> {
        kids: &'a [Vec<usize>],
        dim: &'a [bool],
        done: Vec<bool>,
        rows: Vec<PrRow>,
    }
    // `open[c]`: the line at level `c` continues below this subtree, to a later sibling.
    fn place(t: &mut Tree, p: usize, depth: usize, last: bool, open: Vec<usize>) {
        t.done[p] = true;
        let ks: Vec<usize> = t.kids[p].iter().copied().filter(|&k| !t.done[k]).collect();
        let below = if depth > 0 && !last {
            [open.clone(), vec![depth - 1]].concat()
        } else {
            open.clone()
        };
        t.rows.push(PrRow {
            pr: p,
            dim: t.dim[p],
            graph: Some(Graph {
                depth,
                last,
                down: !ks.is_empty(),
                through: open,
            }),
        });
        for (i, &k) in ks.iter().enumerate() {
            if !t.done[k] {
                place(t, k, depth + 1, i == ks.len() - 1, below.clone());
            }
        }
    }
    let mut t = Tree {
        kids: &kids,
        dim: &dim,
        done: vec![false; n],
        rows: Vec::new(),
    };
    for &p in &visible {
        let mut root = p;
        let mut climbed = vec![p];
        // `climbed` stops at a branch cycle (a → b → a) instead of looping.
        let mut q = parent_of(p);
        while let Some(x) = q.filter(|&x| in_tree[x] && !climbed.contains(&x)) {
            climbed.push(x);
            root = x;
            q = parent_of(x);
        }
        if t.done[root] {
            continue;
        }
        let start = t.rows.len();
        place(&mut t, root, 0, true, Vec::new());
        if t.rows.len() - start == 1 {
            t.rows[start].graph = None;
        }
    }
    t.rows
}

/// The most urgent shown PR of each stack, at the stack's first row (`None` on the others).
pub fn stack_leads(all: &[PrSummary], rows: &[PrRow]) -> Vec<Option<usize>> {
    let mut leads = vec![None; rows.len()];
    let mut start = 0;
    for (i, r) in rows.iter().enumerate() {
        if r.graph.as_ref().is_none_or(|g| g.depth == 0) {
            start = i;
        }
        if !r.dim && leads[start].is_none_or(|l: usize| rank(&all[r.pr]) < rank(&all[l])) {
            leads[start] = Some(r.pr);
        }
    }
    leads
}

/// Whether `viewer` can approve the PR detail: open, someone else's, and its head not approved by
/// them yet. Uses their latest opinionated review: replying to a thread after approving makes a
/// newer "commented" review that doesn't undo the approval.
pub fn can_approve(pr: &Value, viewer: Option<&str>) -> bool {
    let Some(viewer) = viewer else { return false };
    if pr["state"] != "OPEN" || pr["author"]["login"] == viewer {
        return false;
    }
    let reviews = pr["latestOpinionatedReviews"]["nodes"].as_array();
    let mine = reviews
        .into_iter()
        .flatten()
        .find(|r| r["author"]["login"] == viewer);
    !mine.is_some_and(|r| r["state"] == "APPROVED" && r["commit"]["oid"] == pr["headRefOid"])
}

/// Findings ticked until you change it: all but nits and Greptile's lowest priority.
pub fn picked_by_default(f: &Finding) -> bool {
    !matches!(f.severity.split(' ').next(), Some("nit" | "P2"))
}

/// Milliseconds since the epoch of a GitHub timestamp (`2026-01-09T23:59:30Z`).
// shortcut: reads the first 19 characters as UTC, which GitHub always sends; Jira's offsets need more.
pub fn parse_ms(iso: &str) -> Option<u64> {
    let n = |r: std::ops::Range<usize>| iso.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(((days * 24 + hh) * 60 + mm) * 60 + ss)
        .ok()
        .map(|s| s * 1000)
}

pub fn rel_time(then_ms: u64, now_ms: u64) -> String {
    let s = now_ms.saturating_sub(then_ms) / 1000;
    let (min, hour, day) = (60, 3600, 86_400);
    match s {
        _ if s < min => "now".into(),
        _ if s < hour => format!("{}m", s / min),
        _ if s < day => format!("{}h", s / hour),
        _ if s < day * 30 => format!("{}d", s / day),
        _ if s < day * 365 => format!("{}mo", s / (day * 30)),
        _ => format!("{}y", s / (day * 365)),
    }
}

/// A unified-diff patch of the last `n` lines of a review comment's hunk. GitHub's diffHunk ends
/// at the commented line, so the tail is the useful context; the `@@` header is recomputed so line
/// numbers stay right.
pub fn tail_patch(path: &str, hunk: &str, n: usize) -> String {
    let files = format!("--- a/{path}\n+++ b/{path}\n");
    let mut lines = hunk.strip_suffix('\n').unwrap_or(hunk).split('\n');
    let head = lines.next().unwrap_or_default();
    let body: Vec<&str> = lines.collect();
    let Some((old_at, new_at, rest)) = parse_hunk_head(head) else {
        return files + hunk;
    };
    let (skip, keep) = body.split_at(body.len().saturating_sub(n));
    let old = |l: &&str| l.is_empty() || l.starts_with([' ', '-']);
    let neu = |l: &&str| l.is_empty() || l.starts_with([' ', '+']);
    let count = |ls: &[&str], f: &dyn Fn(&&str) -> bool| ls.iter().filter(|l| f(l)).count();
    format!(
        "{files}@@ -{},{} +{},{} @@{rest}\n{}\n",
        old_at + count(skip, &old),
        count(keep, &old),
        new_at + count(skip, &neu),
        count(keep, &neu),
        keep.join("\n"),
    )
}

/// `@@ -10,4 +10,5 @@ rest` → (10, 10, " rest").
fn parse_hunk_head(head: &str) -> Option<(usize, usize, &str)> {
    let (ranges, rest) = head.strip_prefix("@@ -")?.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let start = |r: &str| r.split(',').next()?.parse().ok();
    Some((start(old)?, start(new)?, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(repo: &str, next: Option<Next>) -> PrSummary {
        PrSummary {
            id: String::new(),
            number: 0,
            title: String::new(),
            url: String::new(),
            repo: repo.into(),
            author: String::new(),
            is_draft: false,
            state: "OPEN".into(),
            created_at: String::new(),
            updated_at: String::new(),
            review_decision: None,
            checks: None,
            last_others_activity_at: None,
            rules: Vec::new(),
            unread: false,
            base_ref: "main".into(),
            head_ref: None,
            context: false,
            jira: Vec::new(),
            next,
            snoozed_until: None,
        }
    }

    #[test]
    fn smart_inbox_ranks_by_state_and_stacks_by_their_most_urgent_pr() {
        use Next::*;
        let mut inbox = vec![
            pr("a/1", Some(Waiting)),
            pr("a/2", Some(New)),
            pr("a/3", Some(Back)),
            pr("a/4", None),
            pr("a/5", Some(New)),
            pr("a/6", Some(Done)),
        ];
        assert!(can_wait(&inbox[0]) && can_wait(&inbox[5]) && !can_wait(&inbox[3]));
        // A snoozed PR can wait, whatever its state, and comes after the ones that are done.
        let mut snoozed = pr("a/7", Some(Back));
        snoozed.snoozed_until = Some(1);
        assert!(can_wait(&snoozed));
        inbox.insert(0, snoozed);
        by_next(&mut inbox);
        let repos: Vec<&str> = inbox.iter().map(|p| p.repo.as_str()).collect();
        assert_eq!(repos, ["a/3", "a/2", "a/5", "a/1", "a/6", "a/7", "a/4"]);

        // A waiting root with a `back` child ranks as `back`; its greyed parent doesn't count.
        let stacked = |next, head: &str, base: &str, context| PrSummary {
            head_ref: Some(head.into()),
            base_ref: base.into(),
            context,
            title: head.into(),
            ..pr("o/r", Some(next))
        };
        let mut all = vec![
            stacked(Done, "d", "main", false),
            stacked(Waiting, "a", "p", false),
            stacked(New, "c", "main", false),
            stacked(Back, "b", "a", false),
            stacked(New, "p", "main", true),
        ];
        by_next(&mut all);
        let rows = stack_rows(&all, |p| !p.context);
        let title = |i: usize| all[i].title.as_str();
        let order: Vec<&str> = rows.iter().map(|r| title(r.pr)).collect();
        assert_eq!(order, ["p", "a", "b", "c", "d"]);
        let leads: Vec<Option<&str>> = stack_leads(&all, &rows)
            .into_iter()
            .map(|l| l.map(title))
            .collect();
        assert_eq!(leads, [Some("b"), None, None, Some("c"), Some("d")]);
    }

    #[test]
    fn stack_rows_nest_stacks_as_a_tree_and_dim_ancestors_that_arent_shown() {
        let mk = |n: u64, base: &str, head: Option<&str>, repo: &str, context: bool| PrSummary {
            number: n,
            base_ref: base.into(),
            head_ref: head.map(Into::into),
            context,
            ..pr(repo, None)
        };
        // Newest first: 4 is on 3, which is on 2 (not mine); 5 is alone; 6/7 form a cycle.
        let all = vec![
            mk(4, "b3", Some("b4"), "o/r", false),
            mk(5, "main", Some("b5"), "o/r", false),
            mk(3, "b2", Some("b3"), "o/r", true),
            mk(9, "b4", None, "o/r", true),
            mk(2, "main", Some("b2"), "o/r", true),
            mk(6, "b7", Some("b6"), "o/r", false),
            mk(7, "b6", Some("b7"), "o/r", false),
            mk(8, "b4", Some("b8"), "o/other", false),
            // A branching stack keeps its full nesting.
            mk(20, "main", Some("c20"), "o/b", false),
            mk(21, "c20", Some("c21"), "o/b", false),
            mk(22, "c20", Some("c22"), "o/b", false),
            mk(23, "c21", Some("c23"), "o/b", false),
        ];
        let rows = stack_rows(&all, |p| !p.context);
        let got: Vec<(u64, Option<usize>, bool)> = rows
            .iter()
            .map(|r| (all[r.pr].number, r.graph.as_ref().map(|g| g.depth), r.dim))
            .collect();
        assert_eq!(
            got,
            [
                (2, Some(0), true),
                (3, Some(1), true),
                (4, Some(2), false),
                (5, None, false),
                (7, Some(0), false),
                (6, Some(1), false),
                (8, None, false),
                (20, Some(0), false),
                (21, Some(1), false),
                (23, Some(2), false),
                (22, Some(1), false),
            ]
        );
        let g = |n: u64| {
            let r = rows.iter().find(|r| all[r.pr].number == n).unwrap();
            r.graph.clone().unwrap()
        };
        let graph = |depth, last, down, through: &[usize]| Graph {
            depth,
            last,
            down,
            through: through.to_vec(),
        };
        assert_eq!(g(2), graph(0, true, true, &[]));
        assert_eq!(g(4), graph(2, true, false, &[]));
        assert_eq!(g(21), graph(1, false, true, &[]));
        // 21 has a sibling below (22), so level 0's line runs past 21's child.
        assert_eq!(g(23), graph(2, true, false, &[0]));
        assert_eq!(g(22), graph(1, true, false, &[]));
        let only5 = stack_rows(&all, |p| p.number == 5);
        assert_eq!(
            only5.iter().map(|r| all[r.pr].number).collect::<Vec<_>>(),
            [5]
        );
    }

    #[test]
    fn rel_time_and_parse_ms() {
        let now = parse_ms("2026-01-10T00:00:00Z").unwrap();
        assert_eq!(now, 1_768_003_200_000);
        let rel = |iso| rel_time(parse_ms(iso).unwrap(), now);
        assert_eq!(rel("2026-01-09T23:59:30Z"), "now");
        assert_eq!(rel("2026-01-09T21:00:00Z"), "3h");
        assert_eq!(rel("2026-01-07T00:00:00Z"), "3d");
        assert_eq!(parse_ms("garbage"), None);
    }

    #[test]
    fn tail_patch_keeps_the_end_of_a_hunk_with_its_line_numbers() {
        let hunk = "@@ -10,4 +10,5 @@ fn main() {\n a\n-b\n+c\n+d\n e";
        assert_eq!(
            tail_patch("x.rs", hunk, 3),
            "--- a/x.rs\n+++ b/x.rs\n@@ -12,1 +11,3 @@ fn main() {\n+c\n+d\n e\n"
        );
        assert_eq!(
            tail_patch("x.rs", hunk, 8),
            "--- a/x.rs\n+++ b/x.rs\n@@ -10,3 +10,4 @@ fn main() {\n a\n-b\n+c\n+d\n e\n"
        );
    }

    #[test]
    fn can_approve_open_not_mine_head_not_approved_by_me_yet() {
        use serde_json::json;
        let pr = |extra: Value| {
            let mut v = json!({"state": "OPEN", "headRefOid": "h2", "author": {"login": "bob"}});
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            v
        };
        let mine = |state: &str, oid: &str| {
            json!({"latestOpinionatedReviews": {"nodes": [
                {"author": {"login": "me"}, "state": state, "commit": {"oid": oid}}
            ]}})
        };
        assert!(can_approve(&pr(json!({})), Some("me")));
        assert!(!can_approve(&pr(json!({})), None));
        assert!(!can_approve(
            &pr(json!({"author": {"login": "me"}})),
            Some("me")
        ));
        assert!(!can_approve(&pr(json!({"state": "MERGED"})), Some("me")));
        assert!(!can_approve(&pr(mine("APPROVED", "h2")), Some("me")));
        // New commits since, or changes requested: approve again.
        assert!(can_approve(&pr(mine("APPROVED", "h1")), Some("me")));
        assert!(can_approve(
            &pr(mine("CHANGES_REQUESTED", "h2")),
            Some("me")
        ));
    }

    #[test]
    fn nits_and_p2_are_not_picked_by_default() {
        let f = |severity: &str| Finding {
            severity: severity.into(),
            ..Default::default()
        };
        assert!(picked_by_default(&f("blocker")));
        assert!(picked_by_default(&f("P1")));
        assert!(!picked_by_default(&f("nit")));
        assert!(!picked_by_default(&f("P2 outdated")));
    }

    #[test]
    fn filter_matches_repo_number_title_and_author() {
        let p = PrSummary {
            number: 12,
            title: "Fix login".into(),
            author: "ana".into(),
            ..pr("acme/web", None)
        };
        assert!(matches_filter(&p, ""));
        assert!(matches_filter(&p, "WEB#12"));
        assert!(matches_filter(&p, "login"));
        assert!(!matches_filter(&p, "bob"));
    }
}
