//! Subcommands and flags. Port of dashy/cli.py. No subcommand opens the desktop window.
//! ponytail: clap derive for the parsing, and the same prompts, lines and exit codes as the Python,
//! since the hooks and the reviewer's `gitdashy api` read them.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use base64::Engine;
use clap::{Parser, Subcommand};
use serde_json::Value;

use crate::config::{self, VERSION};
use crate::types::{Pr, Repository};
use crate::{
    autorev, bind as bind_mod, dbrepo, demo, friction as friction_mod, github, install as install_mod,
    knowledge, memory,
};
use crate::{mirror, review as review_mod, team};

pub const USAGE: &str = include_str!("usage.txt");

pub const NO_TOKEN: &str = "  gitdashy: no GitHub token.

  Set one and run again — a classic token with the `repo` scope, or a fine-grained
  token with read access to the repos you review and write access to pull requests:

      export GH_TOKEN=…          (or $GITHUB_TOKEN)
      https://github.com/settings/tokens

  Put it in your shell rc to keep it. Nothing else is needed: gitdashy talks to the
  GitHub API itself and does not use the gh CLI.

  To look around without one:  gitdashy --demo
";

const COMMANDS: &[&str] = &[
    "sync-memory",
    "remember",
    "install",
    "self-review",
    "setup",
    "init",
    "bind",
    "auto",
    "inline",
    "db",
    "friction",
    "api",
    "drafts",
    "teams",
    "self-check",
];

/// The top-level flags: the dashboard's, plus the ones every command shares.
#[derive(Parser, Debug)]
#[command(name = "gitdashy", disable_help_flag = true, disable_version_flag = true)]
pub struct Cli {
    #[arg(long)]
    pub interval: Option<u64>,
    #[arg(long)]
    pub auto: bool,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub effort: Option<String>,
    #[arg(long)]
    pub depth: Option<String>,
    #[arg(long, value_delimiter = ',')]
    pub voice: Option<Vec<String>>,
    #[arg(long, value_delimiter = ',')]
    pub hunter: Option<Vec<String>>,
    #[arg(long)]
    pub instructions: Option<String>,
    /// Post each finding as a comment on the line it is about, beside the review body.
    #[arg(long)]
    pub inline: bool,
    #[arg(long)]
    pub demo: bool,
    #[arg(long, global = true)]
    pub debug: bool,
    #[arg(long)]
    pub browser: bool,
    #[arg(long)]
    pub no_open: bool,
    #[arg(long)]
    pub port: Option<u16>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(name = "sync-memory")]
    SyncMemory {
        #[arg(long)]
        into: Option<String>,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        no_pull: bool,
        #[arg(long)]
        general: bool,
    },
    Remember {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        general: bool,
        /// keep it in your own drafts even in a team's repo
        #[arg(long)]
        private: bool,
        /// ponytail: the fact is everything that is not a flag or a flag's value.
        #[arg(num_args = 0..)]
        fact: Vec<String>,
    },
    Install {
        #[arg(long)]
        full: bool,
        #[arg(long)]
        corpus: Option<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        no_setup: bool,
        #[arg(long)]
        uninstall: bool,
    },
    #[command(name = "self-review")]
    SelfReview {
        number: Option<u64>,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    Setup,
    Init {
        #[arg(long)]
        into: Option<String>,
        #[arg(long)]
        loader: Option<String>,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        forget: bool,
    },
    Bind {
        repo: Option<String>,
        #[arg(long)]
        team: Option<String>,
        #[arg(long)]
        forget: bool,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long)]
        list: bool,
    },
    /// Which repos auto-review is armed for. No arguments reports.
    Auto {
        repo: Option<String>,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long)]
        off: bool,
        #[arg(long)]
        list: bool,
    },
    /// Which repos get a review's findings as comments on the lines they name. No arguments reports.
    Inline {
        repo: Option<String>,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long)]
        off: bool,
        /// Drop the rule, so the target follows its owner or the --inline switch again.
        #[arg(long)]
        clear: bool,
        #[arg(long)]
        list: bool,
    },
    /// Which repo holds each repo's database. No arguments reports.
    Db {
        target: Option<String>,
        db: Option<String>,
        #[arg(long)]
        off: bool,
        #[arg(long)]
        forget: bool,
    },
    Friction {
        #[arg(long)]
        claude_hook: bool,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long, default_value_t = 0)]
        interrupts: u32,
        #[arg(long, default_value_t = 0)]
        denials: u32,
    },
    Api {
        path: Option<String>,
        #[arg(long)]
        diff: bool,
    },
    Drafts {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        count: bool,
    },
    Teams {
        #[arg(long)]
        new: Option<String>,
        #[arg(long)]
        desc: Option<String>,
        #[arg(long)]
        at: Option<String>,
        #[arg(long)]
        join: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        team: Option<String>,
        #[arg(long)]
        connect: Option<String>,
        #[arg(long)]
        cover: Option<String>,
        #[arg(long)]
        uncover: Option<String>,
        #[arg(long)]
        leave: Option<String>,
        #[arg(long)]
        agents_again: bool,
        #[arg(long)]
        publishing_again: bool,
    },
    #[command(name = "self-check")]
    SelfCheck {
        #[arg(long)]
        model: Option<String>,
    },
}

/// Python's `raise SystemExit("msg")`: the message on stderr, exit 1.
fn fail(msg: impl AsRef<str>) -> i32 {
    eprintln!("{}", msg.as_ref());
    1
}

/// A prompt on stdout, the trimmed answer from stdin; None on EOF or a closed stdin.
fn ask_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

fn nonempty(s: Option<String>) -> String {
    s.unwrap_or_default()
}

/// `~/x` as Python's expanduser wrote it: a quoted "~/x" would otherwise mirror into a dir named ~.
fn expanduser(p: &str) -> PathBuf {
    if p == "~" {
        config::home()
    } else if let Some(rest) = p.strip_prefix("~/") {
        config::home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// Python's repr of a str: single quotes, for the messages the tests match on.
fn pyrepr(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn pyrepr_list(v: &[String]) -> String {
    format!("[{}]", v.iter().map(|s| pyrepr(s)).collect::<Vec<_>>().join(", "))
}

/// Python's json.dumps escapes everything past ASCII as \uXXXX; models read this output, so it stays.
fn ascii_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else {
            let mut buf = [0u16; 2];
            for unit in ch.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

/// Python's json.dumps(d, indent=1).
fn json_indent1(v: &Value) -> String {
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    serde::Serialize::serialize(v, &mut ser).ok();
    ascii_json(&String::from_utf8_lossy(&buf))
}

/// Python's json.dumps(s) for one string.
fn json_str(s: &str) -> String {
    ascii_json(&serde_json::to_string(s).unwrap_or_default())
}

/// The origin of the directory we are standing in, or "".
fn here() -> String {
    team::origin_slug(Path::new("."))
}

fn sync_memory(into: Option<String>, repo: Option<String>, no_pull: bool, general: bool) -> i32 {
    let into = nonempty(into);
    if into.is_empty() {
        return fail("gitdashy: sync-memory needs --into PATH");
    }
    team::activate(); // ponytail: names the team and points LOG at its checkout; memory.sources() needs it
    let repo = repo.filter(|r| !r.is_empty()).unwrap_or_else(here);
    println!("{}", mirror::sync(&expanduser(&into), &repo, !no_pull, general));
    0
}

/// Where the shipped corpus lives. ponytail: the binary embeds it (install::CORPUS); this path names
/// the checkout's copy for the installer that wants a directory, as Python's HERE/corpus did.
fn corpus_dir() -> PathBuf {
    // ponytail: CARGO_MANIFEST_DIR is the path on the machine that BUILT this binary, so on every
    // installed copy it names a directory that is not there. An empty path is Src::Embedded, which is
    // what a shipped binary must use; the checkout's copy still wins when you are standing in it.
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus");
    if dir.is_dir() {
        dir
    } else {
        PathBuf::new()
    }
}

/// Wire this machine, after saying what that means and being told to go ahead.
fn install(full: bool, url: Option<String>, dry: bool, yes: bool, no_setup: bool, uninstall: bool) -> i32 {
    let corpus = corpus_dir();
    let url = nonempty(url);
    if uninstall {
        let lines = if full {
            install_mod::full_remove(dry)
        } else {
            install_mod::remove(dry)
        };
        println!("{}", lines.join("\n"));
        return 0;
    }
    let explain = if full {
        install_mod::full_explain(&corpus, &url)
    } else {
        install_mod::explain()
    };
    println!("{}", explain.join("\n"));
    if dry {
        println!();
        let lines = if full {
            install_mod::full_apply(&corpus, &url, true)
        } else {
            install_mod::apply(true)
        };
        println!("{}", lines.join("\n"));
        println!("\n--dry-run, so nothing was changed. Without it you are asked first.");
        return 0;
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            // ponytail: never write global config from a script that cannot be asked
            return fail("\ngitdashy: not a terminal — pass --yes if you meant to install unattended");
        }
        match ask_line("\nGo ahead? [y/N] ") {
            Some(a) if matches!(a.to_lowercase().as_str(), "y" | "yes") => {}
            Some(_) => {
                println!("nothing changed");
                return 0;
            }
            None => {
                println!("\nnothing changed");
                return 0;
            }
        }
    }
    println!();
    let out = if full {
        install_mod::full_apply(&corpus, &url, false)
    } else {
        install_mod::apply(false)
    };
    println!("{}", out.join("\n"));
    // ponytail: --full ONLY. Plain install puts no corpus on the machine, so there is no USER.md to
    // fill in, and its whole promise is that it stays out of the way: no corpus, no hooks, no
    // settings.json, and nothing to answer. The project brief still matters at that tier and setup
    // writes it, but offering a two-part flow whose first half SKIPs is worse than saying nothing.
    if full && !out.iter().any(|l| l.starts_with("FAIL")) {
        offer_setup(yes, no_setup);
    }
    0
}

/// After a full install, offer the questions rather than only naming the file to hand-edit.
///
/// ponytail: the installer used to say "fill it in, it is the highest-value file here" and never
/// mention `gitdashy setup`. The guided path existed and was unreachable from the one moment you are
/// deciding how to fill it.
pub fn offer_setup(yes: bool, no_setup: bool) {
    // ponytail: --yes too. This command already tells you "pass --yes if you meant to install unattended"
    // when stdin is not a tty, so --yes means "do not ask me" for BOTH gates or the promise is false. A
    // bootstrap script run from an interactive shell inherits that tty, so isatty alone does not cover it.
    if no_setup || yes || !std::io::stdin().is_terminal() {
        return;
    }
    if install_mod::setup_done(None, false) {
        return; // ponytail: nothing to offer once USER.md is written; the brief is not asked here
    }
    let later = "`gitdashy setup` whenever you want to — nothing else is waiting on it.";
    match ask_line("\nSay who you are now? [Y/n] ") {
        Some(a) if matches!(a.to_lowercase().as_str(), "" | "y" | "yes") => {}
        Some(_) => {
            println!("{later}");
            return;
        }
        None => {
            println!("\n{later}");
            return;
        }
    }
    println!();
    // ponytail: declining halfway through the briefs is a decline, not a failure. The install has
    // COMPLETED and printed its report; a wrapper checking $? must not read it as a failed one.
    let _ = setup(false);
}

/// Pre-review one of your own PRs. Posts nothing; writes a file and prints where it is.
fn self_review(number: Option<u64>, repo: Option<String>, model: Option<String>) -> i32 {
    let repo = repo.filter(|r| !r.is_empty()).unwrap_or_else(here);
    let (Some(n), false) = (number, repo.is_empty()) else {
        return fail(
            "gitdashy: self-review needs a PR number, and --repo owner/name unless this directory has a github origin",
        );
    };
    config::load();
    team::activate();
    let pr = Pr {
        number: n,
        url: format!("https://github.com/{repo}/pull/{n}"),
        repository: Repository {
            name_with_owner: repo.clone(),
            name: repo.split('/').nth(1).unwrap_or("").into(),
        },
        ..Default::default()
    };
    println!("pre-reviewing {repo}#{n} — nothing will be posted…");
    let model = model.unwrap_or_else(|| config::get().model);
    match review_mod::self_review(&pr, &model) {
        Ok((status, dest)) if !dest.as_os_str().is_empty() => {
            println!("{status}\n{}", dest.display());
            0
        }
        Ok((status, _)) => {
            println!("{status}");
            1
        }
        Err(e) => {
            println!("{e}");
            1
        }
    }
}

/// Ask for the two things a corpus cannot work out on its own: who you are, and what this is for.
///
/// ponytail: `project` false is the install-time path: who you are is machine-level, what the work is
/// for is not. See install.setup.
fn setup(project: bool) -> i32 {
    println!(
        "{}Blank keeps what is already there — the prompt shows you what. Edit the files later; nothing here is final.\n",
        if project { "Two short briefs. " } else { "One short brief. " }
    );
    // PORT-NOTE: Python's `ask` raised SystemExit on EOF, which unwound install.setup at once. The Rust
    // install::setup takes `None` as "skip this one", so an EOF here skips every remaining question and
    // the "nothing written" line is printed once the walk returns.
    let mut eof = false;
    let mut ask = |question: &str, current: &str| -> Option<String> {
        if eof {
            return None;
        }
        let now = current.lines().next().unwrap_or("");
        let now: String = now.chars().take(60).collect();
        let prompt = format!(
            "  {question}{}\n  > ",
            if now.is_empty() {
                String::new()
            } else {
                format!("\n  [now: {now}]")
            }
        );
        let got = ask_line(&prompt);
        eof = got.is_none();
        got
    };
    team::activate();
    let lines = install_mod::setup(&mut ask, None, project);
    if eof {
        println!("\nnothing written");
        return 1;
    }
    println!("\n{}", lines.join("\n"));
    0
}

/// Wire one repo so a session there reads its review memory.
fn init(into: Option<String>, loader: Option<String>, repo: Option<String>, forget: bool) -> i32 {
    let (into, loader) = (nonempty(into), nonempty(loader));
    if !into.is_empty() && forget {
        // ponytail: the registry grows on its own, so it needs a way out
        let was = install_mod::unregister(Path::new(&into));
        println!(
            "gitdashy: {} {into}",
            if was {
                "no longer refreshing"
            } else {
                "was not refreshing"
            }
        );
        return 0;
    }
    if into.is_empty() || loader.is_empty() {
        return fail(
            "gitdashy: init needs --into DIR (where the mirror goes) and --loader FILE (the instruction file that should import it)",
        );
    }
    team::activate();
    let repo = repo.filter(|r| !r.is_empty()).unwrap_or_else(here);
    if repo.is_empty() {
        return fail("gitdashy: no git origin here — pass --repo owner/name");
    }
    // ponytail: a REFUSAL exits non-zero. wire_repo reports in prose, so `init` printed "refused: git
    // would commit …" and exited 0; the session hook reads that status to decide whether to start its
    // background sync, and took the refusal for a success.
    let lines = install_mod::wire_repo(Path::new(&into), Path::new(&loader), &repo);
    println!("{}", lines.join("\n"));
    if lines.iter().any(|l| l.contains("refused")) {
        return 1;
    }
    0
}

/// File a fact a coding session learned, into the same drafts a review writes to.
fn remember(repo: Option<String>, general: bool, private: bool, fact: Vec<String>) -> i32 {
    let fact = fact.join(" ").trim().to_string();
    if fact.is_empty() {
        return fail("gitdashy: remember needs a fact to remember");
    }
    team::activate(); // so memory.sources() sees the team as a second source
    let named = repo.filter(|r| !r.is_empty());
    if !general && named.is_none() && here().is_empty() {
        return fail("gitdashy: no git origin here — pass --repo owner/name, or --general");
    }
    // ponytail: --general threw away the repo you are standing in, which is the only thing that says
    // WHICH PROJECT a general fact is about. With two teams joined it then had no destination at all:
    // neither poolable nor shareable, with nothing on screen saying why. The context is kept now; a
    // general fact means "true across this project", and the project is that repo's team.
    let about = if general {
        named.clone().unwrap_or_else(here)
    } else {
        String::new()
    };
    let repo = memory::general_scope(general, private, named.unwrap_or_else(here), &about);
    let where_ = if repo.is_empty() {
        "general".to_string()
    } else {
        repo.clone()
    };
    if memory::already_known(&repo, &fact) {
        println!("gitdashy: {where_} already knows that");
        return 0;
    }
    // ponytail: in a team's repo a session drafts into the team's pool by default, like a review, so a
    // teammate's review can match it. --private keeps a thought yours, wherever you are standing.
    let (promoted, pooled) = if private {
        (memory::append_private(&repo, &fact, "session"), false)
    } else {
        memory::append_routed(&repo, &fact, &about, "session")
    };
    team::push_dir(
        &config::get().memory_dir,
        &format!("memory: remembered for {where_}"),
        "mine",
    );
    team::push(&format!("memory: evidence for {where_}")); // ponytail: a promotion writes the pool, which lives over there
    if let Some(first) = promoted.first() {
        // ponytail: the counter counts observations; it does not know which surface each came from
        let whose = if pooled {
            "the team knows it now"
        } else {
            "confirmed"
        };
        println!("gitdashy: {where_} — {whose}, by a second independent observation: {first}");
        return 0;
    }
    let whose = if pooled { " in the team's drafts" } else { "" };
    println!("gitdashy: {where_} — drafted{whose}; one more independent observation confirms it");
    0
}

/// The team a command acts on: --team as typed, folded to its key, or the one joined team.
///
/// ponytail: refused when it is not a team this machine has joined. `--team NeoMedSys_team` was accepted
/// verbatim, reported success and resolved to nothing: a typo bound an org to a team that did not
/// exist, and the only sign was the pane saying "not in team" on every row of it.
/// ponytail: ONE resolver for every verb that takes --team. There were two, and the second skipped the
/// membership check, so `teams --team nope --cover acme` appended {"owner": "acme", "team": "nope"} to
/// the bindings store and only then failed with "not in nope". A write before a check is worse than
/// the original because it leaves the store dirty.
/// ponytail: none joined and several joined are different problems, so they get different sentences.
fn team_of(typed: Option<&str>) -> Result<String, String> {
    if let Some(typed) = typed.filter(|t| !t.is_empty()) {
        let key = team::key_of(typed);
        if team::dir_of(&key).is_none() {
            let joined = team::joined().join(", ");
            return Err(format!(
                "gitdashy: not in team {} — joined: {}",
                pyrepr(typed),
                if joined.is_empty() { "none".into() } else { joined }
            ));
        }
        return Ok(key);
    }
    let joined = team::joined();
    if joined.len() == 1 {
        return Ok(joined[0].clone());
    }
    Err(format!(
        "gitdashy: {}",
        if joined.is_empty() {
            "not in a team — join one with T in the dashboard, or pass --team SLUG".to_string()
        } else {
            format!("say which team: --team {}", joined.join(" | --team "))
        }
    ))
}

/// "reviews of it read: <whose>" and whether there is anything to read.
fn reads(repo: &str, label: &str) -> String {
    let (text, whose) = memory::brief(Some(repo), None);
    format!(
        "  reviews of {label} read: {whose}{}",
        if text.is_empty() { " (nothing to read)" } else { "" }
    )
}

/// Which repos get inline review comments. Same shape as `auto`: a repo, an owner, or a report.
///
/// ponytail: `--clear` is a third state, not the absence of `--off`. A rule has to be removable or an
/// owner rule, once set, could never be taken off a repo under it — the same reason `Post` has CLEAR.
fn inline_cmd(positional: Option<String>, owner: Option<String>, off: bool, clear: bool, list: bool) -> i32 {
    let show = || {
        let rules = autorev::inlines();
        let listed = rules.listed();
        if listed.is_empty() {
            println!(
                "gitdashy: no repo has a rule of its own; every one follows --inline, which is {}",
                if config::get().inline { "on" } else { "off" }
            );
            return;
        }
        println!(
            "gitdashy: every repo not listed follows --inline, which is {}",
            if config::get().inline { "on" } else { "off" }
        );
        for (t, v) in listed {
            println!(
                "  {t:<36}  →  findings {} on the lines they name",
                if v.on() { "are posted" } else { "are not posted" }
            );
        }
    };
    // ponytail: before every write, the same trap auto_cmd records: --list is a question, and gating
    // it behind the target turned `inline --owner acme --list` into a write that answered nothing.
    if list {
        show();
        return 0;
    }
    if off && clear {
        return fail("gitdashy: --off sets a rule, --clear removes one; not both");
    }
    let rule = if clear {
        None
    } else if off {
        Some(autorev::Inline::Off)
    } else {
        Some(autorev::Inline::On)
    };
    let positional = positional.filter(|p| !p.is_empty());
    let owner = owner.filter(|o| !o.is_empty());
    // ponytail: a positional we cannot read is a TYPO, not an absence — as in auto_cmd.
    let named = match &positional {
        Some(p) if p.contains('/') => p.clone(),
        Some(typo) => {
            return fail(format!(
                "gitdashy: {} is not owner/name — inline takes a full slug, or --owner OWNER",
                pyrepr(typo)
            ))
        }
        None => String::new(),
    };
    if !named.is_empty() && owner.is_some() {
        return fail("gitdashy: name a repo or an owner, not both");
    }
    if let Some(owner) = owner {
        let err = autorev::set_inline_owner(&owner, rule);
        if !err.is_empty() {
            return fail(format!("gitdashy: {err}"));
        }
        show();
        return 0;
    }
    if named.is_empty() {
        // ponytail: a flag with nothing to apply it to is a question asked wrong, not a report.
        if off || clear {
            return fail("gitdashy: that needs a repo or --owner OWNER");
        }
        show();
        return 0;
    }
    let err = autorev::set_inline(&named, rule);
    if !err.is_empty() {
        return fail(format!("gitdashy: {err}"));
    }
    show();
    0
}

/// Which repos auto-review is armed for.
///
/// ponytail: a bare `gitdashy auto` REPORTS, the same rule `bind` has. Naming no repo and asking for
/// no change is a question, and answering it by arming whatever directory you are standing in is a
/// write nobody asked for.
fn auto_cmd(positional: Option<String>, owner: Option<String>, off: bool, list: bool) -> i32 {
    let on = !off;
    let show = || {
        for line in autorev::report(&autorev::scope()) {
            println!("{line}");
        }
        // ponytail: reported, never set here. Where a review posts is a GUI decision — the operator
        // asked for it there — but a command that reports the scope and stays silent about what
        // happens to the verdict is telling half the story.
        let p = autorev::posting();
        for (ran, label) in [
            (autorev::Ran::Manual, "you press r"),
            (autorev::Ran::Auto, "auto runs it"),
        ] {
            for (t, v) in p.rules(ran).listed() {
                if v == autorev::Post::Hold {
                    println!("  {t:<36}  →  waits for a key when {label}");
                }
            }
        }
    };
    // ponytail: BEFORE every write. --list is a read-only question, and bind() learned this one
    // command over: gating it behind a check on the thing you were asking about turned
    // `bind <typo> --list` into an exit instead of an answer. Here it was worse — `auto --owner acme
    // --list` armed the org and returned before reading the store.
    if list {
        show();
        return 0;
    }
    let positional = positional.filter(|p| !p.is_empty());
    let owner = owner.filter(|o| !o.is_empty());
    // ponytail: a positional we cannot read is a TYPO, not an absence — the same trap bind() names.
    // Checked BEFORE --owner, or `auto not-a-slug --owner acme` armed the org and swallowed the typo.
    let named = match &positional {
        Some(p) if p.contains('/') => p.clone(),
        Some(typo) => {
            return fail(format!(
                "gitdashy: {} is not owner/name — auto takes a full slug, or --owner OWNER",
                pyrepr(typo)
            ))
        }
        None => String::new(),
    };
    if !named.is_empty() && owner.is_some() {
        return fail("gitdashy: name a repo or an owner, not both");
    }
    if let Some(owner) = owner {
        let err = autorev::set_owner(&owner, on);
        if !err.is_empty() {
            return fail(format!("gitdashy: {err}"));
        }
        let o = bind_mod::owner_key(&owner);
        // what the store now SAYS, not what was written: a lone --off leaves auto covering everything
        println!(
            "gitdashy: {o}/* {}  (a repo of its own still overrides it)",
            if autorev::scope().armed_owner(&o) {
                "is auto-reviewed"
            } else {
                "is not auto-reviewed"
            }
        );
        show();
        return 0;
    }
    if named.is_empty() {
        // ponytail: --off with nothing to turn off silently reported, so a mistyped command looked
        // like it had worked. A flag that changes nothing is a question that was asked wrong.
        if off {
            return fail("gitdashy: --off needs a repo or --owner OWNER");
        }
        show();
        return 0;
    }
    let err = autorev::set(&named, on);
    if !err.is_empty() {
        return fail(format!("gitdashy: {err}"));
    }
    // the folded key, not what was typed, and the state, not the write
    let k = bind_mod::key(&named);
    println!(
        "gitdashy: {k} {}",
        if autorev::scope().armed(&k) {
            "is auto-reviewed"
        } else {
            "is not auto-reviewed"
        }
    );
    show();
    0
}

/// Point a repo, or every repo under an owner, at the repo its database is defined in.
/// `--off` sets "none", which beats an owner rule; `--forget` takes the rule away, so the owner's applies again.
fn db_cmd(target: Option<String>, db: Option<String>, off: bool, forget: bool) -> i32 {
    let (target, db) = (nonempty(target), nonempty(db));
    // exactly one of a DB repo, --off and --forget, and only with a target
    let asks = [!db.is_empty(), off, forget].iter().filter(|b| **b).count();
    if target.is_empty() != (asks == 0) || asks > 1 {
        return fail("gitdashy: db TARGET DB_REPO, db TARGET --off, or db TARGET --forget");
    }
    if !target.is_empty() {
        let err = if forget {
            dbrepo::clear(&target)
        } else {
            dbrepo::set(&target, &db)
        };
        if !err.is_empty() {
            return fail(format!("gitdashy: {err}"));
        }
    }
    let listed = dbrepo::rules().listed();
    if listed.is_empty() {
        println!("  no DB repos: reviews read no database schema");
    }
    for (t, d) in listed {
        println!("  {t:<36}  →  {}", if d.is_empty() { "none" } else { &d });
    }
    0
}

/// Bind a repo to a team, so reviews of it are told that team's brief and no other.
fn bind(
    positional: Option<String>,
    team_flag: Option<String>,
    forget: bool,
    owner: Option<String>,
    list: bool,
) -> i32 {
    team::activate(); // ponytail: names the team, and seeds bindings from the shared log the first time
                      // ponytail: BEFORE the positional guard. --list is a read-only question, and gating it behind a check
                      // on the thing you were asking about turned `bind <typo> --list` into an exit instead of an answer.
    if list {
        let mut owners: Vec<(String, String)> = bind_mod::owners()
            .into_iter()
            .map(|(o, t)| (o + "/*", t))
            .collect();
        owners.sort();
        let mut bound: Vec<(String, String)> = bind_mod::bindings().into_iter().collect();
        bound.sort();
        let rows: Vec<(String, String)> = owners
            .into_iter()
            .chain(bound)
            .chain(
                bind_mod::excluded()
                    .into_iter()
                    .map(|r| (r, "excluded — kept out of the rule above".to_string())),
            )
            .collect();
        if rows.is_empty() {
            println!("  no repo is bound to a team");
        } else {
            println!(
                "{}",
                rows.iter()
                    .map(|(r, t)| format!("  {r:<36}  →  {t}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
        return 0;
    }
    let positional = positional.filter(|p| !p.is_empty());
    let named = positional.clone().filter(|p| p.contains('/')).unwrap_or_default();
    // ponytail: a positional we cannot read is a TYPO, not an absence. `bind neo-api --team org/mem`
    // used to fall through to this directory's origin and bind whatever repo you were standing in,
    // reporting success with the wrong name.
    if named.is_empty() {
        if let Some(typo) = positional {
            return fail(format!(
                "gitdashy: {} is not owner/name — bind takes a full slug, or --owner OWNER",
                pyrepr(&typo)
            ));
        }
    }
    let repo = if named.is_empty() { here() } else { named.clone() };
    // ponytail: an owner rule is one line for a whole org, and a repo binding still overrides it, so the
    // one repo under that owner which is NOT the project can be excluded with `--forget`, which a pattern
    // on its own cannot express. Handled before the repo path, since --owner names no repo.
    if let Some(owner) = owner.filter(|o| !o.is_empty()) {
        if forget {
            let err = bind_mod::forget_owner(&owner);
            if !err.is_empty() {
                return fail(format!("gitdashy: {err}"));
            }
            println!("gitdashy: {}/* is no longer bound", bind_mod::owner_key(&owner));
            return 0;
        }
        let to = match team_of(team_flag.as_deref()) {
            Ok(t) => t,
            Err(e) => return fail(e),
        };
        let err = bind_mod::bind_owner(&owner, &to);
        if !err.is_empty() {
            return fail(format!("gitdashy: {err}"));
        }
        println!(
            "gitdashy: {}/* → {to}  (a repo binding still overrides it)",
            bind_mod::owner_key(&owner)
        );
        return 0;
    }
    if repo.is_empty() {
        return fail("gitdashy: no git origin here — pass owner/name, or --list");
    }
    // ponytail: a bare `gitdashy bind` REPORTS: it changes no binding of its own. Naming no repo and
    // asking for no change is a question, and answering it by binding this directory to whatever team
    // you are in is a write nobody asked for. team.activate() above still seeds bindings from the shared
    // log, on this and every other command: that is the bootstrap, and a bootstrap only some entry
    // points perform is the one missing on the path nobody tested.
    let team_typed = team_flag.as_deref().is_some_and(|t| !t.is_empty());
    if named.is_empty() && !team_typed && !forget {
        let of = bind_mod::of(&repo);
        println!(
            "gitdashy: {} → {}",
            bind_mod::key(&repo),
            if of.is_empty() { "no team".to_string() } else { of }
        );
        println!("{}", reads(&repo, "it"));
        return 0;
    }
    if forget {
        let was = bind_mod::of(&repo);
        let err = bind_mod::forget(&repo);
        if !err.is_empty() {
            return fail(format!("gitdashy: {err}"));
        }
        println!(
            "gitdashy: {} {}",
            bind_mod::key(&repo),
            if was.is_empty() {
                "was not bound to anything".to_string()
            } else {
                format!("unbound from {was}")
            }
        );
    } else {
        let to = match team_of(team_flag.as_deref()) {
            Ok(t) => t,
            Err(e) => return fail(e),
        };
        let err = bind_mod::bind(&repo, &to);
        if !err.is_empty() {
            return fail(format!("gitdashy: {err}"));
        }
        println!("gitdashy: {} → {to}", bind_mod::key(&repo));
    }
    // ponytail: says what the repo GETS, not that a row was written. A binding is only ever a means to
    // selecting a brief, and the one thing worth confirming is which brief a review will now be given.
    let key = bind_mod::key(&repo);
    println!("{}", reads(&repo, if key.is_empty() { &repo } else { &key }));
    0
}

/// GET one GitHub API path and print it, `--diff` for a unified diff. This is how a review reads the
/// repo now that gh is gone.
///
/// ponytail: GET only, github only, and a file arrives decoded rather than as base64 in an envelope.
/// It is the one command a review is allowed to run, so what it can do is what a reviewer may do: read.
/// ponytail: a PATH, never a URL. The caller is a model that has just read an untrusted diff, and a diff
/// that talks it into `gitdashy api https://elsewhere/…` must not be able to send anything anywhere.
/// github.call() withholds the token off-host as well: two locks, because this one is worth two.
fn api(path: Option<String>, diff: bool) -> i32 {
    let path = nonempty(path);
    if path.is_empty() {
        return fail("gitdashy: api needs a path, e.g. /repos/owner/name/contents/src/app.py");
    }
    if path.starts_with("http://") || path.starts_with("https://") || path.starts_with("//") {
        return fail("gitdashy: api takes an API path, not a URL");
    }
    // ponytail: and the repo, when a review set one. GET-only and host-pinned kept the token in and the
    // writes out; nothing kept the READS to the PR being reviewed, and a review body is posted publicly.
    let scope = std::env::var(github::SCOPE).unwrap_or_default();
    let scope_team = std::env::var(github::SCOPE_TEAM).unwrap_or_default();
    let scope_db = std::env::var(github::SCOPE_DB).unwrap_or_default();
    let path = match github::scoped(&path, &scope, &scope_team, &scope_db) {
        Ok(p) => p,
        Err(e) => return fail(format!("gitdashy: {e}")),
    };
    let accept = if diff {
        "application/vnd.github.v3.diff"
    } else {
        "application/vnd.github+json"
    };
    let full = if path.starts_with('/') {
        path
    } else {
        format!("/{path}")
    };
    let raw = match github::call(&full, "GET", None, accept, 60) {
        Ok(r) => r,
        Err(e) => return fail(format!("gitdashy: {e}")),
    };
    // ponytail: `| head` is a closed pipe, not an error to print: write errors are dropped on purpose.
    let mut out = std::io::stdout().lock();
    let d: Value = match serde_json::from_str(&raw) {
        Ok(d) => d,
        Err(_) => {
            let _ = writeln!(out, "{raw}"); // a diff, a raw file: already text
            return 0;
        }
    };
    if d.get("encoding").and_then(Value::as_str) == Some("base64") {
        let content: String = d
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        return match base64::engine::general_purpose::STANDARD.decode(content) {
            Ok(bytes) => {
                let _ = writeln!(out, "{}", String::from_utf8_lossy(&bytes));
                0
            }
            Err(e) => fail(format!("gitdashy: {e}")),
        };
    }
    let _ = writeln!(out, "{}", json_indent1(&d));
    0
}

/// Show what gitdashy has heard once and not confirmed. Read-only; W in the dashboard acts on it.
fn drafts(repo: Option<String>, count: bool) -> i32 {
    team::activate();
    // ponytail: --count is for a session hook, so it defaults to the repo you are standing in and reads
    // only the local store: no network, and nothing printed when there is nothing to say.
    let only = repo
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| if count { here() } else { String::new() });
    if count && only.is_empty() {
        // ponytail: a repo we cannot name has nothing waiting FOR IT. Without this a local-only repo,
        // a git repo, which is all the hook requires, was told every draft on the machine was its own.
        return 0;
    }
    let mut rows = memory::waiting();
    if !only.is_empty() {
        rows.retain(|r| r.0.as_deref().unwrap_or("general") == only);
    }
    if count {
        if !rows.is_empty() {
            println!(
                "gitdashy: {} draft{} waiting for {only} — `gitdashy drafts` lists them, W in the dashboard promotes or drops them",
                rows.len(),
                if rows.len() != 1 { "s" } else { "" }
            );
        }
        return 0;
    }
    if rows.is_empty() {
        println!("  nothing waiting — every observation so far is either a fact or gone");
        return 0;
    }
    rows.sort_by(|a, b| {
        (
            a.0.clone().unwrap_or_default(),
            a.3 == "self",
            std::cmp::Reverse(a.1),
        )
            .cmp(&(
                b.0.clone().unwrap_or_default(),
                b.3 == "self",
                std::cmp::Reverse(b.1),
            ))
    });
    // ponytail: grouped by repo with a heading per group, general included. A `where = None` sentinel
    // collided with the repo of the GENERAL file, which is also None, so its rows printed under no
    // heading at all. A sentinel that can equal a real value is not a sentinel.
    let mut current: Option<Option<String>> = None;
    for (repo, n, fact, kind) in &rows {
        if current.as_ref() != Some(repo) {
            current = Some(repo.clone());
            let team_of = repo.as_deref().map(bind_mod::of).unwrap_or_default();
            println!(
                "\n  {}{}",
                repo.as_deref().unwrap_or("general"),
                if team_of.is_empty() {
                    String::new()
                } else {
                    format!("  ({team_of})")
                }
            );
        }
        // ponytail: the count is the whole point of the line: it says how close this is to being a
        // fact, and a pre-review finding has no count because one opinion twice is still one opinion.
        let tag = if kind == "self" {
            "pre-review".to_string()
        } else {
            format!("seen {n}×")
        };
        println!("    [{tag:>10}]  {fact}");
    }
    println!(
        "\n  {} waiting · {} independent observations make a fact · W in the dashboard promotes or drops one",
        rows.len(),
        memory::PROMOTE_AT
    );
    0
}

fn team_error_suffix() -> String {
    let e = team::ERROR.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if e.is_empty() {
        String::new()
    } else {
        format!("  ({e})")
    }
}

/// List the teams this machine has joined, or join/leave one.
///
/// ponytail: a CLI as well as `T`, for the same reason `bind` has one: it is scriptable, it is
/// testable without a window, and the join path is the one that clones a repo, which is worth being
/// able to run somewhere errors are visible rather than on a footer.
#[allow(clippy::too_many_arguments)]
fn teams(
    new: Option<String>,
    desc: Option<String>,
    at: Option<String>,
    join: Option<String>,
    name: Option<String>,
    team_flag: Option<String>,
    connect: Option<String>,
    cover: Option<String>,
    uncover: Option<String>,
    leave: Option<String>,
    agents_again: bool,
    publishing_again: bool,
) -> i32 {
    team::activate();
    let some = |s: Option<String>| s.filter(|v| !v.is_empty());
    let key_or_fail = || team_of(team_flag.as_deref());
    if let Some(new) = some(new) {
        // ponytail: START one, with nothing hosted anywhere. Every other path clones a repo that already
        // exists, so the first person on a team was stuck waiting for somebody to make one.
        let err = team::start(&new, &nonempty(desc), &nonempty(at));
        if !err.is_empty() {
            return fail(format!("gitdashy: {err}"));
        }
        let key = team::key_of(&new);
        println!(
            "gitdashy: started {new} ({key}) at {}",
            team::dir_of(&key).unwrap_or_default().display()
        );
        println!("  bind repos to it: gitdashy bind --owner OWNER --team {key}");
        println!("  give it a remote when you have one: gitdashy teams --team {key} --connect URL");
    } else if let Some(url) = some(connect) {
        let key = match key_or_fail() {
            Ok(k) => k,
            Err(e) => return fail(e),
        };
        let err = team::connect(&key, &url);
        if !err.is_empty() {
            return fail(format!("gitdashy: {err}"));
        }
        println!("gitdashy: {key} now pushes to {url}");
    } else if let Some(target) = some(cover) {
        let key = match key_or_fail() {
            Ok(k) => k,
            Err(e) => return fail(e),
        };
        // ponytail: declared in the team AND bound here. The local rule is the cheap, reversible half and
        // goes first: a declaration that published while the binding failed is a rule that works only on
        // other people's machines, which is the one outcome this pair must not produce.
        let (kind, t) = bind_mod::target(&target);
        let mut err = match kind.as_str() {
            "owner" => bind_mod::bind_owner(&t, &key),
            "" => format!("{} is not an owner or an owner/name", pyrepr(&target)),
            _ => bind_mod::bind(&t, &key),
        };
        if err.is_empty() {
            err = team::cover(&key, &target);
        }
        if !err.is_empty() {
            return fail(format!("gitdashy: {err}"));
        }
        println!(
            "gitdashy: {key} now covers {}  (bound here, and everyone who joins gets it once){}",
            bind_mod::cover_key(&target),
            team_error_suffix()
        );
    } else if let Some(target) = some(uncover) {
        let key = match key_or_fail() {
            Ok(k) => k,
            Err(e) => return fail(e),
        };
        let err = team::uncover(&key, &target);
        if !err.is_empty() {
            return fail(format!("gitdashy: {err}"));
        }
        println!(
            "gitdashy: {key} no longer covers {}  (rows it seeded stay until `gitdashy bind ... --forget`){}",
            bind_mod::cover_key(&target),
            team_error_suffix()
        );
    } else if let Some(join) = some(join) {
        // ponytail: what CHANGED, not joined()[-1]: that is the last alphabetically, so already being
        // in "zulu" and joining "acme" printed "joined zulu".
        let before = team::joined();
        let err = team::setup(&join, &nonempty(name));
        if !err.is_empty() {
            return fail(format!("gitdashy: {err}"));
        }
        let mut fresh: Vec<String> = team::joined()
            .into_iter()
            .filter(|k| !before.contains(k))
            .collect();
        fresh.sort();
        println!(
            "gitdashy: joined {}{}",
            fresh.first().cloned().unwrap_or(join),
            team_error_suffix()
        );
    } else if agents_again || publishing_again {
        // ponytail: the same undo for both gates — see memory::ask_publishing_again for why.
        let key = match key_or_fail() {
            Ok(k) => k,
            Err(e) => return fail(e),
        };
        let forgot = if agents_again {
            memory::ask_agents_again(&key)
        } else {
            memory::ask_publishing_again(&key)
        };
        if forgot {
            println!("gitdashy: {key} will be asked about again at the next launch");
        } else {
            println!("gitdashy: nothing recorded for {key} — it is already asked about at launch");
        }
    } else if let Some(leave) = some(leave) {
        let err = knowledge::leave(&leave);
        if !err.is_empty() {
            return fail(format!("gitdashy: {err}"));
        }
        println!("gitdashy: left {leave}");
    }
    let got = team::joined();
    if got.is_empty() {
        println!("  no teams joined — `gitdashy teams --join owner/name` or T in the dashboard");
        return 0;
    }
    let bindings = bind_mod::bindings();
    let owners = bind_mod::owners();
    for key in got {
        let it = team::info(&key);
        let mut bound: Vec<String> = bindings
            .iter()
            .filter(|(_, t)| **t == key)
            .map(|(r, _)| r.clone())
            .collect();
        bound.sort();
        let mut own: Vec<String> = owners
            .iter()
            .filter(|(_, t)| **t == key)
            .map(|(o, _)| o.clone() + "/*")
            .collect();
        own.sort();
        let d = team::dir_of(&key).unwrap_or_default();
        println!("  {}  ({key})", it.name);
        if !it.description.is_empty() {
            println!("      {}", it.description);
        }
        println!(
            "      {}{}",
            d.display(),
            if team::has_remote(&d) {
                ""
            } else {
                "   · no remote yet"
            }
        );
        let declared = team::covers(&key);
        if !declared.is_empty() {
            println!("      declares: {}", declared.join(", "));
        }
        own.extend(bound);
        println!(
            "      {}",
            if own.is_empty() {
                "no repos bound to it yet".to_string()
            } else {
                own.join(", ")
            }
        );
    }
    0
}

/// The Stop hook's answer for one session: the `{"decision": "block", ...}` line, or None for silence.
/// `signals` are the (interrupts, denials) read from the transcript the hook names; `reason` is the
/// policy over them (friction::reason).
///
/// ponytail: stop_hook_active means WE already blocked this stop once. Blocking again is a loop the
/// user cannot leave except by killing the session, so the second ask is never made.
pub fn hook_decision(
    hook: &Value,
    signals: impl FnOnce() -> (u32, u32),
    reason: impl Fn(u32, u32) -> String,
) -> Option<String> {
    let active = match hook.get("stop_hook_active") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Null) | None => false,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    };
    if active
        || hook
            .get("transcript_path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
    {
        return None;
    }
    let (interrupts, denials) = signals();
    let said = reason(interrupts, denials);
    if said.is_empty() {
        return None;
    }
    Some(format!(
        "{{\"decision\": \"block\", \"reason\": {}}}",
        json_str(&said)
    ))
}

/// Ask, when a session hit something worth remembering. The contract every agent is wired against.
///
/// Two ways in, one policy behind both:
///
/// ```text
/// gitdashy friction --interrupts N --denials N     any agent that can count its own signals
/// gitdashy friction --claude-hook                  Claude's Stop hook JSON on stdin, its JSON out
/// ```
///
/// Prints the reason and exits 0 when there is one; prints nothing when there is not. Silence is the
/// normal answer: most sessions are routine, and one that fires every time is a prompt nobody reads.
fn friction(claude_hook: bool, repo: Option<String>, interrupts: u32, denials: u32) -> i32 {
    if !claude_hook {
        let said = friction_mod::reason(interrupts, denials);
        if !said.is_empty() {
            println!("{said}");
        }
        return 0;
    }
    let mut raw = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut raw);
    let hook: Value = match serde_json::from_str(if raw.is_empty() { "{}" } else { &raw }) {
        Ok(v) => v,
        Err(_) => return 0, // ponytail: a hook that cannot parse its own input says nothing, never blocks a stop
    };
    let path = PathBuf::from(hook.get("transcript_path").and_then(Value::as_str).unwrap_or(""));
    // ponytail: the signals are read LAZILY, so the stop_hook_active early-out happens first.
    // claude_signals() parses the whole transcript, which is tens of MB by the end of a long session,
    // and this runs at every stop of every session: on a second stop the answer was thrown away.
    let signals = || {
        if path.as_os_str().is_empty() {
            (0, 0)
        } else {
            friction_mod::claude_signals(&path)
        }
    };
    let Some(answer) = hook_decision(&hook, signals, friction_mod::reason) else {
        return 0;
    };
    // ponytail: the repo is resolved HERE, not at the top. origin_slug() forks `git`, and this runs at
    // the end of every session in every repo; above the check it paid for that fork on every routine
    // session and then discarded the answer, which is ~99% of them.
    let repo = repo.filter(|r| !r.is_empty()).unwrap_or_else(here);
    // ponytail: a session that cannot be dated does NOT veto: 0.0 is a real instant every drafts file postdates.
    let filed = friction_mod::started_at(&path)
        .map(|w| friction_mod::filed_since(&repo, w))
        .unwrap_or(false);
    if !filed {
        println!("{answer}");
    }
    0
}

fn self_check(model: Option<String>) -> i32 {
    let model = model.unwrap_or_else(|| config::get().model);
    let rows = review_mod::self_check(&model);
    for r in &rows {
        println!(
            "{}  {}{}",
            if r.ok { "ok  " } else { "FAIL" },
            r.name,
            if r.ok {
                String::new()
            } else {
                format!("  ({})", r.detail)
            }
        );
    }
    if rows.iter().all(|r| r.ok) {
        0
    } else {
        1
    }
}

/// Dump the log to config.debug_log, panics included. The screen shows one line per failure; this keeps the rest.
fn debug(args: &[String]) {
    let path = config::get().debug_log;
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            // every PR url lands here
        }
        let cfg = simplelog::ConfigBuilder::new()
            .set_thread_level(::log::LevelFilter::Debug)
            .build();
        let _ = simplelog::WriteLogger::init(::log::LevelFilter::Debug, cfg, f);
    }
    config::update(|c| c.debug = true);
    // Log, then hand over to the default hook: a crash still prints to the terminal.
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ::log::error!(
            "uncaught in thread {}: {info}",
            std::thread::current().name().unwrap_or("?")
        );
        default(info);
    }));
    ::log::info!("gitdashy {VERSION} starting: {args:?}");
}

/// The dashboard: config from flags, the refresh loop, the server, then a window or the browser.
fn dashboard(cli: Cli) -> i32 {
    if cli.demo {
        demo::install();
    }
    config::load();
    let split = |v: Vec<String>| -> Vec<String> { v.into_iter().filter(|s| !s.is_empty()).collect() };
    config::update(|c| {
        if let Some(e) = cli.effort {
            c.effort = e;
        }
        if let Some(d) = cli.depth {
            c.depth = d;
        }
    });
    let depth = config::get().depth;
    if !config::DEPTHS.contains(&depth.as_str()) {
        println!(
            "gitdashy: --depth must be low, medium, high or adaptive, not {}",
            pyrepr(&depth)
        );
        return 0;
    }
    if let Some(v) = cli.voice {
        config::update(|c| c.voice = split(v));
    }
    let voice = config::get().voice;
    if voice.iter().any(|v| !config::VOICES.contains(&v.as_str())) {
        println!(
            "gitdashy: --voice must be from {}, not {}",
            config::VOICES.join(", "),
            pyrepr_list(&voice)
        );
        return 0;
    }
    if let Some(h) = cli.hunter {
        config::update(|c| c.hunter = split(h));
    }
    let hunter = config::get().hunter;
    if hunter.iter().any(|h| !config::HUNTERS.contains(&h.as_str())) {
        println!(
            "gitdashy: --hunter must be from {}, not {}",
            config::HUNTERS.join(", "),
            pyrepr_list(&hunter)
        );
        return 0;
    }
    if let Some(i) = cli.interval.filter(|i| !config::interval_ok(*i)) {
        println!(
            "gitdashy: --interval must be {}s to a day, not {i}",
            config::INTERVAL_MIN
        );
        return 0;
    }
    // ponytail: the same rule the file and the settings screen hold a model to. Taking it here and
    // dropping it on the next load is the worse answer: it works until you restart.
    if let Some(m) = cli.model.as_deref().filter(|m| !config::model_ok(m)) {
        println!(
            "gitdashy: --model must be a name of 1 to 60 characters, not {}",
            pyrepr(m)
        );
        return 0;
    }
    config::update(|c| {
        if let Some(i) = cli.instructions {
            c.instructions = i;
        }
        // ponytail: the flag only ever turns it ON. It is a bare bool, so absence is "not asked for"
        // and not "asked for off" — writing it through unconditionally would switch the setting off
        // on every launch that omitted the flag.
        if cli.inline {
            c.inline = true;
        }
        if let Some(i) = cli.interval {
            c.interval = i;
        }
        if let Some(m) = cli.model {
            c.model = m;
        }
    });
    // ponytail: last check before the screen goes up, and after the flags: a typo in --voice is still a
    // typo without a token. Nothing in the dashboard works without one, and three rows of "401 Bad
    // credentials" behind a window is a worse way to learn that than a message with the fix in it.
    if !cli.demo && github::token().is_empty() {
        println!("{NO_TOKEN}");
        return 0;
    }
    // ponytail: BEFORE activate(), which lists teams by looking in TEAMS. A move of the user's files is
    // said; a refusal stays on the Knowledge row.
    let moved = team::migrate();
    if !moved.is_empty() {
        let line = moved.strip_prefix("gitdashy: ").unwrap_or(&moved).to_string();
        if line.starts_with("moved your team checkout") {
            println!("gitdashy: {line}");
        } else {
            *team::ERROR.lock().unwrap_or_else(|e| e.into_inner()) = line.chars().take(60).collect();
        }
    }
    if let Some(done) = install_mod::retire(false)
        .into_iter()
        .find(|l| !l.starts_with("NOTE"))
    {
        println!("gitdashy: {done}");
    }
    team::activate();
    let state = crate::state::State::new();
    // ponytail: on screen, once. A field a damaged settings file lost is otherwise silent outside
    // --debug, and the next save writes the default over it without anyone having been told.
    let gone = config::dropped_settings();
    if !gone.is_empty() {
        state.lock().notices.push(format!(
            "settings: ignored {}; read the rest of the file",
            gone.join(", ")
        ));
    }
    if cli.auto {
        state.set_auto(true, false);
    }
    // ponytail: web::launch_asks was never called, so the desk asked nothing at launch and the
    // Knowledge row sat on "restart to be asked" for ever — restarting asked nothing either.
    state.lock().asks = crate::web::launch_asks();
    let notes = state.clone();
    std::thread::spawn(move || notes.lock().changelog = crate::update::changelog());
    if !config::get().demo {
        crate::lan::start(state.clone());
    }
    let looper = state.clone();
    std::thread::Builder::new()
        .name("refresh".into())
        .spawn(move || looper.run_loop())
        .expect("refresh thread");
    // ponytail: the token arrives in the ENVIRONMENT, not argv: argv is world-readable in ps, and this
    // token starts paid review runs. Removed so it does not ride along into the Claude subprocesses.
    let token = std::env::var("GITDASHY_GUI_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(crate::web::new_token);
    std::env::remove_var("GITDASHY_GUI_TOKEN");
    let port = match crate::web::serve(state.clone(), cli.port.unwrap_or(0), token.clone()) {
        Ok(p) => p,
        Err(e) => return fail(format!("gitdashy: could not serve: {e}")),
    };
    if cli.browser || cli.no_open {
        // a report is a throwaway: one left by an exit that ran no code goes now. ponytail: no
        // single-instance guard in browser mode, so a second --browser run clears the first one's report
        crate::report::clear();
        let url = format!("http://127.0.0.1:{port}/?token={token}");
        println!("gitdashy {VERSION} gui — {url}\n  ctrl-c to stop");
        let _ = std::io::stdout().flush();
        if !cli.no_open {
            github::open_in_browser(&url);
        }
        loop {
            std::thread::park();
        }
    }
    crate::shell::run(state, port, token);
    0
}

/// Run with the given args; the process exit code.
pub fn run(args: Vec<String>) -> i32 {
    if args.iter().any(|a| a == "--debug") || std::env::var_os("PRS_DEBUG").is_some_and(|v| !v.is_empty()) {
        debug(&args);
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return 0;
    }
    if args.iter().any(|a| a == "--version") {
        println!("gitdashy {VERSION}");
        return 0;
    }
    // ponytail: the app icon on stdout, so install.sh can write a .desktop icon from the binary it
    // just put down instead of fetching the same bytes from the repo again.
    if args.iter().any(|a| a == "--icon") {
        let Some((png, _)) = crate::web::asset("head.png") else {
            return fail("gitdashy: no icon in this build");
        };
        return match std::io::stdout().write_all(&png) {
            Ok(()) => 0,
            Err(e) => fail(format!("gitdashy: {e}")),
        };
    }
    // ponytail: an unknown subcommand is an ERROR, not the dashboard. `gitdashy api …` against a build
    // without that command fell through to the dashboard, which is how a review crashed rather than
    // being told the command was not there.
    if let Some(first) = args.first() {
        if !first.starts_with('-') && !COMMANDS.contains(&first.as_str()) {
            return fail(format!(
                "gitdashy: no command {} in {VERSION} — see gitdashy --help",
                pyrepr(first)
            ));
        }
    }
    let cli = match Cli::try_parse_from(std::iter::once("gitdashy".to_string()).chain(args)) {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            return e.exit_code();
        }
    };
    match cli.command {
        Some(Command::SyncMemory {
            into,
            repo,
            no_pull,
            general,
        }) => sync_memory(into, repo, no_pull, general),
        Some(Command::Remember {
            repo,
            general,
            private,
            fact,
        }) => remember(repo, general, private, fact),
        Some(Command::Install {
            full,
            corpus,
            dry_run,
            yes,
            no_setup,
            uninstall,
        }) => install(full, corpus, dry_run, yes, no_setup, uninstall),
        Some(Command::SelfReview { number, repo, model }) => self_review(number, repo, model),
        Some(Command::Setup) => setup(true),
        Some(Command::Init {
            into,
            loader,
            repo,
            forget,
        }) => init(into, loader, repo, forget),
        Some(Command::Bind {
            repo,
            team,
            forget,
            owner,
            list,
        }) => bind(repo, team, forget, owner, list),
        Some(Command::Auto {
            repo,
            owner,
            off,
            list,
        }) => auto_cmd(repo, owner, off, list),
        Some(Command::Inline {
            repo,
            owner,
            off,
            clear,
            list,
        }) => inline_cmd(repo, owner, off, clear, list),
        Some(Command::Db {
            target,
            db,
            off,
            forget,
        }) => db_cmd(target, db, off, forget),
        Some(Command::Friction {
            claude_hook,
            repo,
            interrupts,
            denials,
        }) => friction(claude_hook, repo, interrupts, denials),
        Some(Command::Api { path, diff }) => api(path, diff),
        Some(Command::Drafts { repo, count }) => drafts(repo, count),
        Some(Command::Teams {
            new,
            desc,
            at,
            join,
            name,
            team,
            connect,
            cover,
            uncover,
            leave,
            agents_again,
            publishing_again,
        }) => teams(
            new,
            desc,
            at,
            join,
            name,
            team,
            connect,
            cover,
            uncover,
            leave,
            agents_again,
            publishing_again,
        ),
        Some(Command::SelfCheck { model }) => self_check(model),
        None => dashboard(cli),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("gitdashy").chain(args.iter().copied())).unwrap()
    }

    /// A flag outside the range is refused BEFORE anything is written, so the config still holds what
    /// it held. The check sits above the token check, so this never reaches the server.
    #[test]
    fn a_bad_interval_flag_is_refused_before_anything_is_saved() {
        let _g = crate::config::test_lock();
        config::update(|c| {
            c.settings = None; // never read this machine's real settings file
            c.interval = 300;
        });
        assert_eq!(run(vec!["--interval".into(), "0".into()]), 0);
        assert_eq!(config::get().interval, 300, "a refused flag must not be saved");
        let model = config::get().model;
        assert_eq!(run(vec!["--model".into(), String::new()]), 0);
        assert_eq!(config::get().model, model);
    }

    /// --list is a question. It must answer before anything in this command writes, whatever else
    /// is on the line — the bug this pins armed acme/* and returned before ever reading the store.
    #[test]
    fn db_takes_one_thing_to_do_with_its_target() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        let store = d.path().join("dbrepo");
        config::update(|c| c.dbrepo = store.clone());
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            db_cmd(s("acme/api"), None, false, false),
            1,
            "a target with nothing to do"
        );
        assert_eq!(
            db_cmd(s("acme/api"), s("acme/x"), true, false),
            1,
            "a db repo and --off"
        );
        assert_eq!(db_cmd(s("acme/api"), None, true, true), 1, "--off and --forget");
        assert_eq!(db_cmd(None, None, true, false), 1, "--off with no target");
        assert!(!store.exists(), "a refused command wrote to the store");
        assert_eq!(db_cmd(None, None, false, false), 0, "a bare db reports");
        assert_eq!(db_cmd(s("acme/*"), s("acme/schema"), false, false), 0);
        assert_eq!(db_cmd(s("acme/docs"), None, true, false), 0);
        assert_eq!(crate::dbrepo::of("acme/docs"), "");
        assert_eq!(db_cmd(s("acme/docs"), None, false, true), 0);
        assert_eq!(crate::dbrepo::of("acme/docs"), "acme/schema");
    }

    /// In a team's repo a session drafts where a review would, the team's pool; --private keeps it yours.
    #[test]
    fn remember_drafts_into_the_team_unless_private() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        config::update(|c| {
            c.demo = false;
            c.memory_dir = root.join("mine");
            c.local_memory = root.join("mine");
            c.teams = root.join("teams");
            c.bindings = root.join("bindings");
            c.log = root.join("log.jsonl");
            c.local_log = root.join("log.jsonl");
            c.registry = root.join("mirrors");
            c.settings = None;
        });
        std::fs::create_dir_all(root.join("mine")).unwrap();
        std::fs::create_dir_all(root.join("teams/org-t/.git")).unwrap();
        std::fs::create_dir_all(root.join("teams/org-t/memory")).unwrap();
        memory::allow_publishing("org-t", true);
        assert_eq!(bind_mod::bind("a/b", "org-t"), "");
        let me = memory::whoami();
        let pooled = root.join("teams/org-t/memory/drafts").join(&me).join("a__b.md");

        assert_eq!(
            remember(Some("a/b".into()), false, true, vec!["kept to myself".into()]),
            0
        );
        assert_eq!(memory::drafts(Some("a/b")), [(1, "kept to myself".to_string())]);
        assert!(!pooled.exists(), "--private never reaches the team");

        assert_eq!(
            remember(
                Some("a/b".into()),
                false,
                false,
                vec!["the team should know".into()]
            ),
            0
        );
        assert!(std::fs::read_to_string(&pooled)
            .unwrap()
            .contains("the team should know"));
        assert_eq!(memory::drafts(Some("a/b")).len(), 1, "and nothing more in yours");
    }

    /// A general fact filed in a repo bound to no team is that repo's own; your general file only takes what is
    /// filed with no repo at all.
    #[test]
    fn remember_general_in_an_unbound_repo_stays_with_that_repo() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        config::update(|c| {
            c.demo = false;
            c.memory_dir = root.join("mine");
            c.local_memory = root.join("mine");
            c.teams = root.join("teams");
            c.bindings = root.join("bindings");
            c.log = root.join("log.jsonl");
            c.local_log = root.join("log.jsonl");
            c.registry = root.join("mirrors");
            c.settings = None;
        });
        std::fs::create_dir_all(root.join("mine")).unwrap();
        std::fs::create_dir_all(root.join("teams/org-t/.git")).unwrap();
        std::fs::create_dir_all(root.join("teams/org-t/memory")).unwrap();
        memory::allow_publishing("org-t", true);
        assert_eq!(
            remember(
                Some("me/side-project".into()),
                true,
                false,
                vec!["pin the toolchain".into()]
            ),
            0
        );
        assert_eq!(
            memory::drafts(Some("me/side-project")),
            [(1, "pin the toolchain".to_string())]
        );
        assert!(memory::drafts(None).is_empty(), "not your general file");
        assert!(
            !root.join("teams/org-t/memory/drafts").exists(),
            "nor the one team this machine is in"
        );
        // unless it is said to be yours: --private --general is your general file, wherever you stand
        assert_eq!(
            remember(
                Some("me/side-project".into()),
                true,
                true,
                vec!["I prefer small commits".into()]
            ),
            0
        );
        assert_eq!(memory::drafts(None), [(1, "I prefer small commits".to_string())]);
    }

    #[test]
    fn auto_list_answers_without_writing_whatever_else_is_asked() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        let store = d.path().join("autorev");
        config::update(|c| c.autorev = store.clone());

        assert_eq!(auto_cmd(None, Some("acme".into()), false, true), 0);
        assert_eq!(auto_cmd(Some("acme/api".into()), None, false, true), 0);
        assert_eq!(
            auto_cmd(Some("not-a-slug".into()), None, false, true),
            0,
            "a typo is not an error for a question"
        );
        assert!(!store.exists(), "--list wrote to the store");
        assert!(crate::autorev::scope().everywhere());

        // and without --list the same calls do write
        assert_eq!(auto_cmd(Some("acme/api".into()), None, false, false), 0);
        assert!(crate::autorev::scope().armed("acme/api") && !crate::autorev::scope().armed("other/thing"));
        assert_eq!(auto_cmd(None, Some("beta".into()), false, false), 0);
        assert!(crate::autorev::scope().armed("beta/anything"));
        assert_eq!(auto_cmd(Some("acme/api".into()), None, true, false), 0);
        assert!(
            !crate::autorev::scope().armed("acme/api"),
            "--off disarms it again"
        );
    }

    /// A bare `gitdashy auto` reports: naming no repo and asking for no change is a question, and
    /// answering it by arming whatever directory you are standing in is a write nobody asked for.
    #[test]
    fn a_bare_auto_reports_rather_than_arming_here() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        let store = d.path().join("autorev");
        config::update(|c| c.autorev = store.clone());
        assert_eq!(auto_cmd(None, None, false, false), 0);
        assert!(!store.exists());
    }

    /// Which wins when both are given: neither. Before the typo guard moved above --owner,
    /// `auto not-a-slug --owner acme` armed the org and swallowed the typo.
    #[test]
    fn auto_refuses_a_repo_and_an_owner_together() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        let store = d.path().join("autorev");
        config::update(|c| c.autorev = store.clone());
        assert_eq!(
            auto_cmd(Some("acme/api".into()), Some("beta".into()), false, false),
            1
        );
        assert_eq!(
            auto_cmd(None, None, true, false),
            1,
            "--off with nothing to turn off is a question asked wrong, not a report"
        );
        assert_eq!(
            auto_cmd(Some("not-a-slug".into()), Some("acme".into()), false, false),
            1,
            "the typo is caught before --owner arms anything"
        );
        assert!(!store.exists());
    }

    #[test]
    fn auto_refuses_a_key_it_cannot_read() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| c.autorev = d.path().join("autorev"));
        assert_eq!(auto_cmd(Some("notes".into()), None, false, false), 1);
        assert_eq!(auto_cmd(None, Some("acme/api".into()), false, false), 1);
        assert!(crate::autorev::scope().everywhere());
    }

    #[test]
    fn top_level_flags_parse() {
        let c = parse(&[
            "--interval",
            "60",
            "--auto",
            "--model",
            "sonnet",
            "--effort",
            "high",
            "--depth",
            "low",
            "--voice",
            "review,caveman",
            "--hunter",
            "ponytail",
            "--instructions",
            "f.txt",
            "--demo",
            "--debug",
            "--browser",
            "--no-open",
            "--port",
            "8080",
        ]);
        assert_eq!(c.interval, Some(60));
        assert!(c.auto && c.demo && c.debug && c.browser && c.no_open);
        assert_eq!(c.model.as_deref(), Some("sonnet"));
        assert_eq!(c.effort.as_deref(), Some("high"));
        assert_eq!(c.depth.as_deref(), Some("low"));
        assert_eq!(c.voice, Some(vec!["review".into(), "caveman".into()]));
        assert_eq!(c.hunter, Some(vec!["ponytail".into()]));
        assert_eq!(c.instructions.as_deref(), Some("f.txt"));
        assert_eq!(c.port, Some(8080));
        assert!(c.command.is_none());
    }

    #[test]
    fn every_subcommand_parses() {
        assert!(matches!(
            parse(&[
                "sync-memory",
                "--into",
                "~/mem",
                "--repo",
                "a/b",
                "--no-pull",
                "--general"
            ])
            .command,
            Some(Command::SyncMemory {
                no_pull: true,
                general: true,
                ..
            })
        ));
        let Some(Command::Remember {
            repo, general, fact, ..
        }) = parse(&["remember", "--repo", "other/thing", "migrations", "run", "first"]).command
        else {
            panic!()
        };
        assert_eq!(
            (repo.as_deref(), general, fact.join(" ").as_str()),
            (Some("other/thing"), false, "migrations run first")
        );
        let Some(Command::Remember { fact, general, .. }) =
            parse(&["remember", "--general", "PHI reaches the frontend"]).command
        else {
            panic!()
        };
        assert!(general && fact == vec!["PHI reaches the frontend"]);
        assert!(matches!(
            parse(&[
                "install",
                "--full",
                "--corpus",
                "http://x",
                "--dry-run",
                "--yes",
                "--no-setup"
            ])
            .command,
            Some(Command::Install {
                full: true,
                dry_run: true,
                yes: true,
                no_setup: true,
                uninstall: false,
                ..
            })
        ));
        assert!(matches!(
            parse(&["install", "--uninstall"]).command,
            Some(Command::Install { uninstall: true, .. })
        ));
        let Some(Command::Auto {
            repo,
            owner,
            off,
            list,
        }) = parse(&["auto", "acme/api", "--owner", "acme", "--off", "--list"]).command
        else {
            panic!()
        };
        assert_eq!(
            (repo.as_deref(), owner.as_deref(), off, list),
            (Some("acme/api"), Some("acme"), true, true)
        );
        assert!(matches!(
            parse(&["auto"]).command,
            Some(Command::Auto {
                repo: None,
                owner: None,
                off: false,
                list: false
            })
        ));
        let Some(Command::Inline {
            repo,
            owner,
            off,
            clear,
            list,
        }) = parse(&[
            "inline", "acme/api", "--owner", "acme", "--off", "--clear", "--list",
        ])
        .command
        else {
            panic!()
        };
        assert_eq!(
            (repo.as_deref(), owner.as_deref(), off, clear, list),
            (Some("acme/api"), Some("acme"), true, true, true)
        );
        assert!(matches!(
            parse(&["inline"]).command,
            Some(Command::Inline {
                repo: None,
                owner: None,
                off: false,
                clear: false,
                list: false
            })
        ));
        assert!(matches!(
            parse(&["self-review", "12", "--repo", "a/b", "--model", "opus"]).command,
            Some(Command::SelfReview { number: Some(12), .. })
        ));
        assert!(matches!(parse(&["setup"]).command, Some(Command::Setup)));
        assert!(matches!(
            parse(&["init", "--into", "d", "--loader", "f", "--repo", "a/b"]).command,
            Some(Command::Init { forget: false, .. })
        ));
        assert!(matches!(
            parse(&["init", "--into", "d", "--forget"]).command,
            Some(Command::Init { forget: true, .. })
        ));
        let Some(Command::Bind { repo, team, list, .. }) = parse(&["bind", "not-a-slug", "--list"]).command
        else {
            panic!()
        };
        assert!(repo.as_deref() == Some("not-a-slug") && team.is_none() && list);
        assert!(matches!(
            parse(&["bind", "--owner", "acme", "--team", "org-mem", "--forget"]).command,
            Some(Command::Bind { forget: true, .. })
        ));
        assert!(matches!(
            parse(&["friction", "--interrupts", "4", "--denials", "1"]).command,
            Some(Command::Friction {
                claude_hook: false,
                interrupts: 4,
                denials: 1,
                ..
            })
        ));
        assert!(matches!(
            parse(&["friction", "--claude-hook", "--repo", "a/b"]).command,
            Some(Command::Friction {
                claude_hook: true,
                ..
            })
        ));
        let Some(Command::Api { path, diff }) = parse(&["api", "/repos/a/b/compare/x...y", "--diff"]).command
        else {
            panic!()
        };
        assert!(path.as_deref() == Some("/repos/a/b/compare/x...y") && diff);
        assert!(matches!(
            parse(&["drafts", "--count", "--repo", "a/b"]).command,
            Some(Command::Drafts { count: true, .. })
        ));
        assert!(matches!(
            parse(&["teams", "--team", "k", "--cover", "acme/*"]).command,
            Some(Command::Teams { cover: Some(_), .. })
        ));
        assert!(matches!(
            parse(&["teams", "--new", "n", "--desc", "d", "--at", "dir"]).command,
            Some(Command::Teams { .. })
        ));
        assert!(matches!(
            parse(&["teams", "--join", "x/y", "--name", "n"]).command,
            Some(Command::Teams { .. })
        ));
        assert!(matches!(
            parse(&["teams", "--team", "k", "--agents-again"]).command,
            Some(Command::Teams {
                agents_again: true,
                publishing_again: false,
                ..
            })
        ));
        assert!(matches!(
            parse(&["teams", "--leave", "k"]).command,
            Some(Command::Teams { leave: Some(_), .. })
        ));
        assert!(matches!(
            parse(&["self-check", "--model", "opus"]).command,
            Some(Command::SelfCheck { .. })
        ));
    }

    #[test]
    fn a_flag_with_no_value_is_an_error() {
        let r = Cli::try_parse_from(["gitdashy", "remember", "a fact", "--repo"]);
        assert!(r.is_err());
    }

    #[test]
    fn help_version_and_unknown_commands() {
        assert_eq!(run(vec!["--version".into()]), 0);
        assert_eq!(run(vec!["--help".into()]), 0);
        assert_eq!(run(vec!["bogus".into()]), 1);
        assert_eq!(run(vec!["pr".into(), "view".into(), "7".into()]), 1);
        assert!(USAGE.contains("sync-memory") && USAGE.contains("remember"));
    }

    #[test]
    fn sync_memory_and_remember_need_their_arguments() {
        assert_eq!(run(vec!["sync-memory".into()]), 1);
        assert_eq!(run(vec!["remember".into()]), 1);
        assert_eq!(run(vec!["api".into()]), 1);
        assert_eq!(
            run(vec!["api".into(), "https://evil.example.com/collect?t=".into()]),
            1
        );
        assert_eq!(run(vec!["api".into(), "//evil.example.com/x".into()]), 1);
    }

    #[test]
    fn hook_decision_blocks_only_a_first_stop_with_friction() {
        let reason = |i: u32, d: u32| {
            if i >= 3 || d >= 2 {
                "you interrupted a lot".to_string()
            } else {
                String::new()
            }
        };
        let hook: Value =
            serde_json::from_str(r#"{"transcript_path": "/tmp/t.jsonl", "stop_hook_active": false}"#)
                .unwrap();
        assert_eq!(
            hook_decision(&hook, || (4, 0), reason).as_deref(),
            Some(r#"{"decision": "block", "reason": "you interrupted a lot"}"#)
        );
        assert_eq!(hook_decision(&hook, || (0, 0), reason), None);
        let again: Value =
            serde_json::from_str(r#"{"transcript_path": "/tmp/t.jsonl", "stop_hook_active": true}"#).unwrap();
        assert_eq!(hook_decision(&again, || (4, 0), reason), None);
        let none: Value = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(hook_decision(&none, || (4, 0), reason), None);
    }

    #[test]
    fn json_helpers_match_python() {
        let v: Value = serde_json::json!({"number": 7, "t": "é"});
        assert_eq!(json_indent1(&v), "{\n \"number\": 7,\n \"t\": \"\\u00e9\"\n}");
        assert_eq!(json_str("a\"b"), "\"a\\\"b\"");
        assert_eq!(pyrepr_list(&["a".into(), "b".into()]), "['a', 'b']");
        assert_eq!(pyrepr("x"), "'x'");
    }

    #[test]
    fn expanduser_expands_only_a_leading_tilde() {
        let home = config::home();
        assert_eq!(expanduser("~/mem"), home.join("mem"));
        assert_eq!(expanduser("/x/~"), PathBuf::from("/x/~"));
    }
}
