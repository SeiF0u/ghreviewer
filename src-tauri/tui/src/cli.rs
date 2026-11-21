//! `ghr` subcommands that print and exit instead of opening the terminal UI.

use anyhow::{Context, bail};
use ghreviewer_core::{
    agent::{Run, resume_command},
    config, data_dir,
    db::Db,
    inbox::rel_time,
};
use std::time::{SystemTime, UNIX_EPOCH};

const HELP: &str = "ghr: your GitHub pull requests inbox, in the terminal.

Usage:
  ghr                  open the terminal UI
  ghr config-path      print the path of the config file
  ghr cache-path       print the path of the cache database
  ghr sessions <PR>    list the prompt runs on a PR, with their agent sessions
                       <PR> is owner/repo#123 or a pull request URL
                       --yaml prints them as YAML
  ghr -h, --help       show this help
";

pub fn main(args: &[String]) -> anyhow::Result<()> {
    match args {
        [h] if h == "-h" || h == "--help" || h == "help" => print!("{HELP}"),
        [c] if c == "config-path" => println!("{}", config::path().display()),
        [c] if c == "cache-path" => println!("{}", data_dir()?.join("cache.db").display()),
        [c, pr] if c == "sessions" => sessions(pr, false)?,
        [c, a, b] if c == "sessions" && (a == "--yaml" || b == "--yaml") => {
            sessions(if a == "--yaml" { b } else { a }, true)?
        }
        _ => bail!("unknown command: ghr {}\n\n{HELP}", args.join(" ")),
    }
    Ok(())
}

fn sessions(pr: &str, as_yaml: bool) -> anyhow::Result<()> {
    let (repo, number) = parse_pr(pr).with_context(|| format!("not a PR: {pr}\n\n{HELP}"))?;
    let path = data_dir()?.join("cache.db");
    if !path.exists() {
        bail!("no cache yet at {}", path.display());
    }
    let runs = Db::open(&path)?.pr_runs(&repo, number)?;
    if as_yaml {
        print!("{}", yaml(&runs));
        return Ok(());
    }
    if runs.is_empty() {
        println!("No prompt has run on {repo}#{number}.");
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    for (i, run) in runs.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let status = format!("{:?}", run.status).to_lowercase();
        let harness = format!("{:?}", run.harness).to_lowercase();
        let when = rel_time(run.started_at, now);
        println!("{} · {harness} · {status} · {when} ago", run.prompt);
        let fields = [
            ("session", run.session_id.clone()),
            ("worktree", run.path.clone()),
            ("resume", resume_command(run)),
            ("error", run.error.clone()),
        ];
        for (name, value) in fields {
            if let Some(v) = value {
                println!("  {name:<9}{v}");
            }
        }
    }
    Ok(())
}

/// The runs as a YAML list. Strings are JSON-quoted, which is valid YAML whatever they hold.
fn yaml(runs: &[Run]) -> String {
    if runs.is_empty() {
        return "[]\n".into();
    }
    let q = |s: &str| serde_json::Value::from(s).to_string();
    let opt = |s: Option<&str>| s.map_or("null".into(), q);
    let mut out = String::new();
    for run in runs {
        let fields = [
            ("prompt", q(&run.prompt)),
            ("repo", q(&run.repo)),
            ("number", run.number.to_string()),
            ("harness", q(&format!("{:?}", run.harness).to_lowercase())),
            ("status", q(&format!("{:?}", run.status).to_lowercase())),
            ("started_at", run.started_at.to_string()),
            (
                "finished_at",
                run.finished_at.map_or("null".into(), |t| t.to_string()),
            ),
            ("session", opt(run.session_id.as_deref())),
            ("worktree", opt(run.path.as_deref())),
            ("resume", opt(resume_command(run).as_deref())),
            ("error", opt(run.error.as_deref())),
        ];
        for (i, (k, v)) in fields.iter().enumerate() {
            out += &format!("{} {k}: {v}\n", if i == 0 { "-" } else { " " });
        }
    }
    out
}

/// `owner/repo#123` or `https://github.com/owner/repo/pull/123[/files…]`.
fn parse_pr(s: &str) -> Option<(String, u64)> {
    if let Some((_, path)) = s.split_once("github.com/") {
        let mut parts = path.split(['/', '#', '?']);
        let (owner, name, pull, n) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        return (pull == "pull").then_some((format!("{owner}/{name}"), n.parse().ok()?));
    }
    let (repo, n) = s.split_once('#')?;
    (repo.split('/').count() == 2).then_some((repo.into(), n.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::{Run, parse_pr, yaml};

    #[test]
    fn sessions_as_yaml() {
        assert_eq!(yaml(&[]), "[]\n");
        let run = Run {
            prompt: "review".into(),
            repo: "o/r".into(),
            number: 7,
            started_at: 5,
            error: Some("bad \"x\"\nline: 2".into()),
            ..Default::default()
        };
        let got = yaml(&[run]);
        assert!(got.starts_with("- prompt: \"review\"\n  repo: \"o/r\"\n  number: 7\n"));
        assert!(got.contains("  finished_at: null\n  session: null\n"));
        assert!(got.ends_with("  error: \"bad \\\"x\\\"\\nline: 2\"\n"));
    }

    #[test]
    fn parses_pr_refs() {
        let want = Some(("o/r".to_string(), 12));
        assert_eq!(parse_pr("o/r#12"), want);
        assert_eq!(parse_pr("https://github.com/o/r/pull/12"), want);
        assert_eq!(
            parse_pr("https://github.com/o/r/pull/12/files#diff-1"),
            want
        );
        assert_eq!(parse_pr("o#12"), None);
        assert_eq!(parse_pr("https://github.com/o/r/issues/12"), None);
        assert_eq!(parse_pr("o/r#x"), None);
    }
}
