//! The HTTP server the React frontend talks to. Port of dashy/ui/web.py. Serves the embedded
//! Vite bundle, /api/* JSON, and guards the API with the X-Dashy-Token header (or ?token= on the page).
//!
//! ponytail: tiny_http, no framework. One State, one refresh thread, and one JSON route per thing
//! the dashboard can do: the page polls /api/state and posts back. The desktop window is pointed
//! at this same URL, so there is one UI to maintain and it works in a browser too.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use include_dir::{include_dir, Dir};
use log::{debug, error};
use serde_json::{json, Map, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::state::{last_line, now, State};
use crate::types::{DiffFile, Finding, LogEntry, Mark, Pr, Verdict};
use crate::{
    autorev, bind, config, dbrepo, dbschema, diff, founding, github, held, install, knowledge,
    log as review_log, memory, report, review, spells, story, team, textdiff, update,
};

/// The built Vite app, embedded so the binary stays self-contained. `pnpm build` must run before cargo.
static DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../dist");

/// An error the page should read: (status, message).
#[derive(Debug)]
pub struct Fail(pub u16, pub String);

impl Fail {
    fn new(code: u16, msg: impl Into<String>) -> Fail {
        Fail(code, msg.into())
    }
}

/// Anything else is a 500 with its last line, so a broken action is a flash on the page, not a dead poll.
impl From<anyhow::Error> for Fail {
    fn from(e: anyhow::Error) -> Fail {
        Fail(500, last_line(&format!("{e:#}"), 160, "error"))
    }
}

impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Fail {
        Fail(500, last_line(&e.to_string(), 160, "error"))
    }
}

type Out = Result<Value, Fail>;
/// The query string, first value per key.
type Query = HashMap<String, String>;

/// Everything the settings can change, in the shape config::save writes.
pub fn snapshot() -> Value {
    let mut c = config::get();
    // a spell whose file was deleted stays saved but is not shown, so no surface offers a cast that 400s
    c.spells.retain(|n| spells::get(n).is_some());
    serde_json::to_value(config::snapshot(&c)).unwrap_or_default()
}

fn team_error() -> String {
    team::ERROR.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Everything one frame of the GUI needs, as plain JSON.
pub fn payload(state: &State) -> Value {
    // one read for the count, the flag and the rows, so they cannot describe different files
    let auto_scope = autorev::scope();
    // ponytail: one read of the held store for the whole payload, and it reads the NAMES — asking
    // per row would open the directory once per PR on every poll, and the verdicts inside it are not
    // what a row mark needs.
    let waiting = held::waiting();
    let (
        sections,
        reviews,
        busy,
        since,
        error,
        arrived,
        fetched_at,
        fetching,
        ticks,
        auto,
        pending,
        update,
        asks,
        notices,
        changelog,
    ) = {
        let inner = state.lock();
        (
            inner.sections.clone(),
            inner.reviews.clone(),
            inner.running.clone(),
            inner.since.clone(),
            inner.error.clone(),
            inner.arrived.clone(),
            inner.fetched_at,
            inner.fetching,
            inner.ticks,
            inner.auto,
            inner.pending_rr(&|r| auto_scope.armed(r)).len(),
            inner.update.clone(),
            inner.asks.clone(),
            inner.notices.clone(),
            inner.changelog.clone(),
        )
    };
    let cfg = config::get();
    let (peers, peers_auto) = crate::lan::peers();
    // ponytail: ONE resolver per frame, like the curses screen used to
    let (resolve, (repos, owners)) = bind::resolver_and_maps();
    // each url's newest review, and its newest tagged one, so a re-review without a kind keeps the earlier tag.
    // REVIEWED is newest first, so the first one seen wins.
    let in_log = crate::log::reviewed_urls();
    let mut logged: HashMap<&str, &LogEntry> = HashMap::new();
    let mut tags: HashMap<&str, &LogEntry> = HashMap::new();
    for p in sections
        .iter()
        .filter(|s| s.name == "REVIEWED")
        .flat_map(|s| s.prs.iter().flatten())
    {
        if let Some(r) = &p.review {
            logged.entry(p.url.as_str()).or_insert(r);
            if !r.kind.is_empty() {
                tags.entry(p.url.as_str()).or_insert(r);
            }
        }
    }
    let mut out = Vec::new();
    let team_repos = team::team_repos();
    for s in &sections {
        let mut rows = Vec::new();
        for p in s.prs.iter().flatten() {
            let url = p.url.as_str();
            let pre = if s.name == "MINE" {
                review::self_review_state(p)
            } else {
                (0.0, false)
            };
            let (summary, review_at) = match (&s.name[..], &p.review) {
                ("REVIEWED", Some(r)) => (r.summary.as_str(), r.at.as_str()),
                ("REVIEWED", None) => ("", ""),
                _ => (logged.get(url).map(|r| r.summary.as_str()).unwrap_or(""), ""),
            };
            // a REVIEWED row carries its own review, when that one is tagged; else the url's newest tagged
            let tagged = p
                .review
                .as_deref()
                .filter(|r| !r.kind.is_empty())
                .or_else(|| tags.get(url).copied());
            rows.push(json!({
                "url": url,
                "number": p.number,
                "title": p.title,
                "repo": p.repo(),
                "author": p.author(),
                "updatedAt": p.updated_at,
                "isDraft": p.is_draft,
                "add": p.additions,
                "del": p.deletions,
                "status": p.status,
                "prev": p.prev,
                "checks": p.checks,
                "reviewers": p.reviewers,
                "review": reviews.get(url).cloned().unwrap_or_default(),
                "busy": busy.contains(url),
                "since": since.get(url),
                "team": resolve(p.repo()),
                "summary": summary,
                "reviewAt": review_at,
                "kind": tagged.map(|r| r.kind.as_str()).unwrap_or(""),
                "breaking": tagged.is_some_and(|r| r.breaking),
                "db": p.review.as_deref().or_else(|| logged.get(url).copied()).is_some_and(changes_db),
                "scores": p.review.as_deref().or_else(|| logged.get(url).copied()).map(|r| r.scores.as_slice()).unwrap_or_default(),
                // whether the pane will have an AI REVIEW section: detail() reads the whole log, so
                // this must too. `logged` above only covers the REVIEWED section, and `review` is
                // this session's status string, which says nothing about the log at all.
                "reviewed": p.review.is_some() || in_log.contains(url),
                "pre": pre_json(pre),
                // a finished review nobody has posted yet. The row says so, because a verdict
                // sitting in a file nothing points at is a verdict nobody reads.
                "waiting": held::is_waiting(&waiting, p.repo(), p.number),
                // a team's memory repo: approved by a person, never reviewed by a model
                "humanOnly": team::in_repos(&team_repos, p.repo()),
            }));
        }
        out.push(json!({"name": s.name, "prs": rows, "error": s.err.clone().unwrap_or_default()}));
    }
    let names = team::joined();
    let scopes = github::scope_options(&cfg.scopes, &sections, &names, &repos, &owners);
    json!({
        "version": config::VERSION,
        // your login, so the board can set your own PRs apart; "" until the first fetch has asked
        "me": if cfg.demo { "alice".to_string() } else { github::me_cached() },
        "sections": out,
        "fetchedAt": fetched_at,
        "interval": cfg.interval,
        "fetching": fetching,
        "ticks": ticks,
        "error": error,
        "auto": auto,
        // other gitdashys on the LAN, by a random per-launch id: see lan.rs
        "peers": peers,
        "peersAuto": peers_auto,
        // ponytail: the boolean, not "is the list empty". A store holding nothing but an --off row
        // is a non-empty list while auto still covers everything, so a page deriving the rule from
        // the rows gets it backwards. The rule lives in autorev.rs and says so here.
        "autoEverywhere": auto_scope.everywhere(),
        "autoScope": auto_scope.listed().into_iter().map(|(t, on)| json!({"target": t, "on": on})).collect::<Vec<_>>(),
        // every posting rule on this machine, so the rail can show the whole picture rather than
        // one repo's answer with no way to see what else is set
        "postingRules": posting_rules_json(&board_repos(&sections)),
        "dbRules": dbrepo::rules().listed().into_iter().map(|(t, db)| json!({"target": t, "db": db})).collect::<Vec<_>>(),
        "pending": pending,
        "model": cfg.model,
        "running": busy.len(),
        "update": update,
        "settings": snapshot(),
        "options": {"model": cfg.models, "depth": config::DEPTHS, "effort": config::EFFORTS, "voice": config::VOICES,
                    "hunter": config::HUNTERS, "subs": config::SUBS, "window": config::WINDOWS,
                    "interval": config::INTERVALS, "theme": config::THEMES,
                    "scopes": scopes},
        "knowledge": {
            "report": {
                "job": job("report"),
                "latest": report::latest().and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned())),
            },
            "memory": knowledge::show(&knowledge::effective()) + &knowledge::history_note(),
            "store": if knowledge::store_moved() { knowledge::show(&cfg.teams) } else { String::new() },
            "teams": names.iter().map(|k| json!({"key": k, "name": team::info(k).name, "arrived": arrived.get(k).copied().unwrap_or(0)})).collect::<Vec<_>>(),
            "teamError": team_error(),
            "notes": install::session_notes(),
            // ponytail: a row you can act on, not a note you cannot — see memory::pending_answers.
            "waiting": memory::pending_answers().into_iter()
                .map(|(kind, key, what)| json!({"kind": kind, "key": key, "what": what}))
                .collect::<Vec<_>>(),
        },
        "asks": asks,
        "notices": notices,
        "changelog": changelog,
    })
}

fn pre_json((at, moved): (f64, bool)) -> Value {
    if at != 0.0 {
        json!({"at": at, "moved": moved})
    } else {
        Value::Null
    }
}

/// The review found the PR changes the database: a table added, altered or dropped, or a risk. A PR that only
/// reads and writes rows is ordinary code and gets no mark.
fn changes_db(r: &LogEntry) -> bool {
    let Some(db) = &r.db else { return false };
    let changed = db["tables"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|t| matches!(t["change"].as_str(), Some("added" | "altered" | "dropped")));
    changed || db["risks"].as_array().is_some_and(|a| !a.is_empty())
}

/// The newest review of this PR: the row's own on a REVIEWED row, else the log's.
fn last_review(pr: &Pr) -> Option<LogEntry> {
    pr.review
        .as_deref()
        .cloned()
        .or_else(|| review_log::last(&pr.url))
}

/// The checked findings of a log entry, through the same filter a verdict gets.
fn entry_findings(rev: &LogEntry) -> Vec<Finding> {
    let v = Verdict {
        verdict: rev.verdict.clone(),
        findings: rev
            .findings
            .iter()
            .map(|f| serde_json::to_value(f).unwrap_or_default())
            .collect(),
        ..Default::default()
    };
    review_log::findings(&v)
}

/// The side pane's frame for one PR: its size, its CI checks, the brief and the last review of it.
///
/// ponytail: the same want_detail the curses pane used, so a row costs one GitHub query per revision.
/// It answers pending while that query is in flight; the page keeps polling and the pane fills in.
pub fn detail(state: &State, pr: &Pr, section: &str) -> Value {
    let d = state.want_detail(pr);
    let rev = last_review(pr);
    let repo = pr.repo();
    let (text, whose) = memory::brief(Some(repo), Some(&bind::of(repo)));
    let pre = if section == "MINE" {
        review::self_review_state(pr)
    } else {
        (0.0, false)
    };
    json!({
        "url": pr.url,
        "pending": d.is_none(),
        "branch": d.as_ref().map(|d| d.branch.clone()).unwrap_or_default(),
        "add": d.as_ref().and_then(|d| d.add),
        "del": d.as_ref().and_then(|d| d.del),
        "files": d.as_ref().and_then(|d| d.files),
        "checks": d.as_ref().map(|d| d.checks.clone()).unwrap_or_default(),
        "brief": {"whose": whose, "empty": text.is_empty()},
        "pre": pre_json(pre),
        "spells": review::spell_results(pr.repo(), pr.number)
            .into_iter()
            // quotes: it repeats the spell word for word, so posting it would make the spell public
            .map(|(name, text, at)| {
                let quotes = spells::get(&name).is_some_and(|s| review::quotes(&text, &s));
                json!({"name": name, "text": text, "at": at, "quotes": quotes})
            })
            .collect::<Vec<_>>(),
        "review": rev.as_ref().map(|rev| json!({
            "verdict": config::status(&rev.verdict).unwrap_or(""),
            "summary": rev.summary,
            "model": rev.model,
            "tag": review_log::tag(rev),
            "at": rev.at,
            "findings": entry_findings(rev),
            "db": rev.db,
            "text": if rev.pr.url.is_empty() { rev.body.clone() } else { review_log::detail(rev) },
        })),
    })
}

/// A mark landed on this line: the same file, the new-file line it names, and not a removed line.
fn on_line(m: &Mark, file: usize, line: &crate::types::Line) -> bool {
    m.file == file && m.n != 0 && line.n == Some(m.n) && line.del.is_none()
}

/// The code viewer's diff as a flat list of rows, the review's comments under the lines they are about.
///
/// ponytail: ported from the curses pane as-is. Every mark gets a row: on its line when the diff has
/// that line, as an orphan when it does not; a finding is kept only if it can be read.
pub fn code_rows(files: &[DiffFile], marks: &[Mark]) -> Vec<Value> {
    let mut rows = Vec::new();
    let mut landed = vec![false; marks.len()];
    for (fi, f) in files.iter().enumerate() {
        rows.push(json!({"kind": "file", "path": f.path, "add": f.add, "dele": f.dele}));
        for hunk in &f.hunks {
            rows.push(json!({"kind": "hunk", "header": hunk.header}));
            for l in &hunk.lines {
                rows.push(
                    json!({"kind": "line", "n": l.n, "sign": l.sign, "text": l.text, "del": l.del,
                                 "mark": diff::worst(l)}),
                );
                for (mi, m) in marks.iter().enumerate() {
                    if on_line(m, fi, l) {
                        rows.push(json!({"kind": "note", "mark": m.kind, "text": m.text}));
                        landed[mi] = true;
                    }
                }
            }
        }
        rows.push(json!({"kind": "gap"}));
    }
    for (mi, m) in marks.iter().enumerate() {
        if landed[mi] {
            continue;
        }
        // ponytail: `file` past the list is how a mark says the diff does not touch that file
        let why = if m.file >= files.len() {
            "not in this diff"
        } else {
            "line not in this diff"
        };
        rows.push(json!({"kind": "orphan", "mark": m.kind, "text": m.text, "loc": m.loc, "why": why}));
    }
    rows
}

/// The review against the code it is about. pending until the diff has been read.
pub fn code(state: &State, pr: &Pr, scope: &str, context: usize) -> Value {
    let Some(rev) = last_review(pr) else {
        return json!({"url": pr.url, "pending": false, "rows": [],
                      "empty": "no review yet — r reviews this PR, p pre-reviews it"});
    };
    let Some((files, marks)) = state.want_diff(pr.repo(), pr.number, &pr.head, &entry_findings(&rev)) else {
        return json!({"url": pr.url, "pending": true, "rows": []});
    };
    if files.is_empty() {
        return json!({"url": pr.url, "pending": false, "rows": [],
                      "empty": "no diff to show — GitHub could not read it, or nothing changed"});
    }
    let scoped = scope == "marks";
    // the review's comments show in both scopes; the scope only decides how much code is around them
    let rows = if scoped {
        code_rows(&diff::narrow(&files, context), &marks)
    } else {
        code_rows(&files, &marks)
    };
    if scoped && rows.is_empty() {
        return json!({"url": pr.url, "pending": false, "rows": [],
                      "empty": "the review marked nothing — D shows the whole diff"});
    }
    json!({"url": pr.url, "pending": false, "rows": rows})
}

/// The PR on the board with this url, and the section it is in.
pub fn find_pr(state: &State, url: &str) -> Option<(Pr, String)> {
    let inner = state.lock();
    inner.sections.iter().find_map(|s| {
        s.prs
            .iter()
            .flatten()
            .find(|p| p.url == url)
            .map(|p| (p.clone(), s.name.clone()))
    })
}

fn need_pr(state: &State, url: &str) -> Result<(Pr, String), Fail> {
    find_pr(state, url).ok_or_else(|| Fail::new(404, "no such pr"))
}

// ---------------------------------------------------------------- background jobs (dream, scan)

/// One named job: when it started, whether its thread is still going, and what it said.
#[derive(Default)]
struct Job {
    t0: f64,
    running: bool,
    result: Option<Value>,
    error: String,
}

type Jobs = Mutex<HashMap<String, Arc<Mutex<Job>>>>;
static JOBS: std::sync::LazyLock<Jobs> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn jobs() -> std::sync::MutexGuard<'static, HashMap<String, Arc<Mutex<Job>>>> {
    JOBS.lock().unwrap_or_else(|e| e.into_inner())
}

fn job_of(name: &str) -> Option<Arc<Mutex<Job>>> {
    jobs().get(name).cloned()
}

/// Run `f` on a thread; job() reports on it. One at a time per name: false, and nothing started, when one
/// of that name is already running. The check and the start happen under one lock, so two requests
/// arriving together cannot both start.
pub fn start_job(name: &str, f: impl FnOnce() -> anyhow::Result<Value> + Send + 'static) -> bool {
    let mut all = jobs();
    if let Some(j) = all.get(name) {
        if j.lock().unwrap_or_else(|e| e.into_inner()).running {
            return false;
        }
    }
    let j = Arc::new(Mutex::new(Job {
        t0: now(),
        running: true,
        result: None,
        error: String::new(),
    }));
    all.insert(name.to_string(), j.clone());
    drop(all);
    let name = name.to_string();
    std::thread::spawn(move || {
        let got = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        let mut j = j.lock().unwrap_or_else(|e| e.into_inner());
        match got {
            Ok(Ok(v)) => j.result = Some(v),
            Ok(Err(e)) => {
                error!("{name} failed: {e:#}"); // surfaced in the panel
                j.error = last_line(&format!("{e:#}"), 120, "?");
            }
            Err(e) => {
                let text = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()));
                j.error = last_line(text.as_deref().unwrap_or("?"), 120, "?");
            }
        }
        j.running = false;
    });
    true
}

pub fn job(name: &str) -> Value {
    let Some(j) = job_of(name) else {
        return json!({"running": false, "idle": true});
    };
    let j = j.lock().unwrap_or_else(|e| e.into_inner());
    json!({"running": j.running, "elapsed": (now() - j.t0) as i64, "error": j.error,
           "result": if j.running { Value::Null } else { j.result.clone().unwrap_or(Value::Null) }})
}

/// "mine/a__b.md" -> "mine/a/b": the file name as the dream viewer shows it.
fn dream_name(n: &str) -> String {
    n.strip_suffix(".md").unwrap_or(n).replace("__", "/")
}

fn lookup<'a>(pairs: &'a [(String, String)], name: &str) -> &'a str {
    pairs
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, t)| t.as_str())
        .unwrap_or("")
}

/// Summary plus a unified diff per changed file.
pub fn dream_detail(summary: &str, before: &[(String, String)], new: &[(String, String)]) -> String {
    let mut out = vec![summary.trim().to_string(), String::new()];
    for (n, t) in new {
        let was = lookup(before, n);
        if t.trim() != was.trim() {
            let d = textdiff::unified(&dream_name(n), was, t.trim());
            if !d.is_empty() {
                out.push(d);
            }
            out.push(String::new());
        }
    }
    let text = out.join("\n");
    if text.trim().is_empty() {
        "nothing changed".into()
    } else {
        text
    }
}

pub fn dream_result((summary, before, new): memory::Dream) -> Value {
    // ponytail: everything the page is told comes from the same filter write() applies. The rows, the
    // deletion count and the full diff all used the raw answer, so the panel could name a team file
    // losing nine lines and then not touch it — a promise the apply drops. `theirs` is how many were
    // read and left alone, because a file simply missing from the list reads as one never looked at.
    let mine = memory::writable(&new);
    let theirs = new.len() - mine.len();
    let mut gone: Vec<&str> = mine
        .iter()
        .filter(|(n, t)| t.trim().is_empty() && !lookup(&before, n).trim().is_empty())
        .map(|(n, _)| n.as_str())
        .collect();
    gone.sort();
    let mut names: Vec<&str> = mine.iter().map(|(n, _)| n.as_str()).collect();
    names.sort_by_key(|n| (!gone.contains(n), n.to_string()));
    let files: Vec<Value> = names
        .iter()
        .map(|n| {
            json!({"name": dream_name(n), "before": lookup(&before, n).lines().count(),
                   "after": lookup(&mine, n).lines().count(), "deleted": gone.contains(n)})
        })
        .collect();
    let lost: usize = gone.iter().map(|n| lookup(&before, n).lines().count()).sum();
    let new_obj: Map<String, Value> = mine
        .iter()
        .map(|(n, t)| (n.clone(), Value::String(t.clone())))
        .collect();
    json!({"summary": summary, "files": files, "lost": lost, "theirs": theirs,
           "detail": dream_detail(&summary, &before, &mine), "new": new_obj})
}

// ---------------------------------------------------------------- routes: GET

fn q<'a>(query: &'a Query, key: &str) -> &'a str {
    query.get(key).map(String::as_str).unwrap_or("")
}

fn get_state(state: &State, _q: &Query) -> Out {
    Ok(payload(state))
}

/// The last `n` lines of a file, or the io error when it cannot be read, so a blank tail says why.
fn tail(path: &Path, n: usize) -> String {
    // ponytail: read the last 64 KB, not the whole log — the first line of the window may be a
    // fragment, which is fine for a tail. Seek to a line boundary if that ever matters.
    let read = || -> std::io::Result<String> {
        let mut f = std::fs::File::open(path)?;
        let len = f.seek(SeekFrom::End(0))?;
        f.seek(SeekFrom::Start(len.saturating_sub(64 * 1024)))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    };
    match read() {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(n)..].join("\n")
        }
        Err(e) => format!("({}: {e})", path.display()),
    }
}

/// The diagnostic bundle the debugger screen shows: build, paths, live poll state, and the debug log tail.
fn get_debug(state: &State, _q: &Query) -> Out {
    let cfg = config::get();
    let (
        fetched_at,
        fetching,
        error,
        auto,
        pending,
        running,
        sections,
        details,
        detailing,
        diffs,
        diffing,
        seen,
        known,
        sweeping,
        asks,
        notices,
    ) = {
        let inner = state.lock();
        (
            inner.fetched_at,
            inner.fetching,
            inner.error.clone(),
            inner.auto,
            inner.pending.clone(),
            inner.running.iter().cloned().collect::<Vec<_>>(),
            inner
                .sections
                .iter()
                .map(|s| {
                    json!({"name": s.name, "count": s.prs.as_ref().map(Vec::len).unwrap_or(0), "error": s.err})
                })
                .collect::<Vec<_>>(),
            inner.details.len(),
            inner.detailing.len(),
            inner.diffs.len(),
            inner.diffing.len(),
            inner.seen_at.len(),
            inner.known.as_ref().map(|k| k.len()),
            inner.sweeping,
            inner.asks.len(),
            inner.notices.clone(),
        )
    };
    Ok(json!({
        "at": now(),
        "version": config::VERSION,
        "pid": std::process::id(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "debug": cfg.debug,
        "demo": cfg.demo,
        "model": cfg.model,
        "interval": cfg.interval,
        "paths": {
            "settings": cfg.settings.as_deref().map(config::tilde).unwrap_or_default(),
            "memory": config::tilde(&cfg.memory_dir),
            "log": config::tilde(&cfg.log),
            "debugLog": config::tilde(&cfg.debug_log),
            "selfReviews": config::tilde(&cfg.self_dir),
            "reports": config::tilde(&cfg.reports),
            "backups": config::tilde(&cfg.backups),
            "bindings": config::tilde(&cfg.bindings),
            "autoReview": config::tilde(&cfg.autorev),
            "teams": config::tilde(&cfg.teams),
            "registry": config::tilde(&cfg.registry),
            "corpus": config::tilde(&cfg.corpus_home),
        },
        "state": {
            "fetchedAt": fetched_at,
            "fetching": fetching,
            "error": error,
            "auto": auto,
            "pending": pending,
            "running": running,
            "sections": sections,
            "caches": {"details": details, "detailing": detailing, "diffs": diffs, "diffing": diffing, "seen": seen, "known": known},
            "sweeping": sweeping,
            "asks": asks,
            "notices": notices,
        },
        "log": tail(&cfg.debug_log, 200),
    }))
}

fn get_pr(state: &State, query: &Query) -> Out {
    let (pr, section) = need_pr(state, q(query, "url"))?;
    Ok(detail(state, &pr, &section))
}

/// How much code around a mark the viewer asked for: one of the ring's own values, or the default.
///
/// ponytail: checked against CONTEXTS. `diff::narrow` adds this to a line index, so a
/// usize off the query string overflows that sum -- a panic in a debug build, and in release a wrap that
/// quietly picks the wrong lines. Every other value this server takes is checked against the list it came
/// from; this one was parsed and trusted because the page only ever sends the ring back.
fn context_of(query: &Query) -> usize {
    q(query, "context")
        .parse()
        .ok()
        .filter(|n| diff::CONTEXTS.contains(n))
        .unwrap_or(diff::CONTEXTS[0])
}

fn get_diff(state: &State, query: &Query) -> Out {
    let context = context_of(query);
    let (pr, _) = need_pr(state, q(query, "url"))?;
    let scope = if q(query, "scope").is_empty() {
        "marks"
    } else {
        q(query, "scope")
    };
    Ok(code(state, &pr, scope, context))
}

fn no_prereview(pr: &Pr) -> Fail {
    Fail(404, format!("no pre-review of #{} yet — p runs one", pr.number))
}

fn get_prereview(state: &State, query: &Query) -> Out {
    let (pr, _) = need_pr(state, q(query, "url"))?;
    let (at, moved) = review::self_review_state(&pr);
    if at == 0.0 {
        return Err(no_prereview(&pr));
    }
    let path = review::self_review_path(pr.repo(), pr.number);
    let text = std::fs::read_to_string(&path)?;
    // null when the pre-review was written before its conversation was saved: the screen says why
    let talk = review::self_talk(pr.repo(), pr.number).map(|h| {
        let mut t = talk_json(state, &h);
        t["verdict"] = json!(h.verdict.verdict);
        t
    });
    Ok(json!({"path": path, "text": text, "moved": moved, "talk": talk}))
}

/// Post a spell's result on its PR as a plain comment: no verdict, just what it found.
fn post_spell(state: &State, body: &Body) -> Out {
    let (pr, _) = need_pr(state, &text(body, "url"))?;
    let name = text(body, "name");
    let Some((_, found, _)) = review::spell_results(pr.repo(), pr.number)
        .into_iter()
        .find(|(n, _, _)| *n == name)
    else {
        return Err(Fail(404, format!("no {name} result on this PR")));
    };
    if !config::get().demo {
        github::comment(pr.repo(), pr.number, &format!("**{name}**\n\n{found}"))
            .map_err(|e| Fail(502, format!("{e:#}")))?;
    }
    Ok(json!({"ok": true}))
}

/// Talk about your own PR's pre-review, the way a held review is discussed. Nothing here is posted.
fn post_prereview(state: &State, body: &Body) -> Out {
    let (pr, _) = need_pr(state, &text(body, "url"))?;
    let op = text(body, "op");
    if !TALK_OPS.contains(&op.as_str()) {
        return Err(Fail::new(400, "op must be discuss, revise, accept or keep"));
    }
    let Some(h) = review::self_talk(pr.repo(), pr.number) else {
        return Err(Fail::new(
            409,
            "this pre-review was written before discussions were saved; run it again to discuss it",
        ));
    };
    talk(state, review::Talk::Pre, h, &op, body)
}

/// Where a memory edit reads and writes: your memory dir, or a joined team's memory/ when `team` is named.
/// (the directory memory::path joins onto, the checkout to pull and push, the push label). 404 for a team
/// this machine is not in, so a typed key never becomes a new directory.
fn memory_home(team: &str) -> Result<(Option<std::path::PathBuf>, std::path::PathBuf, &'static str), Fail> {
    if team.is_empty() {
        return Ok((None, config::get().memory_dir, "mine"));
    }
    let d = team::dir_of(team).ok_or_else(|| Fail(404, format!("not in team {team:?}")))?;
    Ok((Some(d.join("memory")), d, "sync"))
}

/// Every memory file there is, yours and each team's, and each team's founding documents: what inspect lists.
///
/// A team's abouts are listed for every repo it could describe: those it already has an about for, those it
/// holds facts about, and the repos on the board that are bound to it, so an about can be started for one.
fn get_memory_files(state: &State, _query: &Query) -> Out {
    let mut files: Vec<Value> = memory::editable()
        .into_iter()
        .map(|(team, repo)| json!({"team": team, "repo": repo.unwrap_or_default()}))
        .collect();
    // ponytail: GitHub's own spelling, not bind::key's. That key is lowercased for lookups and is not a file
    // name: an about started from it was written as martinrovang__git-dashy.md and read as
    // MartinRovang__git-dashy.md, so on Linux no review ever saw it.
    let board: Vec<String> = state
        .lock()
        .sections
        .iter()
        .flat_map(|s| s.prs.iter().flatten())
        .map(|p| p.repo().to_string())
        .collect();
    let of = bind::resolver(); // one read of the bindings for every repo below
    for team in team::joined() {
        let Some(dir) = team::dir_of(&team) else { continue };
        for doc in ["brief", "agents"] {
            files.push(json!({"team": team, "doc": doc}));
        }
        let base = dir.join("memory");
        let mut repos: Vec<String> = std::fs::read_dir(base.join(memory::ABOUT))
            .into_iter()
            .flatten()
            .flatten()
            .chain(std::fs::read_dir(&base).into_iter().flatten().flatten())
            .filter_map(|e| e.file_name().to_str().and_then(memory::repo_of))
            .chain(board.iter().cloned())
            // an about is read only through the repo's binding, so one for a repo bound elsewhere is never read
            .filter(|r| r.contains('/') && of(r).eq_ignore_ascii_case(&team))
            .collect();
        repos.sort_by_key(|r| r.to_lowercase());
        repos.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        for repo in repos {
            files.push(json!({"team": team, "doc": "about", "repo": repo}));
        }
    }
    Ok(json!({ "files": files }))
}

/// The file a founding document lives in, in a joined team's checkout: the brief, agents.md, or one repo's
/// about. 400 for a name that is not one, or an about with no repo; 404 for a team this machine is not in.
fn doc_path(team: &str, doc: &str, repo: &str) -> Result<std::path::PathBuf, Fail> {
    let d = team::dir_of(team).ok_or_else(|| Fail(404, format!("not in team {team:?}")))?;
    let m = d.join("memory");
    match doc {
        "brief" => Ok(m.join(memory::PROJECT)),
        "agents" => Ok(m.join(memory::AGENTS)),
        "about" => {
            safe_repo(repo)?;
            if !repo.contains('/') {
                return Err(Fail::new(400, "an about is for one repo, owner/name"));
            }
            // the one rule for an about's file, reading and writing: memory::about_path
            memory::about_path(team, repo).ok_or_else(|| Fail(404, format!("not in team {team:?}")))
        }
        _ => Err(Fail::new(400, "doc must be brief, agents or about")),
    }
}

/// One memory file to inspect: its facts, or a founding document's text. Read only; nothing here writes.
fn get_memory(_state: &State, query: &Query) -> Out {
    let (team, doc) = (q(query, "team"), q(query, "doc"));
    if !doc.is_empty() {
        let path = doc_path(team, doc, q(query, "repo"))?;
        if doc == "brief" {
            team::seed_project(&path);
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        // an about nobody has written yet opens on its template; nothing is written until a proposal is approved
        let draft = if text.trim().is_empty() && doc == "about" {
            team::ABOUT_TEMPLATE
        } else {
            ""
        };
        return Ok(
            json!({"team": team, "doc": doc, "repo": q(query, "repo"), "path": knowledge::tilde(&path),
                         "text": text, "draft": draft, "guide": team::guide(doc)}),
        );
    }
    let repo = Some(q(query, "repo")).filter(|r| !r.is_empty());
    let (base, dir, label) = memory_home(team)?;
    let path = memory::path(repo, base.as_deref());
    team::pull_dir(&dir, label);
    Ok(json!({
        "repo": repo.unwrap_or("general"),
        "team": team,
        "path": knowledge::tilde(&path),
        "facts": memory::facts_in(&path),
        // who stands behind each of a team's facts, from the evidence lines: "2 people found this"
        "backers": if team.is_empty() {
            Vec::new()
        } else {
            let index = memory::pools_in(&team::dir_of(team).into_iter().collect::<Vec<_>>());
            memory::facts_in(&path).iter().map(|f| memory::backers(&index, repo, f)).collect::<Vec<_>>()
        },
    }))
}

/// Offer a change to a file in a team's checkout as a pull request, `edit` applied to origin's copy of it.
/// {url, branch, note}.
fn propose_in_team(
    file: &std::path::Path,
    edit: &dyn Fn(&str) -> Result<String, String>,
    title: &str,
) -> Out {
    // the checkout is the nearest directory above the file that is a git repo: memory/<file> and
    // memory/about/<file> sit at different depths, and counting parents took memory/ for the checkout
    let checkout = file
        .ancestors()
        .skip(1)
        .find(|d| team::is_repo(d))
        .ok_or_else(|| Fail::new(400, "not a file in a team's checkout"))?;
    let rel = file
        .strip_prefix(checkout)
        .map_err(|_| Fail::new(400, "not a file in a team's checkout"))?;
    let p = team::propose(checkout, &rel.to_string_lossy(), edit, title).map_err(|e| Fail(409, e))?;
    Ok(json!({"ok": true, "url": p.url, "branch": p.branch, "note": p.note}))
}

/// The edit a removal from a team's file proposes: one line out of origin's copy, or why not.
fn remove_from_origin(fact: &str) -> impl Fn(&str) -> Result<String, String> + '_ {
    move |now| {
        memory::without_fact(now, fact)
            .ok_or_else(|| "that fact is not in the team's file on origin any more".to_string())
    }
}

/// A founding document's proposed text. 400 for an empty one: an empty proposal would delete the file.
fn founding_text(body: &Body) -> Result<String, Fail> {
    let t = text(body, "text");
    if t.trim().is_empty() {
        return Err(Fail::new(400, "a founding document cannot be proposed empty"));
    }
    Ok(t)
}

fn get_drafts(_state: &State, _q: &Query) -> Out {
    let mut items = memory::waiting();
    items.sort_by(|a, b| {
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
    let mut items: Vec<Value> = items
        .iter()
        .map(|(repo, n, fact, kind)| {
            json!({"repo": repo, "n": n, "fact": fact, "kind": kind,
                   "team": repo.as_deref().map(bind::of).unwrap_or_default()})
        })
        .collect();
    // yours in each team's pool, after your private ones: kind "team", and the team they are in
    for (team, repo, n, fact) in memory::team_waiting() {
        items.push(json!({"repo": repo, "n": n, "fact": fact, "kind": "team", "team": team}));
    }
    Ok(json!({"promoteAt": memory::PROMOTE_AT, "items": items}))
}

/// The book: the spells on disk, and the built-in passives and voices with a line on what each does.
fn get_spells(_state: &State, _q: &Query) -> Out {
    let c = config::get();
    let about = |n: &str| review::table(review::ABOUT, n).unwrap_or("");
    let built = |names: &[&str], on: &[String]| -> Vec<Value> {
        names
            .iter()
            .map(|n| json!({"name": n, "about": about(n), "on": on.iter().any(|x| x == n)}))
            .collect()
    };
    let spells: Vec<Value> = spells::list()
        .into_iter()
        .map(|(name, text)| {
            let on = c.spells.contains(&name);
            json!({"name": name, "about": spells::about(&text), "on": on})
        })
        .collect();
    Ok(json!({
        "spells": spells,
        "passives": built(config::HUNTERS, &c.hunter),
        "voices": built(config::VOICES, &c.voice),
    }))
}

/// One overlap pair, re-read live: None once either side is no longer a draft.
///
/// ponytail: memory::rows, not memory::drafts. drafts() drops the run ids, and would_merge reads the ids
/// to tell two independent reviews from one review worded twice: with them empty it can only ever answer
/// the max and "origin unknown". That is the panel promising `promotes: false` on the very pair that
/// merge() then promotes, because merge reads rows. The panel and the keypress read the same thing now.
fn pair(repo: Option<&str>, a: &str, b: &str) -> Option<Value> {
    let live = memory::rows(repo);
    let find = |fact: &str| live.iter().find(|d| d.fact == fact).cloned();
    let (a, b) = (find(a)?, find(b)?);
    let (would, says) = memory::would_merge(&a, &b);
    Some(
        json!({"repo": repo, "a": a.fact, "b": b.fact, "would": would, "says": says, "promotes": would >= memory::PROMOTE_AT}),
    )
}

/// The help for a founding document being written: running, or its questions, notes and revised text.
fn get_doc_help(_state: &State, _q: &Query) -> Out {
    Ok(job("doc-help"))
}

/// Ask the model to help with a founding document: {team, doc, repo, text}. It runs in the background; GET
/// reports on it. Nothing it returns is written anywhere.
fn post_doc_help(_state: &State, body: &Body) -> Out {
    let (team, doc) = (text(body, "team"), text(body, "doc"));
    let repo = checked_repo(body)?.unwrap_or_default();
    doc_path(&team, &doc, &repo)?; // the same names, and the same refusals, as reading and proposing one
    let draft = text(body, "text");
    let brief = if doc == "about" {
        std::fs::read_to_string(doc_path(&team, "brief", "")?).unwrap_or_default()
    } else {
        String::new()
    };
    let model = config::get().model;
    // ponytail: one at a time, and a second is refused rather than silently dropped. Answering ok to a start
    // that did not happen let a poll for document B show document A's revision, and F3 put it into B's text.
    // start_job says whether it started, under its own lock: a check here first was a race between two posts.
    let started = start_job("doc-help", move || {
        Ok(serde_json::to_value(founding::help(
            &doc, &repo, &draft, &brief, &model,
        )?)?)
    });
    if !started {
        return Err(Fail::new(409, "the model is still reading another draft"));
    }
    Ok(json!({"ok": true}))
}

/// The scan's state. The model reads the candidates on a thread; pairs are re-read live on each poll.
fn get_overlaps(_state: &State, _q: &Query) -> Out {
    let mut j = job("overlaps");
    let running = j["running"].as_bool().unwrap_or(false);
    let idle = j["idle"].as_bool().unwrap_or(false);
    if running || idle || !j["error"].as_str().unwrap_or("").is_empty() {
        return Ok(j);
    }
    let pairs: Vec<Value> = j["result"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let repo = r[0].as_str();
                    pair(repo, r[2].as_str().unwrap_or(""), r[3].as_str().unwrap_or(""))
                })
                .collect()
        })
        .unwrap_or_default();
    j["result"] = Value::Array(pairs);
    Ok(j)
}

fn used_for(key: &str) -> String {
    let key = key.to_lowercase();
    let mut owners: Vec<String> = bind::owners()
        .iter()
        .filter(|(_, t)| t.to_lowercase() == key)
        .map(|(o, _)| format!("{o}/*"))
        .collect();
    owners.sort();
    let repos = bind::bindings()
        .values()
        .filter(|t| t.to_lowercase() == key)
        .count();
    let mut head = owners.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    if owners.len() > 3 {
        head += &format!(" +{}", owners.len() - 3);
    }
    let tail = if repos > 0 {
        format!("{repos} repo{}", if repos == 1 { "" } else { "s" })
    } else {
        String::new()
    };
    [head, tail]
        .into_iter()
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

fn brief_path(key: &str) -> Result<std::path::PathBuf, Fail> {
    let d = team::dir_of(key).ok_or_else(|| Fail(404, format!("not in team {key:?}")))?;
    let path = d.join("memory").join(memory::PROJECT);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(path)
}

/// The teams joined, one dict each. Opening the list clears the arrival badge, as T did.
fn get_teams(state: &State, query: &Query) -> Out {
    let arrived = state.take_arrivals();
    let key = q(query, "brief");
    if !key.is_empty() {
        let path = brief_path(key)?;
        team::seed_project(&path);
        let text = std::fs::read_to_string(&path)?;
        return Ok(json!({"key": key, "path": knowledge::tilde(&path), "text": text}));
    }
    let mut out = Vec::new();
    for key in team::joined() {
        let d = team::dir_of(&key).unwrap_or_default();
        let it = team::info(&key);
        let url = team::origin_url(&d);
        let real = std::fs::canonicalize(&d).unwrap_or_else(|_| d.clone());
        let linked = std::fs::symlink_metadata(&d)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        out.push(json!({"key": key, "name": it.name, "description": it.description,
                        "checkout": knowledge::tilde(&real), "linked": linked,
                        "remote": if url.is_empty() { String::new() } else { team::redacted(&url) },
                        "used": used_for(&key), "arrived": arrived.get(&key).copied().unwrap_or(0),
                        "undecided": bind::undecided(&team::covers(&key))}));
    }
    Ok(json!({"teams": out, "error": team_error()}))
}

fn owner_of(repo: &str) -> String {
    bind::key(repo).split('/').next().unwrap_or("").to_string()
}

fn get_bind(_state: &State, query: &Query) -> Out {
    let repo = q(query, "repo");
    if repo.is_empty() {
        return Err(Fail::new(400, "no row selected"));
    }
    let (kind, to) = bind::why(repo);
    let teams: Vec<Value> = team::joined()
        .iter()
        .map(|k| json!({"key": k, "name": team::info(k).name}))
        .collect();
    Ok(json!({"repo": repo, "kind": kind, "to": to, "owner": owner_of(repo), "teams": teams}))
}

/// What happens to one repo's reviews: the word in force, whose rule it is, and the owner's own
/// word, which is what a "set the whole owner" control writes.
///
/// ponytail: the owner's OWN word too, not only the effective one. `o` flips the owner rule, and
/// flipping it from the effective value wrote back what was already there whenever a repo row had
/// carved the owner out -- a no-op the flash reported as a change.
///
/// ponytail: the one resolver. Both routes that answer "what happens to this repo" come through here;
/// the copy `get_posting` used to keep had already drifted, returning the raw repo where this returns
/// the folded key.
fn posting_json(repo: &str) -> Value {
    let p = autorev::posting();
    let owner = owner_of(repo);
    let key = bind::key(repo);
    let one = |rules: &autorev::Rules| {
        let via = if rules.repos.contains_key(&key) {
            "repo"
        } else if rules.owners.contains_key(&owner) {
            "owner"
        } else {
            ""
        };
        json!({
            "value": rules.of(repo).word(),
            "via": via,
            "ownerValue": rules.owners.get(&owner).copied().unwrap_or_default().word(),
        })
    };
    json!({"repo": key, "owner": owner, "manual": one(&p.manual), "auto": one(&p.auto)})
}

/// Every repo the board is holding, folded and deduped: what the posting panel lists.
fn board_repos(sections: &[crate::types::Section]) -> Vec<String> {
    let mut v: Vec<String> = sections
        .iter()
        .flat_map(|s| s.prs.iter().flatten())
        .map(|p| bind::key(p.repo()))
        .filter(|k| !k.is_empty())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Every rule that exists, so nothing about this is invisible: (target, manual, auto), owners first.
///
/// Each word is the EFFECTIVE one, resolved the way the controls above the table resolve it, and `via`
/// says where it came from. ponytail: it used to print an unset axis as the default, so with `a/*` holding
/// what you run and `a/b` carrying an auto-only rule, the `a/b` row read "you post" while a manual review
/// on a/b would in fact hold -- the table contradicting the controls, on the one screen built to make the
/// rules visible.
fn posting_rules_json(on_board: &[String]) -> Vec<Value> {
    let p = autorev::posting();
    let il = autorev::inlines();
    // what a target with no rule of its own falls back to
    let switch = config::get().inline;
    // ponytail: every repo the program is handling, not only the ones a rule names. The panel is a list
    // you walk, so a repo with no rule of its own has to be in it -- that is the one you came to set.
    // Each repo's owner comes with it, since the owner row is what a repo with no rule falls back to.
    // ponytail: the inline rules are listed too. Built from the posting rules and the board alone, a
    // repo whose ONLY rule is an inline one — and no open PR — was in force and nowhere on screen.
    let mut targets: Vec<String> = p
        .manual
        .listed()
        .into_iter()
        .chain(p.auto.listed())
        .map(|(t, _)| t)
        // ponytail: each one's OWNER with it, the same as the board chain below. A repo listed here
        // and nowhere else arrived without a parent, so the panel drew a stand-in owner row carrying
        // defaults instead of whatever that owner had actually been set to.
        .chain(il.listed().into_iter().flat_map(|(t, _)| {
            let owner = t.split('/').next().unwrap_or("");
            let owner = (!owner.is_empty() && !t.ends_with("/*")).then(|| format!("{owner}/*"));
            owner.into_iter().chain(Some(t))
        }))
        .chain(on_board.iter().flat_map(|r| {
            let k = bind::key(r);
            let owner = k.split('/').next().unwrap_or("");
            let owner = (!owner.is_empty()).then(|| format!("{owner}/*"));
            owner.into_iter().chain(Some(k).filter(|k| !k.is_empty()))
        }))
        .collect();
    targets.sort();
    targets.dedup();
    // owners first, the way Rules::listed orders one set
    targets.sort_by_key(|t| !t.ends_with("/*"));
    targets
        .into_iter()
        .map(|t| {
            let bare = t.strip_suffix("/*").unwrap_or(&t).to_string();
            // an owner row answers for itself; a repo row inherits its owner's word when it sets none
            let one = |rules: &autorev::Rules| {
                if t.ends_with("/*") {
                    let v = rules.owners.get(&bare).copied().unwrap_or_default();
                    (
                        v,
                        if rules.owners.contains_key(&bare) {
                            "owner"
                        } else {
                            ""
                        },
                    )
                } else if let Some(v) = rules.repos.get(&bare) {
                    (*v, "repo")
                } else {
                    let owner = bare.split('/').next().unwrap_or("").to_string();
                    (
                        rules.of(&bare),
                        if rules.owners.contains_key(&owner) {
                            "owner"
                        } else {
                            ""
                        },
                    )
                }
            };
            let (m, mv) = one(&p.manual);
            let (a, av) = one(&p.auto);
            // ponytail: the store's own resolver, not a second copy of the rule here. "" means the
            // switch decided rather than a constant, which is the one way this axis differs.
            let (inline, inline_via) = if t.ends_with("/*") {
                il.owner_via(&bare, switch)
            } else {
                il.via(&bare, switch)
            };
            let mut row = json!({"target": t, "manual": m.word(), "auto": a.word(),
                                 "manualVia": mv, "autoVia": av,
                                 "inline": inline, "inlineVia": inline_via});
            // an owner switched to per repo still has its rule, as the fallback: the page must not read that
            // rule as "this owner decides"
            if t.ends_with("/*") {
                row["perRepo"] = json!(p.per_repo.contains(&bare));
            }
            row
        })
        .collect()
}

/// What happens to this repo's reviews, and where each answer came from.
///
/// ponytail: `via` as well as the value. A review that stops posting with nothing on screen saying
/// which rule decided it looks like a bug, and the rule may be an owner-wide one set months ago.
fn get_posting(state: &State, query: &Query) -> Out {
    let repo = q(query, "repo");
    if repo.is_empty() {
        return Err(Fail::new(400, "no row selected"));
    }
    let n: u64 = q(query, "number").parse().unwrap_or(0);
    let h = held::get(repo, n);
    // ponytail: the head the verdict was written against, next to the one on the board now. Pressing
    // `p` a week later posts the old verdict against new commits, and nothing on the screen said so.
    let live = state
        .lock()
        .sections
        .iter()
        .flat_map(|s| s.prs.iter().flatten())
        .find(|p| p.repo() == repo && p.number == n)
        .map(|p| p.head.clone())
        .unwrap_or_default();
    let mut out = posting_json(repo);
    out["held"] = json!(h.map(|h| json!({
        "verdict": h.verdict.verdict,
        "summary": h.verdict.summary,
        "body": h.verdict.body,
        "model": h.model,
        "at": h.at,
        // neither side knowing its head is not a move; only two we can compare and that differ
        "moved": !h.pr.head.is_empty() && !live.is_empty() && h.pr.head != live,
        "talk": talk_json(state, &h),
    })));
    Ok(out)
}

/// Every learning event on this machine, for the Knowledge chart. The page buckets and filters them.
///
/// ponytail: the events, not the counts. The chart's filters (kind, team, repo, person, day or week) all apply
/// to the same few hundred rows, so the page recomputes on each change instead of asking again.
fn get_learning(_state: &State, _q: &Query) -> Out {
    Ok(json!({"events": crate::learning::events()}))
}

fn get_dream(_state: &State, _q: &Query) -> Out {
    let mut j = job("dream");
    if let Some(result) = j["result"].as_object_mut() {
        result.remove("new"); // the page never sees the file bodies; apply uses what the server held
    }
    Ok(j)
}

fn get_collaborators(state: &State, query: &Query) -> Out {
    let (pr, _) = need_pr(state, q(query, "url"))?;
    let me = pr.author().to_string();
    let logins: Vec<String> = github::collaborators(pr.repo())
        .into_iter()
        .filter(|c| *c != me)
        .collect();
    Ok(json!({"logins": logins}))
}

/// ponytail: blocks this request's thread for the model call; every request has its own thread.
/// Replace the followed list. A body without one is a mistake, not "unfollow everyone".
fn post_stories(_state: &State, body: &Body) -> Out {
    let Some(list) = body.get("follow").filter(|v| v.is_array()) else {
        return Err(Fail::new(400, "follow must be a list"));
    };
    Ok(json!({"follow": story::set_followed(list)}))
}

/// The card has been read: drop the shift mark on that story. Unknown logins are a no-op, not an error --
/// the card may have been closed and the story pruned by the time this lands.
fn post_story_seen(_state: &State, body: &Body) -> Out {
    let login = body.get("login").and_then(Value::as_str).unwrap_or_default();
    if !story::login_ok(login) {
        return Err(Fail::new(400, "login must be a GitHub username"));
    }
    // the text of the shift the pill showed: only that one is cleared, see story::forget
    let read = body.get("shift").and_then(Value::as_str).unwrap_or_default();
    story::seen(login, read);
    Ok(json!({"ok": true}))
}

fn get_story(_state: &State, query: &Query) -> Out {
    let login = q(query, "login");
    if !story::login_ok(login) {
        return Err(Fail::new(400, "login must be a GitHub username"));
    }
    Ok(story::get(login, !q(query, "fresh").is_empty())?)
}

/// A DB repo's whole schema, parsed from its .sql files. Only a repo some rule names: this clones it.
fn get_dbschema(_state: &State, query: &Query) -> Out {
    let db = bind::key(q(query, "db"));
    if db.is_empty() || !dbrepo::rules().listed().iter().any(|(_, d)| *d == db) {
        return Err(Fail::new(400, "db must be a DB repo a rule names"));
    }
    dbschema::get(&db).map_err(|e| Fail::new(502, e))
}

// ---------------------------------------------------------------- routes: POST

type Body = Map<String, Value>;

/// `str(body.get(key, ""))`: a string as is, null or missing as "", anything else as its JSON.
fn text(body: &Body, key: &str) -> String {
    match body.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    }
}

/// `bool(body.get(key))`.
fn truthy(body: &Body, key: &str) -> bool {
    match body.get(key) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `body.get("repo") or None`.
/// The body's repo when it can only name a memory file of facts. 400 for a path trick (`\\`, `..`) or a name
/// that is a founding document's file: those go through their own route, which refuses an empty proposal.
fn checked_repo(body: &Body) -> Result<Option<String>, Fail> {
    let repo = repo_of(body);
    if let Some(r) = repo.as_deref() {
        safe_repo(r)?;
    }
    Ok(repo)
}

/// 400 for a repo name that could name something other than one repo's file.
fn safe_repo(r: &str) -> Result<(), Fail> {
    // ponytail: case folded. PROJECT.md is the brief on a case-insensitive filesystem (macOS, Windows)
    let slug = memory::slug(r);
    let names = |f: &str| slug.eq_ignore_ascii_case(f);
    if r.contains('\\') || r.contains("..") || names(memory::PROJECT) || names(memory::AGENTS) {
        return Err(Fail::new(400, "not a repo"));
    }
    Ok(())
}

fn repo_of(body: &Body) -> Option<String> {
    Some(text(body, "repo")).filter(|r| !r.is_empty())
}

/// The longest instructions or discussion message taken, in characters. A few paragraphs is the use;
/// anything past this is a paste gone wrong, and it would ride along in every turn of the session.
const ASK_MAX: usize = 8000;

/// What can be done to a saved review from its screen: talk about it, ask for a revision, take it or not.
const TALK_OPS: [&str; 4] = ["discuss", "revise", "accept", "keep"];

/// One of TALK_OPS on a held review or a pre-review. Both routes come here, so the two cannot drift.
fn talk(state: &State, on: review::Talk, h: held::Held, op: &str, body: &Body) -> Out {
    // nothing about a saved review changes while the agent is still answering about it
    if state.busy(&h.pr.url) {
        return Err(Fail::new(409, "the agent is still working on this review"));
    }
    match op {
        "discuss" | "revise" => {
            let why = review::cannot_discuss(&h);
            if !why.is_empty() {
                return Err(Fail::new(409, &why));
            }
            let message = if op == "discuss" {
                let m = text(body, "text");
                if m.trim().is_empty() {
                    return Err(Fail::new(400, "say something"));
                }
                if m.chars().count() > ASK_MAX {
                    return Err(Fail::new(400, "that message is too long"));
                }
                Some(m)
            } else {
                None
            };
            if !state.start_talk(on, h, message) {
                return Err(Fail::new(409, "the agent is still working on this review"));
            }
            Ok(json!({"ok": true}))
        }
        // ponytail: accept is the only thing that puts a revision where the review is read from, and it
        // needs a revision to be waiting; keep throws the revision away and leaves the review alone. Both under
        // the row's claim, on the file as it is then: a turn landing between the read and the write would
        // otherwise be written over by this copy.
        _ => {
            let url = h.pr.url.clone();
            if !state.claim(&url, "saving...") {
                return Err(Fail::new(409, "the agent is still working on this review"));
            }
            let Some(mut h) = on.get(h.pr.repo(), h.pr.number) else {
                state.release(&url, String::new());
                state.forget_review(&url);
                return Err(Fail::new(404, "nothing waiting for that PR"));
            };
            let before = on.status(&h.verdict);
            let done = if op == "accept" {
                match h.proposed.take() {
                    None => Err("no revision is waiting".to_string()),
                    Some(v) => {
                        h.verdict = v;
                        on.accept(&h).map_err(|e| e.to_string())
                    }
                }
            } else {
                h.proposed = None;
                on.put(&h).map_err(|e| e.to_string())
            };
            match done {
                Ok(()) => {
                    state.release(&url, on.status(&h.verdict));
                    Ok(json!({"ok": true}))
                }
                Err(e) => {
                    state.release(&url, before);
                    Err(Fail::new(409, &e))
                }
            }
        }
    }
}

/// A conversation, as the screen reads it: the same shape for a held review and a pre-review.
fn talk_json(state: &State, h: &held::Held) -> Value {
    json!({
        "instructions": h.verdict.instructions,
        "thread": h.thread,
        "proposed": h.proposed.as_ref().map(|v| json!({
            "verdict": v.verdict, "summary": v.summary, "body": v.body,
        })),
        // "" when it can be discussed; otherwise the sentence the screen shows instead of a box
        "cannotDiscuss": review::cannot_discuss(h),
        "busy": state.busy(&h.pr.url),
    })
}

fn post_review(state: &State, body: &Body) -> Out {
    let (pr, _) = need_pr(state, &text(body, "url"))?;
    // pre-review reads the diff and posts nothing; review posts the verdict. Same row, same spinner.
    // ponytail: the start IS the check. It claims the url under the state lock and says whether it got
    // it, so a double-click cannot start two reviews of one PR between the check and the spawn.
    if team::in_repos(&team::team_repos(), pr.repo()) {
        return Err(Fail(403, team::HUMAN_ONLY.into()));
    }
    let spell = text(body, "spell");
    let ask = text(body, "ask");
    if !spell.is_empty() {
        if !ask.is_empty() || truthy(body, "self") {
            return Err(Fail::new(400, "a spell is cast on its own"));
        }
        let Some(t) = spells::get(&spell) else {
            return Err(Fail(400, format!("no spell {spell}")));
        };
        if t.chars().count() > ASK_MAX {
            return Err(Fail(400, format!("spell {spell} is too long: ~/.prs_spells/{spell}.md must stay under {ASK_MAX} characters")));
        }
        if !state.start_spell(&pr, &spell, &t) {
            return Err(Fail::new(409, "already running"));
        }
        return Ok(json!({"ok": true}));
    }
    if ask.chars().count() > ASK_MAX {
        return Err(Fail::new(400, "instructions are too long"));
    }
    let started = if truthy(body, "self") {
        state.start_self_review(&pr)
    } else {
        state.start_review(&pr, autorev::Ran::Manual, &ask)
    };
    if !started {
        return Err(Fail::new(409, "already running"));
    }
    Ok(json!({"ok": true}))
}

fn post_auto(state: &State, body: &Body) -> Out {
    // arming is a scope change, not the master switch. `owner` carries the owner and `repo` the
    // repo: a truthy FLAG that re-read `repo` through owner_of() meant a client sending the owner
    // name in `owner` — the obvious reading — widened a repo arm to the whole org by coincidence.
    let (repo, owner) = (text(body, "repo"), text(body, "owner"));
    if !repo.is_empty() || !owner.is_empty() {
        // ponytail: explicit. `truthy` reads a missing key as false, so {"repo": "acme/api"} with no
        // `on` DISARMED it — a caller that forgot the field got the opposite of what it asked for,
        // silently. #86's toggle is about to be written against this route.
        let Some(on) = body.get("on").and_then(|v| v.as_bool()) else {
            return Err(Fail::new(400, "a scope change needs on: true or on: false"));
        };
        // ponytail: the same answer `auto_cmd` gives. It silently preferred `owner` and dropped
        // `repo`, so the two surfaces resolved one body two ways, and #86's toggle is written
        // against this one.
        if !repo.is_empty() && !owner.is_empty() {
            return Err(Fail::new(400, "name a repo or an owner, not both"));
        }
        fail_if(if owner.is_empty() {
            autorev::set(&repo, on)
        } else {
            autorev::set_owner(&owner, on)
        })?;
        // ponytail: the no-batch rule is tick()'s, not this route's. `gitdashy auto` writes the same
        // store from another process, so suppressing it here left the CLI path firing the batch.
        state.wake();
        return Ok(json!({"ok": true}));
    }
    // ponytail: include_existing only when the page says so: it asks first, with the count, as `a` did.
    state.set_auto(truthy(body, "on"), truthy(body, "includeExisting"));
    Ok(json!({"ok": true}))
}

fn post_refresh(state: &State, _body: &Body) -> Out {
    diff::retry(); // f means "look again", so a diff GitHub failed to read is worth retrying
    state.retry_reads(); // and the pane's own caches, where a failed detail read sat for good
    Ok(json!({"ok": true, "answeredBy": state.wake_answered_by()}))
}

/// Open a PR, or its pre-review file, with the desktop. Only things on the board, never a free path.
fn post_open(state: &State, body: &Body) -> Out {
    let (pr, _) = need_pr(state, &text(body, "url"))?;
    github::open_in_browser(&pr.url);
    Ok(json!({"ok": true, "opened": pr.url}))
}

fn post_copy(state: &State, body: &Body) -> Out {
    let what = match text(body, "text") {
        t if !t.is_empty() => t,
        _ => need_pr(state, &text(body, "url"))?.0.url,
    };
    Ok(json!({"ok": true, "tool": github::copy(&what)}))
}

/// Change a memory file the only ways there are: take a fact out of yours, or propose a change to a team's.
///
/// ponytail: no op writes text. Facts arrive through reviews and the two-sightings gate, never typed in; a
/// person only removes them. What people wrote, a team's brief and agents.md, can be changed, and only as a
/// pull request on the team's repo, because every teammate reads it.
fn post_memory(_state: &State, body: &Body) -> Out {
    let (team, fact) = (text(body, "team"), text(body, "fact"));
    let repo = checked_repo(body)?.filter(|r| r != "general");
    let label = repo.as_deref().unwrap_or("general").to_string();
    match text(body, "op").as_str() {
        "remove" if team.is_empty() => {
            let dir = config::get().memory_dir;
            team::pull_dir(&dir, "mine");
            if !memory::remove_mine(repo.as_deref(), &fact) {
                return Err(Fail::new(404, "that fact is not in this file any more"));
            }
            let err = team::push_dir(&dir, &format!("memory: removed a fact from {label}"), "mine");
            Ok(json!({"ok": true, "error": err}))
        }
        "remove" => {
            let (base, dir, label_) = memory_home(&team)?;
            team::pull_dir(&dir, label_);
            let path = memory::path(repo.as_deref(), base.as_deref());
            if !memory::facts_in(&path).iter().any(|f| f == &fact) {
                return Err(Fail::new(404, "that fact is not in this file any more"));
            }
            propose_in_team(
                &path,
                &remove_from_origin(&fact),
                &format!("memory: remove a fact from {label}"),
            )
        }
        "propose" => {
            let doc = text(body, "doc");
            let path = doc_path(&team, &doc, repo.as_deref().unwrap_or(""))?;
            let new = founding_text(body)?;
            // a soft word first: the proposal waits for `anyway` rather than being refused
            if !truthy(body, "anyway") {
                if let Some(warn) = memory::doc_warning(&doc, &new) {
                    return Ok(json!({"ok": false, "warn": warn}));
                }
            }
            let doc = if doc == "about" {
                format!("about {label}")
            } else {
                doc
            };
            propose_in_team(
                &path,
                &move |_| Ok(new.clone()),
                &format!("{doc}: proposed change for team {team}"),
            )
        }
        _ => Err(Fail::new(400, "op must be remove or propose")),
    }
}

fn post_drafts(_state: &State, body: &Body) -> Out {
    let (repo, fact) = (checked_repo(body)?, text(body, "fact"));
    let label = repo.as_deref().unwrap_or("general");
    let dir = config::get().memory_dir;
    let team = text(body, "team");
    if truthy(body, "pooled") {
        // one of your drafts in a team's pool: drop it from your folder there, or propose it to the team
        return match text(body, "op").as_str() {
            "drop" => {
                if !memory::drop_team(&team, repo.as_deref(), &fact) {
                    return Err(Fail::new(404, "that draft is not in your team drafts any more"));
                }
                team::push(&format!("memory: dropped a draft for {label}"));
                Ok(json!({"ok": true}))
            }
            // ponytail: a person accepting a draft into a TEAM's knowledge is a hand edit of what every
            // teammate's reviews read, so it is a pull request, not a push. Recurrence is the only thing
            // that moves a team draft into team knowledge by itself.
            "promote" => {
                // one line: a newline in the body would add facts nobody proposed
                if fact.trim().is_empty() || fact.contains(['\n', '\r']) {
                    return Err(Fail::new(400, "a fact is one line"));
                }
                let d = team::dir_of(&team).ok_or_else(|| Fail(404, format!("not in team {team:?}")))?;
                let file = memory::path(repo.as_deref(), Some(&d.join("memory")));
                propose_in_team(
                    &file,
                    &|now| memory::with_fact(now, &fact),
                    &format!("memory: a fact for {label}"),
                )
            }
            _ => Err(Fail::new(400, "op must be promote or drop")),
        };
    }
    match text(body, "op").as_str() {
        "promote" => {
            memory::promote(repo.as_deref(), &fact);
            team::push_dir(&dir, &format!("memory: accepted for {label}"), "mine");
            team::push(&format!("memory: evidence for {label}"));
        }
        "drop" => {
            memory::drop(repo.as_deref(), &fact);
            team::push_dir(&dir, &format!("memory: dropped a draft for {label}"), "mine");
        }
        _ => return Err(Fail::new(400, "op must be promote or drop")),
    }
    Ok(json!({"ok": true}))
}

fn post_overlaps(_state: &State, body: &Body) -> Out {
    match text(body, "op").as_str() {
        "start" => {
            let model = config::get().model;
            start_job("overlaps", move || {
                let pairs = memory::overlaps(None);
                if pairs.is_empty() {
                    return Ok(json!([]));
                }
                // ponytail: an empty list is the model saying none match, which is an answer; only "not
                // asked" falls back to every candidate, for a person to read (judged does that itself).
                Ok(serde_json::to_value(memory::judged(&pairs, &model))?)
            });
            Ok(json!({"ok": true}))
        }
        "merge" => {
            let repo = checked_repo(body)?;
            let label = repo.as_deref().unwrap_or("general").to_string();
            let n = memory::merge(repo.as_deref(), &text(body, "keep"), &text(body, "drop"));
            team::push_dir(
                &config::get().memory_dir,
                &format!("memory: folded two drafts for {label}"),
                "mine",
            );
            team::push(&format!("memory: evidence for {label}"));
            Ok(json!({"ok": true, "count": n}))
        }
        _ => Err(Fail::new(400, "op must be start or merge")),
    }
}

/// Bind owner/* here, then declare it in the team. Local first: the cheap, reversible half.
fn cover(key: &str, owner: &str) -> Result<(), Fail> {
    let owner = bind::owner_key(owner);
    if owner.is_empty() {
        return Err(Fail::new(400, "an owner is one name, like neomedsys"));
    }
    let mut err = bind::bind_owner(&owner, key);
    if err.is_empty() {
        err = team::cover(key, &owner);
    }
    if err.is_empty() {
        Ok(())
    } else {
        Err(Fail(400, err))
    }
}

fn fail_if(err: String) -> Result<(), Fail> {
    if err.is_empty() {
        Ok(())
    } else {
        Err(Fail(400, err))
    }
}

fn post_teams(state: &State, body: &Body) -> Out {
    let (op, key) = (text(body, "op"), text(body, "key"));
    if op == "new" {
        let name = text(body, "name").trim().to_string();
        if name.is_empty() {
            return Err(Fail::new(400, "a team needs a name"));
        }
        fail_if(team::start(&name, &text(body, "desc"), ""))?;
        let key = team::key_of(&name);
        let owner = text(body, "owner");
        if !owner.is_empty() {
            cover(&key, &owner)?;
        }
        state.wake();
        return Ok(json!({"ok": true, "key": key}));
    }
    if op == "join" {
        let before = team::joined();
        fail_if(team::setup(text(body, "repo").trim(), ""))?;
        state.wake();
        let mut fresh: Vec<String> = team::joined()
            .into_iter()
            .filter(|k| !before.contains(k))
            .collect();
        fresh.sort();
        let err = team_error();
        let foreign: Vec<String> = fresh
            .iter()
            .filter_map(|k| team::dir_of(k))
            .flat_map(|d| team::foreign_files(&d))
            .collect();
        let warning = if !err.is_empty() {
            format!(
                "joined, but could not publish: {}",
                err.chars().take(70).collect::<String>()
            )
        } else if !foreign.is_empty() {
            // a team's repo is its memory's alone: no pull request on it is ever reviewed by a model
            format!(
                "joined, but this repo also holds {}. Pull requests on a team's repo are never reviewed by a model; keep memory in its own repo.",
                foreign.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
            )
        } else {
            String::new()
        };
        return Ok(
            json!({"ok": true, "key": fresh.first().cloned().unwrap_or_default(), "warning": warning}),
        );
    }
    let Some(dir) = team::dir_of(&key) else {
        return Err(Fail(404, format!("not in team {key:?}")));
    };
    match op.as_str() {
        "connect" => {
            let url = text(body, "url");
            fail_if(team::connect(&key, &url))?;
            Ok(json!({"ok": true, "remote": team::redacted(&url)}))
        }
        "describe" => {
            fail_if(team::write_info(
                &key,
                &team::info(&key).name,
                &text(body, "desc"),
                None,
            ))?;
            team::push_dir(&dir, &format!("team: describe {key}"), "sync");
            Ok(json!({"ok": true}))
        }
        "cover" => {
            cover(&key, &text(body, "owner"))?;
            state.wake();
            Ok(json!({"ok": true}))
        }
        "brief" => {
            // ponytail: proposed, not pushed. The brief reaches every teammate's reviews and sessions, so a
            // change to it is a pull request a person with rights on the team's repo approves.
            let new = founding_text(body)?;
            propose_in_team(
                &brief_path(&key)?,
                &move |_| Ok(new.clone()),
                &format!("brief: proposed change for team {key}"),
            )
        }
        "claim" => {
            let (kind, v) = bind::target(&text(body, "target"));
            let err = match (truthy(body, "yes"), kind == "owner") {
                (true, true) => bind::bind_owner(&v, &key),
                (true, false) => bind::bind(&v, &key),
                (false, true) => bind::forget_owner(&v),
                (false, false) => bind::forget(&v),
            };
            fail_if(err)?;
            state.wake();
            Ok(json!({"ok": true}))
        }
        "leave" => {
            fail_if(knowledge::leave(&key))?;
            state.wake();
            Ok(json!({"ok": true}))
        }
        _ => Err(Fail::new(400, "unknown team op")),
    }
}

fn post_bind(state: &State, body: &Body) -> Out {
    let (repo, op, to) = (text(body, "repo"), text(body, "op"), text(body, "team"));
    if repo.is_empty() {
        return Err(Fail::new(400, "no row selected"));
    }
    let err = match op.as_str() {
        "forget" => bind::forget(&repo),
        "owner" => bind::bind_owner(&owner_of(&repo), &to),
        "bind" => bind::bind(&repo, &to),
        _ => return Err(Fail::new(400, "op must be bind, owner or forget")),
    };
    fail_if(err)?;
    state.wake();
    Ok(json!({"ok": true}))
}

/// Point a repo or an owner (`acme/*`) at its DB repo, or take that rule away. See dbrepo.rs.
fn post_dbrepo(_state: &State, body: &Body) -> Out {
    let target = text(body, "target");
    fail_if(match text(body, "op").as_str() {
        "set" => dbrepo::set(&target, &text(body, "db")),
        "clear" => dbrepo::clear(&target),
        _ => return Err(Fail::new(400, "op must be set or clear")),
    })?;
    Ok(json!({"ok": true}))
}

/// Set what happens to a repo's or an owner's reviews, or release one that is waiting.
fn post_posting(state: &State, body: &Body) -> Out {
    // ponytail: `owner` carries the owner NAME, like /api/auto, not a flag meaning "read `repo` through
    // owner_of". That flag is what the comment in post_auto is about: a client sending the owner in
    // `owner` -- the obvious reading -- widened one repo's rule to the whole org by coincidence.
    let (repo, owner, op) = (text(body, "repo"), text(body, "owner"), text(body, "op"));
    if op == "govern" {
        // explicit, like /api/auto's `on`: a missing field must not read as "turn it off"
        let Some(on) = body.get("on").and_then(|v| v.as_bool()) else {
            return Err(Fail::new(400, "govern needs on: true or on: false"));
        };
        // the board's repos are read first and the lock let go: govern writes the store
        let board = board_repos(&state.lock().sections);
        fail_if(autorev::govern(&owner, on, &board))?;
        state.wake();
        return Ok(json!({"ok": true}));
    }
    if TALK_OPS.contains(&op.as_str()) {
        let n = body.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
        let Some(h) = held::get(&repo, n) else {
            return Err(Fail::new(404, "nothing waiting for that PR"));
        };
        return talk(state, review::Talk::Held, h, &op, body);
    }
    if op == "release" || op == "discard" {
        if repo.is_empty() {
            return Err(Fail::new(400, "no row selected"));
        }
        let n = body.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
        let Some(h) = held::get(&repo, n) else {
            return Err(Fail::new(404, "nothing waiting for that PR"));
        };
        // ponytail: a drop waits for whatever is running on the row, as a release already does through
        // start_post_held. Dropped mid-discussion, the file came straight back when the answer was
        // written to it.
        if op == "discard" && state.busy(&h.pr.url) {
            return Err(Fail::new(409, "a review of this PR is already running"));
        }
        // ponytail: the verdict you read is the verdict that goes up. With a revision waiting, which one
        // you meant to post is a question, and posting the older one silently is the wrong answer to it.
        if op == "release" && h.proposed.is_some() {
            return Err(Fail::new(409, "accept or keep the revision first"));
        }
        if op == "discard" {
            fail_if(
                held::drop(&repo, n)
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_default(),
            )?;
            // ponytail: and the STATUS, not just the file. The hold wrote its verdict into the row
            // through finish(); dropping only the file left the row reading "changes requested
            // (waiting to post)" with nothing waiting, the menu calling it Reviewed, and auto
            // skipping the PR until a push or a restart.
            state.forget_review(&h.pr.url);
            state.wake();
            return Ok(json!({"ok": true, "status": "dropped"}));
        }
        // ponytail: on a thread with the row spinning, like every other review action. Posting is
        // two network calls and this handler answers the UI.
        // ponytail: the return is the answer, not a formality. It is false when a review of this URL
        // is already in flight, and dropping it answered ok to a release that started nothing — the
        // flash said "posting…", the file stayed, and nothing went up. post_review two hundred lines
        // up already knew this.
        if !state.start_post_held(h) {
            return Err(Fail::new(409, "a review of this PR is already running"));
        }
        return Ok(json!({"ok": true}));
    }
    // ponytail: its own op rather than a third `ran`. Inline is a property of the repo and not of
    // who started the review, so folding it into the manual/auto axis would have invented a
    // distinction the rule does not have (#161).
    if op == "inline" {
        let word = text(body, "inline");
        let rule = autorev::Inline::parse(&word);
        if rule.is_none() && word != autorev::CLEAR {
            return Err(Fail::new(400, "inline must be on, off or none"));
        }
        if repo.is_empty() && owner.is_empty() {
            return Err(Fail::new(400, "name a repo or an owner"));
        }
        if !repo.is_empty() && !owner.is_empty() {
            return Err(Fail::new(400, "name a repo or an owner, not both"));
        }
        fail_if(if repo.is_empty() {
            autorev::set_inline_owner(&owner, rule)
        } else {
            autorev::set_inline(&repo, rule)
        })?;
        state.wake();
        return Ok(json!({"ok": true}));
    }
    let ran = match text(body, "ran").as_str() {
        "manual" => autorev::Ran::Manual,
        "auto" => autorev::Ran::Auto,
        _ => return Err(Fail::new(400, "ran must be manual or auto")),
    };
    let word = text(body, "post");
    // `none` takes the rule off rather than setting one: an owner rule that could only ever be flipped
    // between post and hold governed every repo under it for good, with no way back to per-repo control
    let p = autorev::Post::parse(&word);
    if p.is_none() && word != autorev::CLEAR {
        return Err(Fail::new(400, "post must be post, hold or none"));
    }
    if repo.is_empty() && owner.is_empty() {
        return Err(Fail::new(400, "name a repo or an owner"));
    }
    if !repo.is_empty() && !owner.is_empty() {
        return Err(Fail::new(400, "name a repo or an owner, not both"));
    }
    fail_if(match (p, repo.is_empty()) {
        (Some(p), true) => autorev::set_post_owner(&owner, ran, p),
        (Some(p), false) => autorev::set_post(&repo, ran, p),
        (None, true) => autorev::clear_post_owner(&owner, ran),
        (None, false) => autorev::clear_post(&repo, ran),
    })?;
    state.wake();
    Ok(json!({"ok": true}))
}

/// Start the Friday report in the background, or open the newest one written.
fn post_report(_state: &State, body: &Body) -> Out {
    match text(body, "op").as_str() {
        "start" => {
            start_job("report", report::write);
            Ok(json!({"ok": true}))
        }
        "open" => {
            let path = report::latest().ok_or_else(|| Fail::new(404, "no report yet"))?;
            github::open_file(&path);
            Ok(json!({"ok": true}))
        }
        _ => Err(Fail::new(400, "op must be start or open")),
    }
}

fn post_dream(_state: &State, body: &Body) -> Out {
    let op = text(body, "op");
    if op == "start" {
        let model = config::get().model;
        start_job("dream", move || Ok(dream_result(memory::dream(&model)?)));
        return Ok(json!({"ok": true}));
    }
    if op == "discard" {
        jobs().remove("dream");
        return Ok(json!({"ok": true}));
    }
    if op == "apply" {
        let new = job_of("dream").and_then(|j| {
            let j = j.lock().unwrap_or_else(|e| e.into_inner());
            if j.running {
                return None;
            }
            j.result
                .as_ref()
                .and_then(|r| r.get("new"))
                .and_then(Value::as_object)
                .cloned()
        });
        let Some(new) = new else {
            return Err(Fail::new(409, "no dream to apply"));
        };
        let dir = config::get().memory_dir;
        // ponytail: YOUR dir only, on both legs. A dream rewrites nothing under a team checkout now, so
        // pulling one first bought nothing and pushing one afterwards was worse: push_dir runs
        // `git add -A`, so somebody else's pending team changes were committed under "dream cleanup",
        // and its error could report "memory rewritten, but NOT committed" about a source the dream
        // never touched.
        team::pull_dir(&dir, "mine");
        let new: Vec<(String, String)> = new
            .iter()
            .map(|(n, t)| (n.clone(), t.as_str().unwrap_or("").to_string()))
            .collect();
        memory::write(&new)?;
        jobs().remove("dream");
        let err = team::push_dir(&dir, "memory: dream cleanup", "mine");
        let error = if err.is_empty() {
            String::new()
        } else {
            format!("memory rewritten, but NOT committed: {err} — a backup is in ~/.prs_backups")
        };
        return Ok(json!({"ok": true, "error": error}));
    }
    Err(Fail::new(400, "op must be start, apply or discard"))
}

fn post_request_review(state: &State, body: &Body) -> Out {
    let (pr, _) = need_pr(state, &text(body, "url"))?;
    let login = text(body, "login").trim().to_string();
    if login.is_empty() {
        return Err(Fail::new(400, "a login is needed"));
    }
    fail_if(github::request_review(pr.repo(), pr.number, &login))?;
    state.wake(); // refetch so the new reviewer shows on the row
    Ok(json!({"ok": true}))
}

/// Answer one launch-time ask: whether a team may receive facts, or its agents.md may reach sessions.
fn post_consent(state: &State, body: &Body) -> Out {
    let (kind, key, yes) = (text(body, "kind"), text(body, "key"), truthy(body, "yes"));
    // ponytail: "again" forgets the recorded answer so the ask comes back. The prompts list what has
    // NO answer, so re-asking without forgetting first would draw nothing and read as a dead button.
    if text(body, "op") == "again" {
        match kind.as_str() {
            "publishing" => memory::ask_publishing_again(&key),
            "agents" => memory::ask_agents_again(&key),
            _ => return Err(Fail::new(400, "kind must be publishing or agents")),
        };
        // ponytail: computed ONCE. It walks the draft queue and the fact files per team, which is not
        // a thing to do twice in three lines.
        let asks = launch_asks();
        state.lock().asks = asks.clone();
        state.wake();
        return Ok(json!({"ok": true, "asks": asks}));
    }
    match kind.as_str() {
        "publishing" => memory::allow_publishing(&key, yes),
        "agents" => memory::allow_agents(&key, &text(body, "text"), yes), // what was SHOWN is what gets recorded
        _ => return Err(Fail::new(400, "kind must be publishing or agents")),
    }
    state
        .lock()
        .asks
        .retain(|a| !(a["kind"] == kind && a["key"] == key));
    state.wake();
    Ok(json!({"ok": true}))
}

/// Point Memory (L) or Store (C) somewhere else. Answers {confirm} first when the move needs a yes.
fn post_path(_state: &State, body: &Body) -> Out {
    let (which, new, force) = (
        text(body, "which"),
        text(body, "path").trim().to_string(),
        truthy(body, "force"),
    );
    if (which != "L" && which != "C") || new.is_empty() {
        return Err(Fail::new(400, "which must be L or C, with a path"));
    }
    let cfg = config::get();
    let cur = if which == "L" { cfg.local_memory } else { cfg.teams };
    let err = if knowledge::is_remote(&new) {
        if which != "L" {
            return Err(Fail::new(
                400,
                "Store is a local directory — T is what clones a team repo",
            ));
        }
        if !force {
            return Ok(
                json!({"confirm": format!("clone {new} into {}, keeping the facts already there?", knowledge::tilde(&cur))}),
            );
        }
        knowledge::adopt(&new, None)
    } else if knowledge::inside_git(Path::new(&new)) && !force {
        return Ok(
            json!({"confirm": format!("{new} sits in a git repo that does not ignore it — memory could be committed. continue?")}),
        );
    } else if which == "L" {
        knowledge::set_local(Path::new(&new))
    } else {
        knowledge::set_store(Path::new(&new))
    };
    fail_if(err)?;
    Ok(json!({"ok": true}))
}

/// Install the newest release and re-exec. The reply goes out first; the page reconnects.
fn post_update(state: &State, _body: &Body) -> Out {
    let (version, token) = {
        let inner = state.lock();
        (inner.update.clone(), inner.token.clone())
    };
    if version.is_empty() {
        return Err(Fail::new(409, "already on the newest release"));
    }
    let me = state.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        if !token.is_empty() {
            std::env::set_var("GITDASHY_GUI_TOKEN", &token); // the re-exec'd server must answer on the same token
        }
        let err = update::apply_update(&version); // re-execs on success
        if !err.is_empty() {
            me.lock().notices.push(format!("update failed: {err}"));
        }
    });
    Ok(json!({"ok": true}))
}

fn post_quit(_state: &State, _body: &Body) -> Out {
    // ponytail: the request threads have nothing to flush; the reply goes out, then the process ends
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(200));
        report::clear();
        std::process::exit(0);
    });
    Ok(json!({"ok": true}))
}

fn post_notices(state: &State, _body: &Body) -> Out {
    state.lock().notices.clear();
    Ok(json!({"ok": true}))
}

fn post_changelog(state: &State, _body: &Body) -> Out {
    state.lock().changelog.clear();
    update::mark_seen();
    Ok(json!({"ok": true}))
}

/// A JSON value that names one of `options`, as Python's `body[key] in options`.
fn pick<'a>(v: &Value, options: &[&'a str]) -> Option<&'a str> {
    let s = v.as_str()?;
    options.iter().copied().find(|o| *o == s)
}

/// Apply the settings the page can change, and persist them.
///
/// ponytail: values off the wire, so every one is checked here. The interval drives a loop that hits
/// the GitHub API: 0 would spin it flat out against your rate limit, and a string would break inside
/// the refresh thread, where nothing is watching. Nothing lands until every key checked out.
fn post_settings(state: &State, body: &Body) -> Out {
    // every request has its own thread; see config::SAVING
    let _held = config::SAVING.lock().unwrap_or_else(|e| e.into_inner());
    let mut c = config::get();
    let mut wake = false;
    let mut window_changed = false;
    if let Some(v) = body.get("interval") {
        let n = match v {
            Value::Number(n) => n.as_f64().map(|f| f as i64),
            Value::String(s) => s.trim().parse::<i64>().ok(),
            _ => None,
        };
        let Some(n) = n else {
            return Err(Fail::new(400, "interval must be a number"));
        };
        let Some(n) = u64::try_from(n).ok().filter(|n| config::interval_ok(*n)) else {
            return Err(Fail::new(400, "interval must be 30s to a day"));
        };
        c.interval = n;
        wake = true; // a shorter interval should not wait out the longer one it replaced
    }
    if let Some(v) = body.get("model") {
        let name = text(body, "model").trim().to_string();
        if !v.is_string() || !config::model_ok(&name) {
            return Err(Fail::new(400, "bad model"));
        }
        c.model = name;
    }
    for (key, options) in [
        ("depth", config::DEPTHS),
        ("effort", config::EFFORTS),
        ("theme", config::THEMES),
    ] {
        if let Some(v) = body.get(key) {
            let Some(got) = pick(v, options) else {
                let list = options
                    .iter()
                    .map(|o| if o.is_empty() { "default" } else { o })
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(Fail(400, format!("{key} must be one of {list}")));
            };
            match key {
                "depth" => c.depth = got.into(),
                "effort" => c.effort = got.into(),
                _ => c.theme = got.into(),
            }
        }
    }
    for (key, options) in [("voice", config::VOICES), ("hunter", config::HUNTERS)] {
        if let Some(v) = body.get(key) {
            let got: Vec<&str> = v
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let odd = v
                .as_array()
                .map(|a| a.iter().any(|x| !x.is_string()))
                .unwrap_or(false);
            if odd || got.iter().any(|g| !options.contains(g)) {
                return Err(Fail(400, format!("{key} must be from {}", options.join(", "))));
            }
            let new: Vec<String> = options
                .iter()
                .filter(|o| got.contains(o))
                .map(|o| o.to_string())
                .collect(); // ponytail: rebuilt in option order
            if key == "voice" && new.is_empty() {
                return Err(Fail::new(
                    400,
                    "at least one voice stays on, or nothing gets posted",
                ));
            }
            if key == "voice" {
                c.voice = new;
            } else {
                c.hunter = new;
            }
        }
    }
    if let Some(v) = body.get("spells") {
        let Some(names) = v.as_array().and_then(|a| {
            a.iter()
                .map(|x| x.as_str().map(String::from))
                .collect::<Option<Vec<_>>>()
        }) else {
            return Err(Fail::new(400, "spells must be a list of names"));
        };
        if let Some(bad) = names.iter().find(|n| spells::get(n).is_none()) {
            return Err(Fail(400, format!("no spell {bad}")));
        }
        c.spells = names;
    }
    if let Some(v) = body.get("subs") {
        let Some(got) = pick(v, config::SUBS) else {
            return Err(Fail(
                400,
                format!("subs must be one of {}", config::SUBS.join(", ")),
            ));
        };
        c.sub = got.into();
    }
    if let Some(v) = body.get("window") {
        let got = match v {
            Value::Null => Some(None),
            Value::Number(n) => n.as_u64().map(Some),
            _ => None,
        };
        match got {
            Some(w) if config::WINDOWS.contains(&w) => {
                c.window = w;
                // TEAM searches within the window, so it has to refetch -- but only if a scope is still
                // on once this request is done. Deciding here read the scopes the request was about to
                // replace, so a body setting `window` and clearing `scopes` woke a fetch for nothing.
                window_changed = true;
            }
            _ => {
                return Err(Fail::new(
                    400,
                    "window must be one of the offered hours, or null for all",
                ))
            }
        }
    }
    if body.contains_key("drafts") {
        c.drafts = truthy(body, "drafts");
    }
    if let Some(v) = body.get("scopes") {
        // ponytail: shape-checked, not checked against scope_options: an org you toggled stays on
        // while its PRs are merged away. scope_terms drops anything it cannot put in a query.
        let got: Option<Vec<String>> = v.as_array().and_then(|a| {
            a.iter()
                .map(|x| {
                    x.as_str()
                        .filter(|s| (s.starts_with("org:") || s.starts_with("team:")) && s.len() <= 100)
                        .map(String::from)
                })
                .collect()
        });
        let Some(got) = got.filter(|g| g.len() <= 50) else {
            return Err(Fail::new(
                400,
                "scopes must be a list of org:<owner> or team:<key>",
            ));
        };
        // a scope already searched this session is filtered in the board, no fetch; a new one is fetched now
        wake |= got.iter().any(|s| !github::fetched(s));
        c.scopes = got;
    }
    wake |= window_changed && !c.scopes.is_empty();
    // ponytail: capped, not pruned here; the page keeps only the newest marks before it sends
    let marks = |key: &str| -> Result<Option<HashMap<String, String>>, Fail> {
        let Some(v) = body.get(key) else { return Ok(None) };
        let got: Option<HashMap<String, String>> = v.as_object().filter(|m| m.len() <= 5000).and_then(|m| {
            m.iter()
                .map(|(u, t)| {
                    t.as_str()
                        .filter(|t| u.len() <= 512 && t.len() <= 64)
                        .map(|t| (u.clone(), t.to_string()))
                })
                .collect()
        });
        got.map(Some).ok_or_else(|| {
            Fail::new(
                400,
                format!("{key} must be an object of url to updatedAt, at most 5000"),
            )
        })
    };
    if let Some(got) = marks("read")? {
        c.read = got;
    }
    if let Some(got) = marks("hidden")? {
        c.hidden = got;
    }
    if body.contains_key("hinted") {
        c.hinted = truthy(body, "hinted");
    }
    if body.contains_key("keyhints") {
        c.keyhints = truthy(body, "keyhints");
    }
    // ponytail: the one setting that writes to someone else's PR, and until now the only one with no
    // way to switch it off from inside the app — `--inline` saved itself and every later launch kept
    // posting (#161). A repo's own rule still beats it.
    if body.contains_key("inline") {
        c.inline = truthy(body, "inline");
    }
    // the two panels' layouts: a map of list name to section names
    let layout = |key: &str| -> Result<Option<HashMap<String, Vec<String>>>, Fail> {
        let Some(v) = body.get(key) else { return Ok(None) };
        let names = |l: &Value| -> Option<Vec<String>> {
            l.as_array()
                .filter(|a| a.len() <= 20)?
                .iter()
                .map(|x| x.as_str().filter(|s| s.len() <= 20).map(String::from))
                .collect()
        };
        v.as_object()
            .filter(|m| m.len() <= 10)
            .and_then(|m| m.iter().map(|(k, l)| Some((k.clone(), names(l)?))).collect())
            .map(Some)
            .ok_or_else(|| Fail::new(400, format!("{key} must map a name to a list of section names")))
    };
    if let Some(got) = layout("pane")? {
        c.pane = got;
    }
    if let Some(got) = layout("side")? {
        c.side = got;
    }
    if body.contains_key("notify") {
        c.notify = truthy(body, "notify");
    }
    config::normalise(&mut c);
    let saved = config::snapshot(&c);
    config::update(|cfg| *cfg = c);
    config::save(&saved)?;
    if wake {
        state.wake();
    }
    Ok(json!({"ok": true}))
}

// ---------------------------------------------------------------- the server

type Get = fn(&State, &Query) -> Out;
type Post = fn(&State, &Body) -> Out;

fn get_route(path: &str) -> Option<Get> {
    Some(match path {
        "/api/state" => get_state,
        "/api/debug" => get_debug,
        "/api/asks" => get_asks,
        "/api/pr" => get_pr,
        "/api/diff" => get_diff,
        "/api/prereview" => get_prereview,
        "/api/memory" => get_memory,
        "/api/memory/files" => get_memory_files,
        "/api/drafts" => get_drafts,
        "/api/spells" => get_spells,
        "/api/overlaps" => get_overlaps,
        "/api/doc-help" => get_doc_help,
        "/api/teams" => get_teams,
        "/api/bind" => get_bind,
        "/api/posting" => get_posting,
        "/api/learning" => get_learning,
        "/api/dream" => get_dream,
        "/api/collaborators" => get_collaborators,
        "/api/story" => get_story,
        "/api/dbschema" => get_dbschema,
        "/api/repos" => |_, _| Ok(json!({"repos": github::my_repos()})),
        "/api/stories" => |_, _| Ok(json!({"follow": story::followed()})),
        "/api/changelog" => |_, _| {
            update::recent()
                .map(|text| json!({"text": text}))
                .map_err(|e| Fail::new(502, format!("release notes: {e}")))
        },
        _ => return None,
    })
}

fn post_route(path: &str) -> Option<Post> {
    Some(match path {
        "/api/review" => post_review,
        "/api/prereview" => post_prereview,
        "/api/spell" => post_spell,
        "/api/auto" => post_auto,
        "/api/settings" => post_settings,
        "/api/refresh" => post_refresh,
        "/api/open" => post_open,
        "/api/copy" => post_copy,
        "/api/memory" => post_memory,
        "/api/drafts" => post_drafts,
        "/api/overlaps" => post_overlaps,
        "/api/doc-help" => post_doc_help,
        "/api/teams" => post_teams,
        "/api/bind" => post_bind,
        "/api/posting" => post_posting,
        "/api/dbrepo" => post_dbrepo,
        "/api/dream" => post_dream,
        "/api/report" => post_report,
        "/api/request-review" => post_request_review,
        "/api/consent" => post_consent,
        "/api/path" => post_path,
        "/api/update" => post_update,
        "/api/quit" => post_quit,
        "/api/notices" => post_notices,
        "/api/changelog" => post_changelog,
        "/api/stories" => post_stories,
        "/api/story/seen" => post_story_seen,
        _ => return None,
    })
}

/// The launch-time consent questions, as the page shows them: publishing per team, then agents.md.
pub fn launch_asks() -> Vec<Value> {
    let mut asks = Vec::new();
    for (key, drafts, facts) in memory::unasked() {
        let plural = |n: usize, what: &str| {
            if n > 0 {
                format!("{n} {what}{}", if n == 1 { "" } else { "s" })
            } else {
                String::new()
            }
        };
        let waiting = [plural(drafts, "draft"), plural(facts, "fact")]
            .into_iter()
            .filter(|x| !x.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        asks.push(
            json!({"kind": "publishing", "key": key, "name": team::info(&key).name, "waiting": waiting}),
        );
    }
    for (key, text) in memory::unacked_agents() {
        let path = bind::team_dir(&key).unwrap_or_default().join(memory::AGENTS);
        asks.push(json!({"kind": "agents", "key": key, "name": team::info(&key).name, "text": text, "path": knowledge::tilde(&path)}));
    }
    asks
}

/// The launch-time consent questions the page still shows, recomputed on demand. The inline flows
/// (a team just started or joined) ask again without waiting for a restart.
fn get_asks(_state: &State, _query: &Query) -> Out {
    Ok(json!({"asks": launch_asks()}))
}

/// The built app's index shell. Vite emits one; a binary with no dist is a build mistake, not a crash.
fn index_html() -> Vec<u8> {
    DIST.get_file("index.html")
        .map(|f| f.contents().to_vec())
        .unwrap_or_default()
}

/// One embedded asset by request path ("/assets/x.js"), and its content type.
pub(crate) fn asset(path: &str) -> Option<(Vec<u8>, &'static str)> {
    let file = DIST.get_file(path.trim_start_matches('/'))?;
    let ctype = match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    };
    Some((file.contents().to_vec(), ctype))
}

fn parse_query(raw: &str) -> Query {
    let mut out = Query::new();
    for part in raw.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        let decode = |s: &str| {
            urlencoding::decode(&s.replace('+', " "))
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| s.to_string())
        };
        out.entry(decode(k)).or_insert_with(|| decode(v));
    }
    out
}

fn header(req: &Request, name: &'static str) -> String {
    req.headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_default()
}

/// What the window is allowed to load. The window navigates to this server over http, and a remote URL
/// takes its policy from the RESPONSE, not from tauri.conf.json > app.security.csp, which only covers
/// pages on the asset protocol. So the page that renders PR titles, diffs and model output, none of it
/// written by us, gets its CSP here or nowhere.
///
/// ponytail: 'unsafe-inline' for styles only. React writes style attributes and the app sets its theme
/// through CSS variables on <html>; scripts stay 'self', which is the half that matters. The two font
/// origins are what dist/index.html already asks for. frame-src is the logo's player and nothing else.
const CSP: &str = "default-src 'self'; script-src 'self'; connect-src 'self'; img-src 'self' data:; \
     style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src 'self' https://fonts.gstatic.com; \
     frame-src https://www.youtube-nocookie.com; object-src 'none'; base-uri 'self'; frame-ancestors 'none'";

/// The most a POST body may be, before it is read rather than after.
///
/// ponytail: the biggest honest body is a settings post carrying `read` and `hidden`, and post_settings
/// caps those at 5000 entries of a 512-char url and a 64-char time -- about 3 MB each. 16 MB leaves that
/// room several times over and still puts a number on what was `read_to_end` with no limit at all.
const BODY_MAX: usize = 16 * 1024 * 1024;

/// Every reply the server makes. `cache` is None for anything with data in it.
///
/// ponytail: one responder, because send and send_bytes had drifted into the same four headers written
/// twice, and a header added to one of them would have been a header the other quietly lacked.
fn send_with(req: Request, code: u16, body: Vec<u8>, ctype: &str, cache: Option<&str>) {
    let ok = |h: Result<Header, ()>| h.expect("a static header is well formed");
    let mut resp = Response::from_data(body)
        .with_status_code(code)
        .with_header(ok(Header::from_bytes("Content-Type", ctype)))
        // ponytail: the page talks to its own origin only; nothing here is meant to be embedded.
        .with_header(ok(Header::from_bytes("X-Frame-Options", "DENY")))
        // ponytail: `--browser` puts the token in the page URL, and the CSP lets the two font origins be
        // reached. Today's browsers default to strict-origin-when-cross-origin, which would already keep
        // a query string off the wire; setting it explicitly keeps it that way if browser defaults change.
        .with_header(ok(Header::from_bytes("Referrer-Policy", "no-referrer")))
        .with_header(ok(Header::from_bytes("Content-Security-Policy", CSP)));
    if let Some(c) = cache {
        resp = resp.with_header(ok(Header::from_bytes("Cache-Control", c)));
    }
    if let Err(e) = req.respond(resp) {
        debug!("gui reply failed: {e}");
    }
}

fn send_json(req: Request, code: u16, body: Value) {
    send_with(req, code, body.to_string().into_bytes(), "application/json", None);
}

/// A static asset. Cacheable hashed names, but no-store keeps it simple.
fn send_bytes(req: Request, code: u16, body: Vec<u8>, ctype: &str) {
    send_with(req, code, body, ctype, Some("no-store"));
}

/// Equal without leaking where they differ.
fn same_token(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The host a Host header names, without its port.
fn host_of(raw: &str) -> String {
    let raw = raw.trim();
    let no_port = match raw.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => raw,
    };
    no_port.trim_matches(|c| c == '[' || c == ']').to_string()
}

/// A shared secret on every request, and a localhost Host.
///
/// ponytail: this server answers with your PRs and starts Claude runs that cost money, so it is
/// a trust boundary even on loopback. Any page you happen to have open can POST to 127.0.0.1
/// without reading the reply, and can reach it by a hostname that resolves there (DNS
/// rebinding): the token stops the first, the Host check stops the second.
fn guard(req: &Request, query: &Query, token: &str) -> Result<(), (u16, &'static str)> {
    let host = host_of(&header(req, "Host"));
    if !["127.0.0.1", "localhost", "::1"].contains(&host.as_str()) {
        return Err((403, "bad host"));
    }
    // The shell and its assets hold no data, so they load without a token; every /api route keeps one.
    // ponytail: the app bundle is 270 KB of JS, and a token query on every <script>/<link> would mean
    // rewriting Vite's index — a hole in the guard for static files is the smaller price.
    let path = req.url().split('?').next().unwrap_or("");
    if path != "/" && !path.starts_with("/api/") {
        return Ok(());
    }
    let got = header(req, "X-Dashy-Token");
    let got = if got.is_empty() {
        q(query, "token").to_string()
    } else {
        got
    };
    if !same_token(&got, token) {
        return Err((401, "bad token"));
    }
    Ok(())
}

/// Run a route and send what it says. A Fail is a status the page reads.
fn answer(req: Request, out: Out) {
    match out {
        Ok(v) => send_json(req, 200, v),
        Err(Fail(code, msg)) => send_json(req, code, json!({"error": msg})),
    }
}

fn handle(state: &State, token: &str, mut req: Request) {
    let url = req.url().to_string();
    let (path, raw_query) = url.split_once('?').unwrap_or((&url, ""));
    let query = parse_query(raw_query);
    debug!("gui {} {}", req.method(), path);
    if let Err((code, msg)) = guard(&req, &query, token) {
        return send_json(req, code, json!({"error": msg}));
    }
    match req.method() {
        Method::Get => {
            if path == "/" {
                return send_bytes(req, 200, index_html(), "text/html; charset=utf-8");
            }
            if let Some((body, ctype)) = asset(path) {
                return send_bytes(req, 200, body, ctype);
            }
            match get_route(path) {
                Some(f) => answer(req, f(state, &query)),
                None => send_json(req, 404, json!({"error": "not found"})),
            }
        }
        Method::Post => {
            let Some(f) = post_route(path) else {
                return send_json(req, 404, json!({"error": "not found"}));
            };
            // an honest Content-Length is refused without reading a byte of it; a body that lies, or
            // declares nothing at all, is caught by the cap on the read below
            if header(&req, "Content-Length")
                .parse::<u64>()
                .is_ok_and(|n| n > BODY_MAX as u64)
            {
                return send_json(req, 413, json!({"error": "body too large"}));
            }
            let mut raw = Vec::new();
            // one byte past the cap, so a body that hits it is known to be over rather than exactly at it
            let read = req.as_reader().take(BODY_MAX as u64 + 1).read_to_end(&mut raw);
            if raw.len() > BODY_MAX {
                return send_json(req, 413, json!({"error": "body too large"}));
            }
            let body: Option<Body> = read
                .ok()
                .and_then(|_| {
                    if raw.is_empty() {
                        Some(Value::Object(Body::new()))
                    } else {
                        serde_json::from_slice(&raw).ok()
                    }
                })
                .and_then(|v: Value| if let Value::Object(o) = v { Some(o) } else { None });
            match body {
                Some(body) => answer(req, f(state, &body)),
                None => send_json(req, 400, json!({"error": "bad body"})),
            }
        }
        _ => send_json(req, 404, json!({"error": "not found"})),
    }
}

/// Serve on 127.0.0.1:`port` (0 = any free) and return the bound port. Runs its accept loop on a thread.
pub fn serve(state: State, port: u16, token: String) -> std::io::Result<u16> {
    let server = Server::http(("127.0.0.1", port)).map_err(|e| std::io::Error::other(e.to_string()))?;
    let bound = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(port);
    state.lock().token = token.clone();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let (state, token) = (state.clone(), token.clone());
            std::thread::spawn(move || handle(&state, &token, req));
        }
    });
    Ok(bound)
}

/// A random session token, hex.
pub fn new_token() -> String {
    let mut raw = [0u8; 24];
    getrandom::fill(&mut raw).expect("randomness");
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_row_is_marked_for_schema_changes_and_risks_not_for_reads() {
        let with = |db: serde_json::Value| crate::types::LogEntry {
            db: Some(db),
            ..Default::default()
        };
        let table = |change: &str| serde_json::json!({"tables": [{"name": "t", "change": change}]});
        for change in ["added", "altered", "dropped"] {
            assert!(super::changes_db(&with(table(change))), "{change}");
        }
        assert!(!super::changes_db(&with(table("read"))));
        assert!(!super::changes_db(&with(table("written"))));
        assert!(super::changes_db(&with(
            serde_json::json!({"risks": [{"text": "x"}]})
        )));
        assert!(!super::changes_db(&with(serde_json::json!("junk"))));
        assert!(!super::changes_db(&Default::default()));
    }

    #[test]
    fn csp_lets_the_player_frame_load() {
        // the embed in src/components/FloatingVideo.tsx; vite dev sends no CSP, so only this catches a block
        let embed = include_str!("../../src/components/FloatingVideo.tsx");
        assert!(embed.contains("const EMBED = 'https://www.youtube-nocookie.com'"));
        assert!(super::CSP.contains("frame-src https://www.youtube-nocookie.com;"));
    }

    use super::*;
    use crate::types::{Hunk, Line, Login, Repository, Section};

    fn pr() -> Pr {
        Pr {
            number: 7,
            title: "T".into(),
            url: "u".into(),
            updated_at: "2020-01-01T00:00:00Z".into(),
            author: Some(Login { login: "me".into() }),
            repository: Repository {
                name_with_owner: "a/b".into(),
                name: "b".into(),
            },
            status: "· awaiting review".into(),
            ..Default::default()
        }
    }

    /// A real server over a State with one PR in it: (base url, token, state).
    fn served() -> (String, String, State) {
        let state = State::new();
        state.lock().sections = vec![
            Section {
                name: "MINE".into(),
                prs: Some(vec![pr()]),
                err: None,
            },
            Section {
                name: "ASSIGNED".into(),
                prs: None,
                err: Some("boom\nsecond line".into()),
            },
        ];
        let token = "t0ken".to_string();
        let port = serve(state.clone(), 0, token.clone()).unwrap();
        (format!("http://127.0.0.1:{port}"), token, state)
    }

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent()
    }

    fn get(url: &str, token: Option<&str>) -> (u16, Value) {
        let mut req = agent().get(url);
        if let Some(t) = token {
            req = req.header("X-Dashy-Token", t);
        }
        let mut resp = req.call().unwrap();
        let text = resp.body_mut().read_to_string().unwrap();
        (
            resp.status().as_u16(),
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    fn post(url: &str, body: Value, token: &str) -> (u16, Value) {
        let mut resp = agent()
            .post(url)
            .header("X-Dashy-Token", token)
            .send_json(body)
            .unwrap();
        let text = resp.body_mut().read_to_string().unwrap();
        (
            resp.status().as_u16(),
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    #[test]
    fn story_routes_turn_away_a_bad_login_and_a_missing_list() {
        let (base, token, _state) = served();
        let (code, body) = get(&format!("{base}/api/story?login=x%22%20repo:evil"), Some(&token));
        assert_eq!(
            (code, body["error"].as_str()),
            (400, Some("login must be a GitHub username"))
        );
        let (code, body) = post(&format!("{base}/api/stories"), json!({}), &token);
        assert_eq!(
            (code, body["error"].as_str()),
            (400, Some("follow must be a list"))
        );
        // ponytail: clearing a mark still goes through login_ok -- it names a key in the story store, and a
        // body with no login at all must not reach it either
        for bad in [json!({}), json!({"login": "../../etc"})] {
            let (code, body) = post(&format!("{base}/api/story/seen"), bad.clone(), &token);
            assert_eq!(
                (code, body["error"].as_str()),
                (400, Some("login must be a GitHub username")),
                "{bad}"
            );
        }
        assert_eq!(post(&format!("{base}/api/story/seen"), json!({}), "").0, 401);
    }

    #[test]
    fn the_context_a_query_asks_for_is_one_the_ring_offers() {
        let of = |raw: &str| context_of(&parse_query(raw));
        for &c in diff::CONTEXTS {
            assert_eq!(of(&format!("context={c}")), c);
        }
        // ponytail: the whole point. narrow() computes `i + context`, so usize::MAX off the wire
        // overflowed that sum -- a panic here in debug, wrong lines in release.
        for bad in [
            "",
            "context=",
            "context=9",
            "context=-1",
            "context=x",
            &format!("context={}", usize::MAX),
        ] {
            assert_eq!(of(bad), diff::CONTEXTS[0], "{bad}");
        }
    }

    #[test]
    fn a_post_body_past_the_cap_is_refused_before_it_is_parsed() {
        let (base, token, _state) = served();
        // ponytail: /api/notices, not /api/settings. The cap is about the read, not about any one route,
        // and post_settings writes the config -- whose DEFAULT path is the real ~/.prs_settings.json, so a
        // test posting there without config::test_lock() and a temp path saves over the settings of
        // whoever is running the suite. This route clears a Vec on the test's own State and touches no disk.
        let url = format!("{base}/api/notices");
        assert_eq!(post(&url, json!({"pad": "x".repeat(1024)}), &token).0, 200);
        let (code, body) = post(&url, json!({"pad": "x".repeat(BODY_MAX)}), &token);
        assert_eq!((code, body["error"].as_str()), (413, Some("body too large")));
        // and the server keeps accepting after a 413
        assert_eq!(get(&format!("{base}/api/state"), Some(&token)).0, 200);
    }

    #[test]
    fn an_about_is_offered_as_github_spells_the_repo_and_found_whatever_the_case() {
        let _g = config::test_lock();
        let d = tempfile::tempdir().unwrap();
        let origin = d.path().join("crew.git");
        let t = d.path().join("teams/crew");
        std::fs::create_dir_all(t.join("memory")).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main", &origin.to_string_lossy()])
            .status()
            .unwrap()
            .success());
        assert!(team::init_history(&t));
        assert!(std::process::Command::new("git")
            .args(["remote", "add", "origin", &origin.to_string_lossy()])
            .current_dir(&t)
            .status()
            .unwrap()
            .success());
        std::fs::write(t.join("memory/project.md"), "brief\n").unwrap();
        assert_eq!(team::push_dir(&t, "seed", "sync"), "");
        let bindings = config::get().bindings;
        config::update(|c| {
            c.demo = false;
            c.teams = d.path().join("teams");
            c.bindings = d.path().join("bindings");
        });
        assert_eq!(bind::bind("Acme/API", "crew"), "");
        let (base, token, state) = served();
        // the board: one repo bound to crew, spelled with capitals, and one bound to nothing
        let on = |repo: &str| {
            let mut p = pr();
            p.repository.name_with_owner = repo.into();
            p
        };
        state.lock().sections[0].prs = Some(vec![on("Acme/API"), on("Other/thing")]);

        let (_, j) = get(&format!("{base}/api/memory/files"), Some(&token));
        let abouts: Vec<&str> = j["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["doc"] == "about")
            .map(|f| f["repo"].as_str().unwrap())
            .collect();
        assert_eq!(
            abouts,
            ["Acme/API"],
            "GitHub's spelling, and only what is bound to the team"
        );

        let (code, j) = post(
            &format!("{base}/api/memory"),
            json!({"op": "propose", "team": "crew", "doc": "about", "repo": "Acme/API",
                   "text": "# What this repo is\n\n## Its role\n\nBilling.\n"}),
            &token,
        );
        assert_eq!(code, 200, "{j}");
        // the proposal is approved: the team's file now says it, and every spelling reads it
        std::fs::create_dir_all(t.join("memory/about")).unwrap();
        std::fs::write(t.join("memory/about/Acme__API.md"), "Billing.\n").unwrap();
        for spelled in ["Acme/API", "acme/api"] {
            assert_eq!(memory::about(spelled).0, "Billing.", "{spelled}");
        }
        // and a proposal for another spelling edits that same file, not a second one
        assert_eq!(
            memory::about_path("crew", "acme/api").unwrap(),
            t.join("memory/about/Acme__API.md")
        );
        config::update(|c| c.bindings = bindings);
    }

    #[test]
    fn a_second_request_for_help_waits_its_turn() {
        let _g = config::test_lock();
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("teams/crew/memory")).unwrap();
        assert!(team::init_history(&d.path().join("teams/crew")));
        config::update(|c| c.teams = d.path().join("teams"));
        let (base, token, _state) = served();
        // a help job that has not finished: the one another draft is still waiting on
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        start_job("doc-help", move || {
            let _ = rx.recv_timeout(Duration::from_secs(10));
            Ok(json!({}))
        });
        let (code, j) = post(
            &format!("{base}/api/doc-help"),
            json!({"team": "crew", "doc": "brief", "text": "x"}),
            &token,
        );
        assert_eq!(
            (code, j["error"].as_str()),
            (409, Some("the model is still reading another draft"))
        );
        tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while job("doc-help")["running"] == true && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(job("doc-help")["running"], false);
    }

    #[test]
    fn help_with_a_founding_document_runs_in_the_background_and_writes_nothing() {
        let _g = config::test_lock();
        let d = tempfile::tempdir().unwrap();
        let t = d.path().join("teams/crew");
        std::fs::create_dir_all(t.join("memory")).unwrap();
        assert!(team::init_history(&t));
        config::update(|c| {
            c.demo = true; // demo answers without a model
            c.teams = d.path().join("teams");
        });
        let (base, token, _state) = served();
        let ask = |body: Value| post(&format!("{base}/api/doc-help"), body, &token);
        assert_eq!(ask(json!({"team": "crew", "doc": "general", "text": "x"})).0, 400);
        assert_eq!(ask(json!({"team": "nope", "doc": "brief", "text": "x"})).0, 404);
        assert_eq!(
            ask(json!({"team": "crew", "doc": "about", "repo": "acme/api", "text": "my draft"})).0,
            200
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let j = loop {
            let (_, j) = get(&format!("{base}/api/doc-help"), Some(&token));
            if j["running"] == false || std::time::Instant::now() > deadline {
                break j;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(j["result"]["text"], "my draft", "{j}");
        assert!(
            j["result"]["questions"].as_array().is_some_and(|q| !q.is_empty()),
            "{j}"
        );
        assert!(
            !t.join("memory/about").exists(),
            "help proposes; nothing is written"
        );
        config::update(|c| c.demo = false);
    }

    #[test]
    fn inspect_reads_removes_yours_and_only_ever_proposes_a_change_to_a_team() {
        let _g = config::test_lock();
        let d = tempfile::tempdir().unwrap();
        let (mine, teams) = (d.path().join("mine"), d.path().join("teams"));
        let origin = d.path().join("crew.git");
        let t = teams.join("crew");
        std::fs::create_dir_all(t.join("memory")).unwrap();
        std::fs::create_dir_all(&mine).unwrap();
        let git = |cwd: &std::path::Path, args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(cwd)
                    .status()
                    .unwrap()
                    .success(),
                "{args:?}"
            )
        };
        git(
            d.path(),
            &["init", "-q", "--bare", "-b", "main", &origin.to_string_lossy()],
        );
        assert!(team::init_history(&t));
        git(&t, &["remote", "add", "origin", &origin.to_string_lossy()]);
        std::fs::write(t.join("memory/general.md"), "- theirs\n- kept\n").unwrap();
        std::fs::write(t.join("memory/acme__api.md"), "- api\n").unwrap();
        std::fs::write(t.join("memory/project.md"), "the brief\n").unwrap();
        std::fs::write(t.join("memory/agents.md"), "file what you learn\n").unwrap();
        assert_eq!(team::push_dir(&t, "seed", "sync"), "");
        std::fs::write(mine.join("acme__web.md"), "# acme/web\n- web\n").unwrap();
        std::fs::write(mine.join("general.md"), "- mine one\n- mine two\n").unwrap();
        config::update(|c| {
            c.demo = false;
            c.memory_dir = mine.clone();
            c.teams = teams.clone();
        });
        let (base, token, _state) = served();

        // yours first, general before repos in each; a team's founding documents listed apart from its facts
        let (code, j) = get(&format!("{base}/api/memory/files"), Some(&token));
        assert_eq!(code, 200);
        let files: Vec<String> = j["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                format!(
                    "{}|{}|{}",
                    f["team"].as_str().unwrap(),
                    f["repo"].as_str().unwrap_or(""),
                    f["doc"].as_str().unwrap_or("")
                )
            })
            .collect();
        assert_eq!(
            files,
            [
                "||",
                "|acme/web|",
                "crew||",
                "crew|acme/api|",
                "crew||brief",
                "crew||agents"
            ],
            "acme/api has team facts but is bound to no team here, so an about for it would never be read"
        );
        // an about nobody wrote opens on its template, with what an about is for beside it; nothing is written
        let (code, j) = get(
            &format!("{base}/api/memory?team=crew&doc=about&repo=acme/api"),
            Some(&token),
        );
        assert_eq!(code, 200, "{j}");
        assert_eq!(
            (j["text"].as_str(), j["draft"].as_str()),
            (Some(""), Some(team::ABOUT_TEMPLATE))
        );
        assert_eq!(j["guide"].as_str(), Some(team::ABOUT_GUIDE));
        assert!(!t.join("memory/about").exists());
        let (_, j) = get(&format!("{base}/api/memory?team=crew&doc=brief"), Some(&token));
        assert_eq!(j["guide"].as_str(), Some(team::BRIEF_GUIDE));
        for bad in [
            "doc=about",
            "doc=about&repo=../x",
            "doc=about&repo=nope",
            "doc=general",
        ] {
            assert_eq!(
                get(&format!("{base}/api/memory?team=crew&{bad}"), Some(&token)).0,
                400,
                "{bad}"
            );
        }

        // facts, not text: a heading is not one
        let (_, j) = get(&format!("{base}/api/memory?repo=acme/web"), Some(&token));
        assert_eq!(j["facts"], json!(["web"]));
        let (_, j) = get(&format!("{base}/api/memory?team=crew"), Some(&token));
        assert_eq!(j["facts"], json!(["theirs", "kept"]));
        let (_, j) = get(&format!("{base}/api/memory?team=crew&doc=agents"), Some(&token));
        assert_eq!(j["text"], "file what you learn\n");

        // yours: removed there and then, and only the one
        let remove = |team: &str, fact: &str| {
            post(
                &format!("{base}/api/memory"),
                json!({"op": "remove", "team": team, "repo": "general", "fact": fact}),
                &token,
            )
        };
        assert_eq!(remove("", "mine one").0, 200);
        assert_eq!(
            std::fs::read_to_string(mine.join("general.md")).unwrap(),
            "- mine two\n"
        );
        assert_eq!(remove("", "mine one").0, 404, "already gone");

        // a team's: a branch on its repo, and the checkout every review reads is what it was
        let (code, j) = remove("crew", "theirs");
        assert_eq!(code, 200, "{j}");
        let branch = j["branch"].as_str().unwrap().to_string();
        assert!(branch.starts_with("gitdashy/propose-general-"), "{j}");
        assert_eq!(
            std::fs::read_to_string(t.join("memory/general.md")).unwrap(),
            "- theirs\n- kept\n"
        );
        let shown = std::process::Command::new("git")
            .args(["show", &format!("origin/{branch}:memory/general.md")])
            .current_dir(&t)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&shown.stdout), "- kept\n");

        // a brief with none of a brief's sections gets a word first, and goes only when asked again
        let propose = |body: Value| post(&format!("{base}/api/memory"), body, &token);
        let (code, j) =
            propose(json!({"op": "propose", "team": "crew", "doc": "brief", "text": "a sharper brief\n"}));
        assert_eq!((code, j["ok"].as_bool()), (200, Some(false)), "{j}");
        assert!(
            j["warn"].as_str().unwrap().contains("none of a brief's sections"),
            "{j}"
        );
        assert!(j.get("branch").is_none());
        let (code, j) = propose(
            json!({"op": "propose", "team": "crew", "doc": "brief", "text": "a sharper brief\n", "anyway": true}),
        );
        assert_eq!(code, 200, "{j}");
        assert!(
            j["branch"]
                .as_str()
                .unwrap()
                .starts_with("gitdashy/propose-project-"),
            "{j}"
        );
        // an about is proposed the same way, into about/<repo>.md
        let (code, j) = propose(
            json!({"op": "propose", "team": "crew", "doc": "about", "repo": "acme/api",
                                        "text": "# What this repo is\n\n## Its role\n\nBilling.\n"}),
        );
        assert_eq!(code, 200, "{j}");
        let branch = j["branch"].as_str().unwrap().to_string();
        let shown = std::process::Command::new("git")
            .args(["show", &format!("origin/{branch}:memory/about/acme__api.md")])
            .current_dir(&t)
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&shown.stdout).contains("Billing."));
        assert_eq!(
            std::fs::read_to_string(t.join("memory/project.md")).unwrap(),
            "the brief\n"
        );

        // an empty founding document would be a proposal to delete it
        let (code, _) = post(
            &format!("{base}/api/memory"),
            json!({"op": "propose", "team": "crew", "doc": "agents", "text": "  \n"}),
            &token,
        );
        assert_eq!(code, 400);

        // there is no way left to type a fact in, nor to propose a file that is not a founding document
        let (code, _) = post(
            &format!("{base}/api/memory"),
            json!({"repo": "general", "text": "- typed\n"}),
            &token,
        );
        assert_eq!(code, 400);
        let (code, _) = post(
            &format!("{base}/api/memory"),
            json!({"op": "propose", "team": "crew", "doc": "general", "text": "- typed\n"}),
            &token,
        );
        assert_eq!(code, 400);
        assert_eq!(
            post(
                &format!("{base}/api/memory"),
                json!({"op": "remove", "team": "strangers", "fact": "x"}),
                &token
            )
            .0,
            404
        );
        assert!(!teams.join("strangers").exists());

        // a fact scope that is a founding document's file, or a path trick, is not a repo
        for repo in ["project", "agents", "PROJECT", "Agents", "..\\x", "a/../b"] {
            let (code, _) = post(
                &format!("{base}/api/memory"),
                json!({"op": "remove", "repo": repo, "fact": "x"}),
                &token,
            );
            assert_eq!(code, 400, "{repo}");
        }
        // the pooled drop takes the same repo, and refuses the same tricks
        for repo in ["PROJECT", "..\\x"] {
            let (code, _) = post(
                &format!("{base}/api/drafts"),
                json!({"op": "drop", "pooled": true, "team": "crew", "repo": repo, "fact": "x"}),
                &token,
            );
            assert_eq!(code, 400, "{repo}");
        }
    }

    #[test]
    fn your_team_drafts_are_waiting_and_accepting_one_by_hand_is_a_pull_request() {
        let _g = config::test_lock();
        let d = tempfile::tempdir().unwrap();
        let (mine, teams) = (d.path().join("mine"), d.path().join("teams"));
        let origin = d.path().join("crew.git");
        let t = teams.join("crew");
        std::fs::create_dir_all(t.join("memory")).unwrap();
        std::fs::create_dir_all(&mine).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main", &origin.to_string_lossy()])
            .status()
            .unwrap()
            .success());
        assert!(team::init_history(&t));
        assert!(std::process::Command::new("git")
            .args(["remote", "add", "origin", &origin.to_string_lossy()])
            .current_dir(&t)
            .status()
            .unwrap()
            .success());
        std::fs::write(t.join("memory/general.md"), "- theirs\n").unwrap();
        assert_eq!(team::push_dir(&t, "seed", "sync"), "");
        let bindings = config::get().bindings;
        config::update(|c| {
            c.demo = false;
            c.memory_dir = mine.clone();
            c.teams = teams.clone();
            c.bindings = d.path().join("bindings");
        });
        memory::allow_publishing("crew", true);
        assert_eq!(bind::bind("a/b", "crew"), "");
        memory::append("a/b", "- retry owns backoff", "");
        let (base, token, _state) = served();

        let (_, j) = get(&format!("{base}/api/drafts"), Some(&token));
        let team: Vec<&Value> = j["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["kind"] == "team")
            .collect();
        assert_eq!(team.len(), 1, "{j}");
        assert_eq!(
            (team[0]["team"].as_str(), team[0]["fact"].as_str()),
            (Some("crew"), Some("retry owns backoff"))
        );

        let draft = |op: &str| {
            post(
                &format!("{base}/api/drafts"),
                json!({"op": op, "pooled": true, "team": "crew", "repo": "a/b", "fact": "retry owns backoff"}),
                &token,
            )
        };
        // by hand into the team's knowledge: a branch on its repo, and the team's file untouched here
        let (code, j) = draft("promote");
        assert_eq!(code, 200, "{j}");
        assert!(
            j["branch"].as_str().unwrap().starts_with("gitdashy/propose-a-b-"),
            "{j}"
        );
        assert!(!t.join("memory/a__b.md").exists());
        // dropping your own draft is yours to do, once
        assert_eq!(draft("drop").0, 200);
        assert_eq!(draft("drop").0, 404);
        assert!(memory::team_waiting().is_empty());

        // a team file's facts come with who stands behind each
        std::fs::write(t.join("memory/a__b.md"), "- known\n").unwrap();
        for who in ["alice", "bob"] {
            std::fs::create_dir_all(t.join("memory/pool").join(who)).unwrap();
            std::fs::write(t.join("memory/pool").join(who).join("a__b.md"), "- known\n").unwrap();
        }
        let (_, j) = get(&format!("{base}/api/memory?team=crew&repo=a/b"), Some(&token));
        assert_eq!(
            (j["facts"].clone(), j["backers"].clone()),
            (json!(["known"]), json!([["alice", "bob"]]))
        );
        config::update(|c| c.bindings = bindings);
    }

    #[test]
    fn a_pr_on_a_teams_repo_is_never_reviewed_by_a_model() {
        let _g = config::test_lock();
        let d = tempfile::tempdir().unwrap();
        let t = d.path().join("teams/crew");
        std::fs::create_dir_all(&t).unwrap();
        assert!(team::init_history(&t));
        // pr() is on a/b: make a/b the team's repo
        assert!(std::process::Command::new("git")
            .args(["remote", "add", "origin", "git@github.com:a/b.git"])
            .current_dir(&t)
            .status()
            .unwrap()
            .success());
        config::update(|c| {
            c.demo = false;
            c.teams = d.path().join("teams");
        });
        let (base, token, state) = served();
        let (_, j) = get(&format!("{base}/api/state"), Some(&token));
        assert_eq!(j["sections"][0]["prs"][0]["humanOnly"], true);
        for body in [
            json!({"url": "u"}),
            json!({"url": "u", "self": true}),
            json!({"url": "u", "ask": "look closer"}),
            json!({"url": "u", "spell": "auth-check"}),
        ] {
            let (code, j) = post(&format!("{base}/api/review"), body.clone(), &token);
            assert_eq!(
                (code, j["error"].as_str()),
                (403, Some(team::HUMAN_ONLY)),
                "{body}"
            );
        }
        assert!(!state.busy("u"), "nothing was started");
        assert!(review::cast_spell(&pr(), "opus", "auth-check", "look")
            .is_err_and(|e| e.to_string() == team::HUMAN_ONLY));
        // and where a model run starts, whoever calls it
        let status = review::review(&pr(), "opus", autorev::Ran::Auto, "").unwrap();
        assert_eq!(status, format!("error: {}", team::HUMAN_ONLY));
        assert!(review::self_review(&pr(), "opus")
            .unwrap()
            .0
            .contains(team::HUMAN_ONLY));
    }

    #[test]
    fn the_learning_route_needs_the_token_and_sends_events_without_their_text() {
        let _g = config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| {
            // the talk tests leave demo on under this lock, and demo records and reads nothing
            c.demo = false;
            c.learning = d.path().join("learning.jsonl");
            c.memory_dir = d.path().join("not-a-repo");
            c.teams = d.path().join("no-teams");
        });
        // a repo no other test names: memory.rs tests record events too, and theirs can land in this log
        crate::learning::record("draft", "learning-route/probe", "review", "a probe");
        let (base, token, _state) = served();
        assert_eq!(get(&format!("{base}/api/learning"), None).0, 401);
        let (code, j) = get(&format!("{base}/api/learning"), Some(&token));
        assert_eq!(code, 200);
        let events = j["events"].as_array().unwrap();
        assert!(
            events
                .iter()
                .any(|e| e["repo"] == "learning-route/probe" && e["source"] == "review"),
            "{j}"
        );
        assert!(
            events.iter().all(|e| e.get("fact").is_none()),
            "the fact's text never leaves the machine's memory"
        );
        config::update(|c| c.learning = std::path::PathBuf::new());
    }

    #[test]
    fn clearing_every_scope_with_the_window_does_not_wake_a_fetch() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| {
            c.settings = Some(d.path().join("settings.json"));
            c.scopes = vec!["org:acme".into()];
            c.window = Some(168);
        });
        let (base, token, state) = served();
        // ponytail: both keys in ONE body. The window's refetch used to be decided against the scopes the
        // same request was about to replace, so this woke a fetch for a board with nothing left to search.
        let (code, body) = post(
            &format!("{base}/api/settings"),
            json!({"window": 24, "scopes": []}),
            &token,
        );
        assert_eq!(code, 200, "{body}");
        assert_eq!(config::get().scopes, Vec::<String>::new());
        assert!(!state.lock().wake.is_set(), "no scope is left to search");

        // and the other way: a window change with a scope still on is exactly what has to refetch
        config::update(|c| c.scopes = vec!["org:acme".into()]);
        let (base, token, state) = served();
        let (code, body) = post(&format!("{base}/api/settings"), json!({"window": 720}), &token);
        assert_eq!(code, 200, "{body}");
        assert!(state.lock().wake.is_set());
    }

    #[test]
    fn a_poisoned_job_lock_still_reads_as_running() {
        let name = "poisoned-probe";
        let go = std::sync::Arc::new(Mutex::new(false));
        // a job that will not finish until this test lets it, so `running` is still true below
        let wait = go.clone();
        start_job(name, move || {
            while !*wait.lock().unwrap_or_else(|e| e.into_inner()) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(json!({}))
        });
        let first = job_of(name).expect("the job is registered");
        // poison its mutex: a panic while the lock is held is the only way one gets poisoned
        let p = first.clone();
        let _ = std::thread::spawn(move || {
            let _g = p.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert!(first.is_poisoned());
        // ponytail: `.map(..).unwrap_or(false)` read a poisoned lock as "not running" and started a SECOND
        // job for this name on top of the one still going -- which shows up as the registry's entry being
        // REPLACED. Asserting on the new closure instead would race it: it can finish before we look.
        start_job(name, || Ok(json!({})));
        assert!(
            std::sync::Arc::ptr_eq(&first, &job_of(name).unwrap()),
            "a second job replaced the one still running under {name}"
        );
        *go.lock().unwrap_or_else(|e| e.into_inner()) = true;
    }

    #[test]
    fn debug_route_needs_the_token_and_carries_the_paths() {
        let _g = crate::config::test_lock(); // the paths it carries come from the global config
        let (base, token, _state) = served();
        assert_eq!(get(&format!("{base}/api/debug"), None).0, 401);
        let (code, d) = get(&format!("{base}/api/debug"), Some(&token));
        assert_eq!(code, 200);
        assert_eq!(d["version"], config::VERSION);
        assert!(!d["paths"]["debugLog"].as_str().unwrap().is_empty());
        assert_eq!(d["state"]["sections"][0]["name"], "MINE");
    }

    #[test]
    fn tail_handles_missing_short_and_long_files() {
        let dir = std::env::temp_dir().join(format!("dashy-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("nope.log");
        assert!(tail(&missing, 3).contains("nope.log"));
        let empty = dir.join("empty.log");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(tail(&empty, 3), "");
        let few = dir.join("few.log");
        std::fs::write(&few, "a\nb\n").unwrap();
        assert_eq!(tail(&few, 3), "a\nb");
        let many = dir.join("many.log");
        std::fs::write(&many, (0..10).map(|i| format!("{i}\n")).collect::<String>()).unwrap();
        assert_eq!(tail(&many, 3), "7\n8\n9");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_token_is_refused_and_a_good_one_gets_the_payload() {
        // ponytail: served() answers out of the global config, so a test rewriting it next door is
        // what this payload is built from otherwise. Same lock as everything else that reads it.
        let _g = crate::config::test_lock();
        let (base, token, state) = served();
        assert_eq!(get(&format!("{base}/api/state"), None).0, 401);
        assert_eq!(get(&format!("{base}/api/state?token=wrong"), None).0, 401);
        let (code, d) = get(&format!("{base}/api/state"), Some(&token));
        assert_eq!(code, 200);
        assert_eq!(d["version"], config::VERSION);
        assert_eq!(d["sections"][0]["name"], "MINE");
        let row = &d["sections"][0]["prs"][0];
        assert_eq!(
            (
                row["number"].as_u64(),
                row["repo"].as_str(),
                row["author"].as_str()
            ),
            (Some(7), Some("a/b"), Some("me"))
        );
        assert_eq!(row["status"], "· awaiting review");
        assert_eq!(row["busy"], false);
        // no additions on the fixture: the graph's `?? 0` path depends on null, not a missing zero
        assert_eq!((&row["add"], &row["del"]), (&Value::Null, &Value::Null));
        if let Some(prs) = state.lock().sections[0].prs.as_mut() {
            (prs[0].additions, prs[0].deletions) = (Some(12), Some(3));
        }
        let row = &get(&format!("{base}/api/state"), Some(&token)).1["sections"][0]["prs"][0];
        assert_eq!((&row["add"], &row["del"]), (&json!(12), &json!(3)));
        // a re-review with no kind keeps the tag of the newest review that had one
        {
            let mut st = state.lock();
            let pr = st.sections[0].prs.as_ref().unwrap()[0].clone();
            let entry = |kind: &str, breaking: bool| Pr {
                review: Some(Box::new(LogEntry {
                    kind: kind.into(),
                    breaking,
                    ..Default::default()
                })),
                ..pr.clone()
            };
            st.sections.push(Section {
                name: "REVIEWED".into(),
                prs: Some(vec![
                    entry("", false),
                    entry("security", true),
                    entry("docs", false),
                ]), // newest first
                err: None,
            });
        }
        let d = get(&format!("{base}/api/state"), Some(&token)).1;
        let (row, newest) = (&d["sections"][0]["prs"][0], &d["sections"][2]["prs"][0]);
        assert_eq!(
            (&row["kind"], &row["breaking"]),
            (&json!("security"), &json!(true))
        );
        assert_eq!(
            (&newest["kind"], &newest["breaking"]),
            (&json!("security"), &json!(true))
        );
        assert_eq!(d["sections"][1]["prs"], json!([]));
        assert!(d["sections"][1]["error"].as_str().unwrap().starts_with("boom"));
        // the token in the query works too, as the page load uses it
        assert_eq!(
            get(&format!("{base}/api/state?token={token}"), None).1["running"],
            0
        );
        assert_eq!(get(&format!("{base}/api/nope"), Some(&token)).0, 404);
    }

    #[test]
    fn the_shell_is_guarded_but_its_assets_are_not() {
        let (base, token, _state) = served();
        let (code, body) = get(&format!("{base}/?token={token}"), None);
        assert_eq!(code, 200);
        let html = body.as_str().unwrap();
        assert!(html.contains("<div id=\"root\">"));
        assert_eq!(get(&format!("{base}/"), None).0, 401);
        // static assets load with no token; the app's own <script>/<link> carry none
        let asset = agent().get(format!("{base}/head.png")).call().unwrap();
        assert_eq!(asset.status().as_u16(), 200);

        // the window loads this over http, so the policy has to ride on the response or it is absent.
        // Both fonts origins are in it, or the page renders in a fallback face.
        for r in [
            &asset,
            &agent().get(format!("{base}/?token={token}")).call().unwrap(),
        ] {
            let csp = r.headers()["content-security-policy"].to_str().unwrap();
            assert!(csp.starts_with("default-src 'self'"), "{csp}");
            assert!(csp.contains("script-src 'self';"), "{csp}");
            assert!(csp.contains("https://fonts.gstatic.com"), "{csp}");
            assert!(!csp.contains('\n'), "one header line, not three: {csp:?}");
            // --browser puts the token in the page url; it must not ride out to the font origins above
            assert_eq!(r.headers()["referrer-policy"].to_str().unwrap(), "no-referrer");
        }
    }

    #[test]
    fn busy_follows_in_flight_and_the_review_rides_along() {
        let _g = crate::config::test_lock(); // as above: the payload is read through the config
        let (base, token, state) = served();
        state.lock().running.insert("u".into());
        state.lock().reviews.insert("u".into(), "3 findings".into());
        let d = get(&format!("{base}/api/state"), Some(&token)).1;
        assert_eq!(d["sections"][0]["prs"][0]["busy"], true);
        assert_eq!(d["sections"][0]["prs"][0]["review"], "3 findings");
        assert_eq!(d["running"], 1);
        // and a review of a row in flight is refused; one off the board is not something to pay for
        assert_eq!(
            post(&format!("{base}/api/review"), json!({"url": "u"}), &token).0,
            409
        );
        assert_eq!(
            post(
                &format!("{base}/api/review"),
                json!({"url": "https://elsewhere/1"}),
                &token
            )
            .0,
            404
        );
    }

    /// The route carries two different jobs on one path: the master switch, and the per-repo scope.
    /// A body with no `repo` must still be the switch, or turning auto on would arm nothing.
    #[test]
    fn the_auto_route_arms_a_repo_without_touching_the_switch() {
        let _g = crate::config::test_lock(); // config.autorev is process-global; one lock for every test that moves it
        let d = tempfile::tempdir().unwrap();
        config::update(|c| c.autorev = d.path().join("autorev"));
        let (base, token, state) = served();

        post(&format!("{base}/api/auto"), json!({"on": true}), &token);
        assert!(state.lock().auto, "no repo named, so this is the master switch");
        assert_eq!(
            get(&format!("{base}/api/state"), Some(&token)).1["autoScope"],
            json!([])
        );

        post(&format!("{base}/api/auto"), json!({"on": false}), &token);
        assert!(!state.lock().auto);
        post(
            &format!("{base}/api/auto"),
            json!({"repo": "acme/api", "on": true}),
            &token,
        );
        assert!(!state.lock().auto, "arming a repo must not flip the switch");
        assert_eq!(
            get(&format!("{base}/api/state"), Some(&token)).1["autoScope"],
            json!([{"target": "acme/api", "on": true}])
        );
        assert!(autorev::scope().armed("acme/api") && !autorev::scope().armed("other/thing"));
        assert_eq!(
            get(&format!("{base}/api/state"), Some(&token)).1["autoEverywhere"],
            json!(false)
        );

        // the owner is its own value, not a flag that re-reads `repo`
        post(
            &format!("{base}/api/auto"),
            json!({"owner": "acme", "on": true}),
            &token,
        );
        assert!(
            autorev::scope().armed("acme/web"),
            "the owner rule reaches a sibling repo"
        );
        post(
            &format!("{base}/api/auto"),
            json!({"owner": "acme", "on": false}),
            &token,
        );
        assert!(!autorev::scope().armed("acme/web") && autorev::scope().armed("acme/api"));

        // a store holding nothing but an --off row is a non-empty list while auto still covers
        // everything, so the flag and the rows must not be derived from each other
        post(
            &format!("{base}/api/auto"),
            json!({"repo": "acme/api", "on": false}),
            &token,
        );
        let d = get(&format!("{base}/api/state"), Some(&token)).1;
        assert_eq!(
            d["autoEverywhere"],
            json!(true),
            "nothing armed, so auto covers everything"
        );
        assert_eq!(
            d["autoScope"],
            json!([{"target": "acme/*", "on": false}, {"target": "acme/api", "on": false}])
        );

        // a scope change with no `on` is a caller bug, not a disarm: truthy() read it as false
        let (code, body) = post(&format!("{base}/api/auto"), json!({"repo": "acme/api"}), &token);
        assert_eq!(
            (code, body["error"].as_str()),
            (400, Some("a scope change needs on: true or on: false"))
        );

        // both at once resolves one way here and another in the CLI unless it is refused
        let (code, body) = post(
            &format!("{base}/api/auto"),
            json!({"repo": "acme/api", "owner": "beta", "on": true}),
            &token,
        );
        assert_eq!(
            (code, body["error"].as_str()),
            (400, Some("name a repo or an owner, not both"))
        );
        assert!(
            !autorev::scope().listed().iter().any(|(t, _)| t == "beta/*"),
            "the refused body wrote nothing"
        );

        let (code, body) = post(
            &format!("{base}/api/auto"),
            json!({"repo": "notes", "on": true}),
            &token,
        );
        assert_eq!(
            (code, body["error"].as_str()),
            (400, Some("notes is not an owner/name"))
        );
        let (code, body) = post(
            &format!("{base}/api/auto"),
            json!({"owner": "acme/api", "on": true}),
            &token,
        );
        assert_eq!(
            (code, body["error"].as_str()),
            (400, Some("acme/api is not an owner"))
        );
    }

    /// The schema graph only clones a DB repo some rule names: anything else is refused before git runs.
    #[test]
    fn db_schema_refuses_a_repo_no_rule_names() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| c.dbrepo = d.path().join("dbrepo"));
        assert_eq!(dbrepo::set("acme/*", "acme/schema"), "");
        let (base, token, _state) = served();
        for db in ["", "acme/other", "not a repo", "acme/*"] {
            let (code, body) = get(
                &format!("{base}/api/dbschema?db={}", urlencoding::encode(db)),
                Some(&token),
            );
            assert_eq!(
                (db, code, body["error"].as_str()),
                (db, 400, Some("db must be a DB repo a rule names"))
            );
        }
    }

    /// The rail's Database group through HTTP: set, clear, and a bad op or target refused, each read back off the payload.
    #[test]
    fn db_repo_rules_through_the_route() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| c.dbrepo = d.path().join("dbrepo"));
        let (base, token, _state) = served();
        let rules = || get(&format!("{base}/api/state"), Some(&token)).1["dbRules"].clone();
        let url = format!("{base}/api/dbrepo");
        assert_eq!(
            post(
                &url,
                json!({"op": "set", "target": "acme/*", "db": "acme/schema"}),
                &token
            )
            .0,
            200
        );
        assert_eq!(
            post(
                &url,
                json!({"op": "set", "target": "acme/docs", "db": ""}),
                &token
            )
            .0,
            200
        );
        assert_eq!(
            rules(),
            json!([{"target": "acme/*", "db": "acme/schema"}, {"target": "acme/docs", "db": ""}])
        );
        assert_eq!(
            post(&url, json!({"op": "clear", "target": "acme/docs"}), &token).0,
            200
        );
        assert_eq!(rules(), json!([{"target": "acme/*", "db": "acme/schema"}]));
        assert_eq!(
            post(&url, json!({"op": "drop", "target": "acme/*"}), &token).0,
            400
        );
        assert_eq!(
            post(&url, json!({"op": "set", "target": "a/b/c", "db": "x/y"}), &token).0,
            400
        );
        assert_eq!(
            post(&url, json!({"op": "set", "target": "acme/*"}), "wrong").0,
            401
        );
        assert_eq!(rules(), json!([{"target": "acme/*", "db": "acme/schema"}]));
    }

    /// The rail reads the answer off the selected PR's detail, so the page never resolves the rule
    /// itself, and the payload lists every rule that exists.
    #[test]
    fn the_payload_lists_every_posting_target_resolved() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| {
            c.autorev = d.path().join("autorev");
            c.held_dir = d.path().join("held");
            // ponytail: pinned, because the panel's inline column falls back to this switch. Left to
            // whatever the process config holds, the rows read differently depending on which test
            // ran before — the failure #160 was about, in a new place.
            c.inline = false;
        });
        let (base, token, _state) = served();
        let rules = || get(&format!("{base}/api/state"), Some(&token)).1["postingRules"].clone();

        // the board's own repo is listed before anything is set: the panel is a list you walk, and the
        // repo with no rule is the one you came to give one
        assert_eq!(
            rules(),
            json!([
                {"target": "a/*", "manual": "post", "auto": "post", "manualVia": "", "autoVia": "",
                 "inline": false, "inlineVia": "", "perRepo": false},
                {"target": "a/b", "manual": "post", "auto": "post", "manualVia": "", "autoVia": "",
                 "inline": false, "inlineVia": ""},
            ]),
            "every repo on the board, with its owner, whether or not a rule names it"
        );

        autorev::set_post_owner("a", autorev::Ran::Auto, autorev::Post::Hold);

        // the carve-out: the repo posts, the owner still holds, and both are listed
        autorev::set_post("a/b", autorev::Ran::Auto, autorev::Post::Now);
        assert_eq!(
            rules(),
            json!([
                {"target": "a/*", "manual": "post", "auto": "hold", "manualVia": "", "autoVia": "owner",
                 "inline": false, "inlineVia": "", "perRepo": false},
                {"target": "a/b", "manual": "post", "auto": "post", "manualVia": "", "autoVia": "repo",
                 "inline": false, "inlineVia": ""},
            ]),
            "a target set on one axis still lists the other"
        );

        // an axis this repo sets nothing on must read as what WILL happen, not as the default: the
        // table used to print "you post" here while a manual review on a/b would have held
        autorev::set_post_owner("a", autorev::Ran::Manual, autorev::Post::Hold);
        assert_eq!(
            rules()[1],
            json!({"target": "a/b", "manual": "hold", "auto": "post", "manualVia": "owner", "autoVia": "repo",
                   "inline": false, "inlineVia": ""}),
            "the owner's word, marked as inherited"
        );

        // a target on BOTH axes must appear once, an owner that sorts after a repo must still come
        // first (with `a/*` and `a/b` alone, alphabetical order happens to agree), and a ruled owner
        // with nothing on the board still shows up
        autorev::set_post("a/b", autorev::Ran::Manual, autorev::Post::Hold);
        autorev::set_post_owner("zeta", autorev::Ran::Manual, autorev::Post::Hold);
        assert_eq!(
            rules(),
            json!([
                {"target": "a/*", "manual": "hold", "auto": "hold", "manualVia": "owner", "autoVia": "owner",
                 "inline": false, "inlineVia": "", "perRepo": false},
                {"target": "zeta/*", "manual": "hold", "auto": "post", "manualVia": "owner", "autoVia": "",
                 "inline": false, "inlineVia": "", "perRepo": false},
                {"target": "a/b", "manual": "hold", "auto": "post", "manualVia": "repo", "autoVia": "repo",
                 "inline": false, "inlineVia": ""},
            ]),
            "owners first, then repos, and a/b listed once for both axes"
        );
    }

    /// #161: the panel's inline axis, end to end — the route writes the rule, the payload reads it
    /// back with where it came from, and the switch is what a target with no rule falls back to.
    #[test]
    fn the_inline_rule_goes_through_the_route_and_comes_back_in_the_payload() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| {
            c.autorev = d.path().join("autorev");
            c.held_dir = d.path().join("held");
            c.settings = None;
            c.inline = false;
        });
        let (base, token, _state) = served();
        let url = format!("{base}/api/posting");
        let row = |t: &str| {
            get(&format!("{base}/api/state"), Some(&token)).1["postingRules"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["target"] == t)
                .cloned()
                .unwrap()
        };

        // nothing set: the switch decides, and the payload says so with an empty via
        assert_eq!(row("a/b")["inline"], json!(false));
        assert_eq!(row("a/b")["inlineVia"], json!(""));

        // the owner's rule, through the route the panel uses
        assert_eq!(
            post(
                &url,
                json!({"op": "inline", "owner": "a", "inline": "on"}),
                &token
            )
            .0,
            200
        );
        assert_eq!(row("a/*")["inline"], json!(true));
        assert_eq!(row("a/*")["inlineVia"], json!("owner"));
        assert_eq!(row("a/b")["inline"], json!(true), "the repo follows it");
        assert_eq!(row("a/b")["inlineVia"], json!("owner"));

        // the repo carves itself out
        assert_eq!(
            post(
                &url,
                json!({"op": "inline", "repo": "a/b", "inline": "off"}),
                &token
            )
            .0,
            200
        );
        assert_eq!(row("a/b")["inline"], json!(false));
        assert_eq!(row("a/b")["inlineVia"], json!("repo"));
        assert_eq!(row("a/*")["inline"], json!(true), "and leaves its owner alone");

        // `none` drops the rule, so it follows again
        assert_eq!(
            post(
                &url,
                json!({"op": "inline", "repo": "a/b", "inline": "none"}),
                &token
            )
            .0,
            200
        );
        assert_eq!(row("a/b")["inlineVia"], json!("owner"));

        // ponytail: a target with no PR on the board. The panel's targets come from the posting rules
        // and the board, so a repo whose ONLY rule is an inline one was in force and nowhere on
        // screen until il.listed() was chained in — this is that, asserted.
        assert_eq!(
            post(
                &url,
                json!({"op": "inline", "repo": "zeta/quiet", "inline": "on"}),
                &token
            )
            .0,
            200
        );
        assert_eq!(row("zeta/quiet")["inline"], json!(true));
        assert_eq!(row("zeta/quiet")["inlineVia"], json!("repo"));
        assert_eq!(
            row("zeta/*")["inlineVia"],
            json!(""),
            "its owner is listed too, with no rule"
        );

        // an owner's rule comes off through the same route, and its repos follow the switch again
        assert_eq!(
            post(
                &url,
                json!({"op": "inline", "owner": "a", "inline": "none"}),
                &token
            )
            .0,
            200
        );
        assert_eq!(row("a/*")["inlineVia"], json!(""));
        assert_eq!(
            row("a/b")["inlineVia"],
            json!(""),
            "and the repo under it follows the switch"
        );
        assert_eq!(row("a/b")["inline"], json!(false), "which is off");

        // the words it will not take, and naming both or neither
        for bad in [
            json!({"op": "inline", "repo": "a/b", "inline": "maybe"}),
            json!({"op": "inline", "inline": "on"}),
            json!({"op": "inline", "repo": "a/b", "owner": "a", "inline": "on"}),
        ] {
            assert_eq!(post(&url, bad, &token).0, 400);
        }
    }

    /// The switch itself, which until #161 could be turned on by a flag and never off from the app.
    #[test]
    fn the_inline_switch_can_be_set_from_the_settings_route() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| {
            c.autorev = d.path().join("autorev");
            c.settings = None;
            c.inline = false;
        });
        let (base, token, _state) = served();
        let seen = || get(&format!("{base}/api/state"), Some(&token)).1["settings"]["inline"].clone();
        assert_eq!(seen(), json!(false));

        assert_eq!(
            post(&format!("{base}/api/settings"), json!({"inline": true}), &token).0,
            200
        );
        assert_eq!(seen(), json!(true));
        assert!(config::get().inline, "and the review path reads the same value");

        assert_eq!(
            post(&format!("{base}/api/settings"), json!({"inline": false}), &token).0,
            200
        );
        assert_eq!(seen(), json!(false));
        assert!(!config::get().inline);
    }

    /// The switch through HTTP changes what the payload lists: on, the owner decides; off, each repo keeps
    /// its word and the owner's rule stays as the fallback, marked per repo.
    #[test]
    fn the_owner_switch_through_the_route_changes_what_the_payload_lists() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| {
            c.autorev = d.path().join("autorev");
            c.held_dir = d.path().join("held");
            // ponytail: pinned, because the panel's inline column falls back to this switch. Left to
            // whatever the process config holds, the rows read differently depending on which test
            // ran before — the failure #160 was about, in a new place.
            c.inline = false;
        });
        let (base, token, _state) = served();
        let url = format!("{base}/api/posting");
        let rules = || get(&format!("{base}/api/state"), Some(&token)).1["postingRules"].clone();

        autorev::set_post("a/b", autorev::Ran::Auto, autorev::Post::Hold);
        assert_eq!(
            post(&url, json!({"op": "govern", "owner": "a", "on": true}), &token).0,
            200
        );
        assert_eq!(
            rules(),
            json!([
                {"target": "a/*", "manual": "post", "auto": "hold", "manualVia": "owner", "autoVia": "owner",
                 "inline": false, "inlineVia": "", "perRepo": false},
                {"target": "a/b", "manual": "post", "auto": "hold", "manualVia": "owner", "autoVia": "owner",
                 "inline": false, "inlineVia": ""},
            ]),
            "on: the owner holds what a/b held, and a/b follows it"
        );
        assert_eq!(
            post(&url, json!({"op": "govern", "owner": "a", "on": false}), &token).0,
            200
        );
        assert_eq!(
            rules(),
            json!([
                {"target": "a/*", "manual": "post", "auto": "hold", "manualVia": "owner", "autoVia": "owner",
                 "inline": false, "inlineVia": "", "perRepo": true},
                {"target": "a/b", "manual": "post", "auto": "hold", "manualVia": "repo", "autoVia": "repo",
                 "inline": false, "inlineVia": ""},
            ]),
            "off: a/b owns its words, and the owner keeps its rule for a repo nobody listed"
        );
    }

    /// The screen's three answers: what is in force, where it came from, and the OWNER's own word —
    /// the owner toggles flip that one, and flipping it from the effective value wrote back what was
    /// already there whenever a repo row had carved the owner out.
    #[test]
    fn the_posting_route_reports_the_owner_rule_as_well_as_the_effective_one() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| {
            c.autorev = d.path().join("autorev");
            c.held_dir = d.path().join("held");
            // ponytail: pinned, because the panel's inline column falls back to this switch. Left to
            // whatever the process config holds, the rows read differently depending on which test
            // ran before — the failure #160 was about, in a new place.
            c.inline = false;
        });
        let (base, token, _state) = served();
        let get_one = || {
            get(
                &format!("{base}/api/posting?repo=acme/api&number=7"),
                Some(&token),
            )
            .1
        };

        let j = get_one();
        assert_eq!(j["auto"]["value"], "post");
        assert_eq!(j["auto"]["via"], "");
        assert_eq!(j["auto"]["ownerValue"], "post");
        assert_eq!(j["held"], json!(null));

        post(
            &format!("{base}/api/posting"),
            json!({"owner": "acme", "ran": "auto", "post": "hold"}),
            &token,
        );
        let j = get_one();
        assert_eq!(j["auto"]["value"], "hold");
        assert_eq!(j["auto"]["via"], "owner");
        assert_eq!(j["auto"]["ownerValue"], "hold");

        // the carve-out: the repo says post, the owner still says hold, and `o` must see the latter
        post(
            &format!("{base}/api/posting"),
            json!({"repo": "acme/api", "ran": "auto", "post": "post"}),
            &token,
        );
        let j = get_one();
        assert_eq!(j["auto"]["value"], "post");
        assert_eq!(j["auto"]["via"], "repo");
        assert_eq!(
            j["auto"]["ownerValue"], "hold",
            "the owner's own word, not the effective one"
        );
        assert_eq!(
            j["manual"]["value"], "post",
            "the other kind is untouched throughout"
        );

        // owner control through the route: `on` is required, and a missing one is not a quiet "off"
        let (code, body) = post(
            &format!("{base}/api/posting"),
            json!({"op": "govern", "owner": "acme"}),
            &token,
        );
        assert_eq!(
            (code, body["error"].as_str()),
            (400, Some("govern needs on: true or on: false"))
        );
        let (code, body) = post(
            &format!("{base}/api/posting"),
            json!({"op": "govern", "owner": "acme/api", "on": true}),
            &token,
        );
        assert_eq!(
            (code, body["error"].as_str()),
            (400, Some("acme/api is not an owner"))
        );

        // and the rule comes off again, which is what the panel's owner toggle does
        post(
            &format!("{base}/api/posting"),
            json!({"owner": "acme", "ran": "auto", "post": "none"}),
            &token,
        );
        let j = get_one();
        assert_eq!(j["auto"]["ownerValue"], "post", "the owner rule is gone");
        assert_eq!(j["auto"]["value"], "post", "and the repo's own still stands");
        assert_eq!(j["auto"]["via"], "repo");
        post(
            &format!("{base}/api/posting"),
            json!({"repo": "acme/api", "ran": "auto", "post": "none"}),
            &token,
        );
        assert_eq!(get_one()["auto"]["via"], "", "nothing set anywhere now");
        let (code, body) = post(
            &format!("{base}/api/posting"),
            json!({"repo": "acme/api", "ran": "auto", "post": "maybe"}),
            &token,
        );
        assert_eq!(
            (code, body["error"].as_str()),
            (400, Some("post must be post, hold or none"))
        );

        // ponytail: one or the other, never both and never neither. The flag this replaced meant a body
        // naming the owner in `owner` -- the obvious reading -- set a rule on `repo`'s whole org instead.
        for (bad, said) in [
            (
                json!({"repo": "acme/api", "owner": "acme", "ran": "auto", "post": "hold"}),
                "name a repo or an owner, not both",
            ),
            (json!({"ran": "auto", "post": "hold"}), "name a repo or an owner"),
        ] {
            let (code, body) = post(&format!("{base}/api/posting"), bad.clone(), &token);
            assert_eq!((code, body["error"].as_str()), (400, Some(said)), "{bad}");
        }

        // the route folds the repo it is asked about: `A/B` and `a/b` are one repo, one rule
        autorev::set_post_owner("a", autorev::Ran::Manual, autorev::Post::Hold);
        autorev::set_post("a/b", autorev::Ran::Auto, autorev::Post::Hold);
        let lower = get(&format!("{base}/api/posting?repo=a/b&number=7"), Some(&token)).1;
        let mixed = get(&format!("{base}/api/posting?repo=A/B&number=7"), Some(&token)).1;
        assert_eq!(mixed["repo"], "a/b", "the folded key, not what was typed");
        assert_eq!(
            (&mixed["manual"], &mixed["auto"]),
            (&lower["manual"], &lower["auto"]),
            "and the same rule it resolves for a/b"
        );
        assert_eq!(mixed["auto"]["via"], "repo");
        assert_eq!(mixed["manual"]["via"], "owner");
    }

    /// Wait for whatever runs on a row to finish; a turn that never does fails the test instead of hanging it.
    fn settle(state: &State, url: &str) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while state.busy(url) {
            assert!(std::time::Instant::now() < until, "the turn never finished");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A held review discussed end to end, in demo mode so no model runs: the message is on disk before the
    /// answer, a revision waits beside the verdict, a release is refused until it is settled, and only an
    /// accept puts it where the post reads from.
    #[test]
    fn a_held_review_can_be_discussed_revised_and_the_revision_accepted() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| {
            c.demo = true;
            c.autorev = d.path().join("autorev");
            c.held_dir = d.path().join("held");
        });
        let (base, token, state) = served();
        let (repo, n) = (pr().repo().to_string(), pr().number);
        held::put(&held::Held {
            pr: pr(),
            model: "opus".into(),
            session: "5e3ae8e0-544e-4128-88af-fe301d354aae".into(),
            verdict: crate::types::Verdict {
                verdict: "request_changes".into(),
                instructions: "focus on auth".into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        let url = format!("{base}/api/posting");
        let act = |b: Value| post(&url, b, &token);
        let held_of = || get(&format!("{url}?repo={repo}&number={n}"), Some(&token)).1["held"].clone();
        let read = || held_of()["talk"].clone();

        let h = read();
        assert_eq!(
            (h["cannotDiscuss"].as_str(), h["busy"].as_bool()),
            (Some(""), Some(false))
        );
        assert_eq!(h["instructions"], "focus on auth", "shown to you, locally");

        let (code, body) = act(json!({"op": "discuss", "repo": repo, "number": n, "text": "  "}));
        assert_eq!((code, body["error"].as_str()), (400, Some("say something")));

        assert_eq!(
            act(json!({"op": "discuss", "repo": repo, "number": n, "text": "why is auth blocking?"})).0,
            200
        );
        settle(&state, &pr().url);
        let who: Vec<String> = read()["thread"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["who"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(who, ["you", "agent"]);

        let (code, body) = act(json!({"op": "accept", "repo": repo, "number": n}));
        assert_eq!(
            (code, body["error"].as_str()),
            (409, Some("no revision is waiting"))
        );

        assert_eq!(act(json!({"op": "revise", "repo": repo, "number": n})).0, 200);
        settle(&state, &pr().url);
        assert_eq!(read()["proposed"]["verdict"], "comment");
        assert_eq!(
            held_of()["verdict"],
            "request_changes",
            "not over the verdict until accepted"
        );

        let (code, body) = act(json!({"op": "release", "repo": repo, "number": n}));
        assert_eq!(
            (code, body["error"].as_str()),
            (409, Some("accept or keep the revision first"))
        );

        assert_eq!(act(json!({"op": "accept", "repo": repo, "number": n})).0, 200);
        assert_eq!(
            (held_of()["verdict"].as_str(), read()["proposed"].is_null()),
            (Some("comment"), true)
        );
        assert_eq!(
            held::get(&repo, n).unwrap().verdict.instructions,
            "focus on auth",
            "the accepted revision still says what it was asked"
        );

        // nothing changes, and nothing is dropped, while something runs on the row
        state.lock().running.insert(pr().url.clone());
        for op in ["discuss", "revise", "accept", "keep"] {
            let (code, _) = act(json!({"op": op, "repo": repo, "number": n, "text": "x"}));
            assert_eq!(code, 409, "{op}");
        }
        let (code, body) = act(json!({"op": "discard", "repo": repo, "number": n}));
        assert_eq!(
            (code, body["error"].as_str()),
            (409, Some("a review of this PR is already running"))
        );
        assert!(held::get(&repo, n).is_some());
        state.lock().running.remove(&pr().url);

        // a review that ran anywhere but the claude CLI says why instead of starting
        let mut other = held::get(&repo, n).unwrap();
        other.model = "openrouter:x-ai/grok-4".into();
        held::put(&other).unwrap();
        let (code, body) = act(json!({"op": "discuss", "repo": repo, "number": n, "text": "hi"}));
        assert_eq!(code, 409);
        assert!(body["error"].as_str().unwrap().contains("needs the claude CLI"));
    }

    /// A spell's result reaches the detail with its time and whether it quotes the spell, and posting one that
    /// does not exist is a 404.
    #[test]
    fn a_spell_result_is_shown_and_posted() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| {
            c.demo = true;
            c.self_dir = d.path().join("self");
            c.spells_dir = d.path().to_path_buf();
        });
        std::fs::write(
            d.path().join("auth-check.md"),
            "trace every request path to its auth check",
        )
        .unwrap();
        let (base, token, _state) = served();
        let result = review::spell_path(pr().repo(), pr().number, "auth-check");
        std::fs::create_dir_all(result.parent().unwrap()).unwrap();
        std::fs::write(&result, "I will trace every request path to its auth check").unwrap();
        let (code, d) = get(&format!("{base}/api/pr?url={}", pr().url), Some(&token));
        assert_eq!(code, 200);
        let s = &d["spells"][0];
        assert_eq!(
            (s["name"].as_str(), s["quotes"].as_bool()),
            (Some("auth-check"), Some(true))
        );
        assert!(s["at"].as_f64().unwrap() > 0.0);
        let url = format!("{base}/api/spell");
        assert_eq!(
            post(&url, json!({"url": pr().url, "name": "auth-check"}), &token).0,
            200
        );
        assert_eq!(
            post(&url, json!({"url": pr().url, "name": "test-gaps"}), &token).0,
            404
        );
    }

    /// A pre-review discussed the same way, in demo mode: the conversation lives beside the markdown, and
    /// an accepted revision rewrites the markdown without making it look newer than it is.
    #[test]
    fn a_pre_review_can_be_discussed_and_an_accepted_revision_keeps_its_old_time() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| {
            c.demo = true;
            c.autorev = d.path().join("autorev");
            c.held_dir = d.path().join("held");
            c.self_dir = d.path().join("self");
        });
        let (base, token, state) = served();
        std::fs::create_dir_all(d.path().join("self")).unwrap();
        let md = review::self_review_path(pr().repo(), pr().number);
        std::fs::write(&md, "# Pre-review\n\nthe original body\n").unwrap();
        let written = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::File::options()
            .write(true)
            .open(&md)
            .unwrap()
            .set_modified(written)
            .unwrap();
        review::put_self_talk(&held::Held {
            pr: pr(),
            model: "opus".into(),
            session: "5e3ae8e0-544e-4128-88af-fe301d354aae".into(),
            at: 1_700_000_000.0,
            verdict: crate::types::Verdict {
                verdict: "request_changes".into(),
                body: "the original body".into(),
                depth: "high".into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        let url = format!("{base}/api/prereview");
        let act = |b: Value| post(&url, b, &token);
        let read = || get(&format!("{url}?url={}", pr().url), Some(&token)).1["talk"].clone();

        assert_eq!(
            (read()["cannotDiscuss"].as_str(), read()["verdict"].as_str()),
            (Some(""), Some("request_changes"))
        );
        assert_eq!(
            act(json!({"url": pr().url, "op": "discuss", "text": "is this really blocking?"})).0,
            200
        );
        settle(&state, &pr().url);
        let who: Vec<String> = read()["thread"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["who"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(who, ["you", "agent"]);
        let body = std::fs::read_to_string(&md).unwrap();
        assert!(
            !body.contains("is this really blocking"),
            "the conversation never goes in the markdown"
        );

        assert_eq!(act(json!({"url": pr().url, "op": "revise"})).0, 200);
        settle(&state, &pr().url);
        assert_eq!(read()["proposed"]["verdict"], "comment");
        assert!(
            std::fs::read_to_string(&md)
                .unwrap()
                .contains("the original body"),
            "not until accepted"
        );

        assert_eq!(act(json!({"url": pr().url, "op": "accept"})).0, 200);
        let body = std::fs::read_to_string(&md).unwrap();
        assert!(
            body.contains("demo: the revised review") && !body.contains("the original body"),
            "{body}"
        );
        assert_eq!(
            std::fs::metadata(&md).unwrap().modified().unwrap(),
            written,
            "rewritten at its old time, so a push since it is not hidden"
        );
        assert!(read()["proposed"].is_null());
    }

    /// Accepting a pre-review's revision when its markdown cannot be written loses nothing: the revision
    /// still waits, and the markdown and the saved verdict still agree.
    #[test]
    #[cfg(unix)]
    fn a_pre_review_accept_that_cannot_write_the_markdown_keeps_the_revision() {
        use std::os::unix::fs::PermissionsExt;
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| {
            c.demo = true;
            c.autorev = d.path().join("autorev");
            c.held_dir = d.path().join("held");
            c.self_dir = d.path().join("self");
        });
        let (base, token, _state) = served();
        std::fs::create_dir_all(d.path().join("self")).unwrap();
        let md = review::self_review_path(pr().repo(), pr().number);
        std::fs::write(&md, "# Pre-review\n\nthe original body\n").unwrap();
        let verdict = |v: &str, body: &str| crate::types::Verdict {
            verdict: v.into(),
            body: body.into(),
            ..Default::default()
        };
        review::put_self_talk(&held::Held {
            pr: pr(),
            model: "opus".into(),
            session: "5e3ae8e0-544e-4128-88af-fe301d354aae".into(),
            verdict: verdict("request_changes", "the original body"),
            proposed: Some(verdict("comment", "the revision")),
            ..Default::default()
        })
        .unwrap();
        std::fs::set_permissions(&md, std::fs::Permissions::from_mode(0o444)).unwrap();
        // root writes a read-only file anyway, so there is nothing to test there
        if std::fs::OpenOptions::new().write(true).open(&md).is_ok() {
            std::fs::set_permissions(&md, std::fs::Permissions::from_mode(0o644)).unwrap();
            return;
        }

        let (code, _) = post(
            &format!("{base}/api/prereview"),
            json!({"url": pr().url, "op": "accept"}),
            &token,
        );
        std::fs::set_permissions(&md, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_ne!(code, 200, "the markdown could not be written");
        let saved = review::self_talk(pr().repo(), pr().number).unwrap();
        assert_eq!(
            saved.proposed.map(|v| v.body),
            Some("the revision".into()),
            "the revision still waits"
        );
        assert_eq!(
            saved.verdict.body, "the original body",
            "and the saved verdict still matches the markdown"
        );
    }

    #[test]
    fn a_pre_review_with_no_saved_conversation_says_to_run_it_again() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| c.self_dir = d.path().join("self"));
        let (base, token, _state) = served();
        let url = format!("{base}/api/prereview");
        let (code, body) = post(
            &url,
            json!({"url": pr().url, "op": "discuss", "text": "hi"}),
            &token,
        );
        assert_eq!(code, 409);
        assert!(body["error"].as_str().unwrap().contains("run it again"));
        let (code, body) = post(&url, json!({"url": pr().url, "op": "release"}), &token);
        assert_eq!(
            (code, body["error"].as_str()),
            (400, Some("op must be discuss, revise, accept or keep")),
            "a pre-review is never posted"
        );
    }

    #[test]
    fn instructions_past_the_cap_are_refused_before_anything_starts() {
        let _g = crate::config::test_lock();
        let (base, token, state) = served();
        let long = "x".repeat(ASK_MAX + 1);
        let (code, body) = post(
            &format!("{base}/api/review"),
            json!({"url": pr().url, "ask": long}),
            &token,
        );
        assert_eq!(
            (code, body["error"].as_str()),
            (400, Some("instructions are too long"))
        );
        assert!(!state.busy(&pr().url), "no review was started");
    }

    #[test]
    fn a_spell_is_cast_alone_and_refused_when_its_file_is_too_long() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("huge.md"), "x".repeat(ASK_MAX + 1)).unwrap();
        config::update(|c| c.spells_dir = d.path().to_path_buf());
        let (base, token, state) = served();
        let url = format!("{base}/api/review");
        let (code, body) = post(
            &url,
            json!({"url": pr().url, "spell": "huge", "self": true}),
            &token,
        );
        assert_eq!(
            (code, body["error"].as_str()),
            (400, Some("a spell is cast on its own"))
        );
        let (code, body) = post(&url, json!({"url": pr().url, "spell": "huge"}), &token);
        assert_eq!(code, 400);
        assert!(
            body["error"].as_str().unwrap().contains("huge.md"),
            "the error names the file"
        );
        assert!(!state.busy(&pr().url), "no review was started");
    }

    /// A release that could not start must say so. It answered ok, the flash said "posting…", and
    /// nothing went up.
    #[test]
    fn a_release_that_cannot_start_is_a_409() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| {
            c.demo = true;
            c.autorev = d.path().join("autorev");
            c.held_dir = d.path().join("held");
        });
        let (base, token, state) = served();
        held::put(&held::Held {
            pr: pr(),
            model: "opus".into(),
            verdict: crate::types::Verdict {
                verdict: "approve".into(),
                ..Default::default()
            },
            hello: String::new(),
            at: 100.0,
            ..Default::default()
        })
        .unwrap();
        // the row is marked from the store, by name
        let j = get(&format!("{base}/api/state"), Some(&token)).1;
        let waiting: Vec<bool> = j["sections"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|s| s["prs"].as_array().unwrap())
            .map(|p| p["waiting"].as_bool().unwrap())
            .collect();
        assert_eq!(waiting, [true], "the row says a review is waiting");

        state.lock().running.insert(pr().url.clone());
        let (code, body) = post(
            &format!("{base}/api/posting"),
            json!({"op": "release", "repo": pr().repo(), "number": pr().number}),
            &token,
        );
        assert_eq!(
            (code, body["error"].as_str()),
            (409, Some("a review of this PR is already running"))
        );
        assert!(held::get(pr().repo(), pr().number).is_some(), "still waiting");

        let (code, body) = post(
            &format!("{base}/api/posting"),
            json!({"op": "release", "repo": "other/thing", "number": 1}),
            &token,
        );
        assert_eq!(
            (code, body["error"].as_str()),
            (404, Some("nothing waiting for that PR"))
        );
    }

    /// bind::key lowercases, so the filenames are folded and a raw nameWithOwner never matched.
    /// Every other fixture here is lowercase, so no test caught it.
    #[test]
    fn a_mixed_case_repo_still_marks_its_row_as_waiting() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| {
            c.demo = true;
            c.autorev = d.path().join("autorev");
            c.held_dir = d.path().join("held");
        });
        let (base, token, state) = served();
        let mut mixed = pr();
        mixed.repository.name_with_owner = "MartinRovang/git-dashy".into();
        mixed.url = "https://x/MartinRovang/git-dashy/7".into();
        state.lock().sections = vec![Section {
            name: "REVIEW REQUESTED".into(),
            prs: Some(vec![mixed.clone()]),
            err: None,
        }];
        held::put(&held::Held {
            pr: mixed.clone(),
            model: "opus".into(),
            verdict: crate::types::Verdict {
                verdict: "approve".into(),
                ..Default::default()
            },
            hello: String::new(),
            at: 100.0,
            ..Default::default()
        })
        .unwrap();
        let j = get(&format!("{base}/api/state"), Some(&token)).1;
        assert_eq!(j["sections"][0]["prs"][0]["waiting"], json!(true));
    }

    /// The warning on the waiting screen: only two heads we can both read and that differ.
    #[test]
    fn the_waiting_screen_says_when_the_head_has_moved() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| {
            c.demo = true;
            c.autorev = d.path().join("autorev");
            c.held_dir = d.path().join("held");
        });
        let (base, token, state) = served();
        let ask = |head: &str| {
            let mut live = pr();
            live.head = head.into();
            state.lock().sections = vec![Section {
                name: "REVIEW REQUESTED".into(),
                prs: Some(vec![live]),
                err: None,
            }];
            get(
                &format!("{base}/api/posting?repo={}&number={}", pr().repo(), pr().number),
                Some(&token),
            )
            .1["held"]["moved"]
                .clone()
        };
        let mut held_pr = pr();
        held_pr.head = "aaa".into();
        held::put(&held::Held {
            pr: held_pr,
            model: "opus".into(),
            verdict: crate::types::Verdict {
                verdict: "approve".into(),
                ..Default::default()
            },
            hello: String::new(),
            at: 100.0,
            ..Default::default()
        })
        .unwrap();

        assert_eq!(ask("bbb"), json!(true), "two heads we can read, and they differ");
        assert_eq!(ask("aaa"), json!(false), "the same head is not a move");
        assert_eq!(
            ask(""),
            json!(false),
            "a head we cannot read is not a move either"
        );
    }

    /// Dropping a held review must clear the STATUS as well as the file. The hold wrote its verdict
    /// into the row, so removing only the file left it reading "changes requested (waiting to post)"
    /// with nothing waiting, the menu calling it Reviewed, and auto skipping the PR for good.
    #[test]
    fn discarding_a_held_review_leaves_the_row_clean() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| {
            c.demo = true;
            c.autorev = d.path().join("autorev");
            c.held_dir = d.path().join("held");
        });
        let (base, token, state) = served();
        held::put(&held::Held {
            pr: pr(),
            model: "opus".into(),
            verdict: crate::types::Verdict {
                verdict: "request_changes".into(),
                ..Default::default()
            },
            hello: String::new(),
            at: 100.0,
            ..Default::default()
        })
        .unwrap();
        // the hold path puts its verdict on the row, the way finish() does
        state
            .lock()
            .reviews
            .insert(pr().url.clone(), "✗ changes requested (waiting to post)".into());

        let row = |base: &str| -> Value {
            get(&format!("{base}/api/state"), Some(token.as_str())).1["sections"][0]["prs"][0].clone()
        };
        assert_eq!(row(&base)["waiting"], json!(true));

        let (code, _) = post(
            &format!("{base}/api/posting"),
            json!({"op": "discard", "repo": pr().repo(), "number": pr().number}),
            &token,
        );
        assert_eq!(code, 200);
        let r = row(&base);
        assert_eq!(r["waiting"], json!(false), "nothing is waiting any more");
        assert_eq!(
            r["review"], "",
            "and the row does not claim a verdict nobody posted"
        );
        assert!(
            !state.lock().reviews.contains_key(&pr().url),
            "so auto can reach it again"
        );
    }

    #[test]
    fn unknown_pr_is_a_404_json() {
        let (base, token, _state) = served();
        let (code, d) = get(&format!("{base}/api/pr?url=nope"), Some(&token));
        assert_eq!(code, 404);
        assert_eq!(d["error"], "no such pr");
        let (code, d) = post(&format!("{base}/api/open"), json!({"url": "/etc/passwd"}), &token);
        assert_eq!((code, d["error"].as_str()), (404, Some("no such pr")));
    }

    #[test]
    fn a_bad_body_is_a_400() {
        let (base, token, _state) = served();
        let mut resp = agent()
            .post(format!("{base}/api/auto"))
            .header("X-Dashy-Token", &token)
            .send("[1,2]")
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400);
        assert!(resp.body_mut().read_to_string().unwrap().contains("bad body"));
        assert_eq!(post(&format!("{base}/api/nope"), json!({}), &token).0, 404);
    }

    /// The fold panel and the keypress must not disagree: `pair` answering "1, origin unknown" while
    /// `merge` folds to 2 and promotes is the one wrong answer here that writes to the team pool.
    #[test]
    fn the_fold_panel_counts_what_merge_would_actually_do() {
        let _guard = crate::config::test_lock();
        let dir = tempfile::tempdir().unwrap();
        config::update(|c| c.memory_dir = dir.path().to_path_buf());
        let queue = memory::queue_path(None);
        std::fs::create_dir_all(queue.parent().unwrap()).unwrap();
        // two drafts from two genuinely independent review runs
        std::fs::write(
            &queue,
            "- (1) [r:aaaa] the retry loop never backs off\n- (1) [r:bbbb] retries fire with no backoff\n",
        )
        .unwrap();

        let panel = pair(
            None,
            "the retry loop never backs off",
            "retries fire with no backoff",
        )
        .expect("both rows are live");
        let would = panel["would"].as_u64().unwrap() as u32;
        assert_eq!(would, 2, "two run ids, so two observations: {panel}");
        assert!(panel["promotes"].as_bool().unwrap());

        let merged = memory::merge(
            None,
            "the retry loop never backs off",
            "retries fire with no backoff",
        );
        assert_eq!(would, merged, "the panel promised {would}, the fold did {merged}");
    }

    /// ponytail: a theme the stylesheet has no rule for renders :root and looks like nothing happened.
    /// "dashy" IS :root, so it is the one name that needs no body[data-theme=...] block.
    #[test]
    fn every_theme_but_the_root_one_has_a_rule_in_the_stylesheet() {
        let css: String = DIST
            .get_dir("assets")
            .expect("built assets")
            .files()
            .filter(|f| f.path().extension().is_some_and(|e| e == "css"))
            .map(|f| String::from_utf8_lossy(f.contents()).into_owned())
            .collect();
        assert!(!css.is_empty(), "no stylesheet in the bundle");
        for t in config::THEMES.iter().filter(|t| **t != "dashy") {
            // the bundler drops the quotes: [data-theme=pencil], not [data-theme="pencil"]
            assert!(
                css.contains(&format!("data-theme={t}")) || css.contains(&format!(r#"data-theme="{t}""#)),
                "theme {t} has no rule in the stylesheet"
            );
        }
    }

    #[test]
    fn settings_change_the_theme_and_persist() {
        let _guard = crate::config::test_lock();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        config::update(|c| {
            c.settings = Some(file.clone());
            c.theme = "dashy".into();
            c.interval = 300;
        });
        let (base, token, state) = served();
        let (code, d) = post(
            &format!("{base}/api/settings"),
            json!({"theme": "nord", "interval": 60, "voice": ["bot", "review"]}),
            &token,
        );
        assert_eq!((code, d["ok"].as_bool()), (200, Some(true)));
        let c = config::get();
        assert_eq!((c.theme.as_str(), c.interval), ("nord", 60));
        assert_eq!(c.voice, vec!["review", "bot"]); // ponytail: rebuilt in option order
        assert!(state.lock().wake.is_set()); // a shorter interval must not wait out the longer one
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(
            (saved["theme"].as_str(), saved["interval"].as_u64()),
            (Some("nord"), Some(60))
        );
        // junk off the wire never lands
        for body in [
            json!({"interval": 0}),
            json!({"interval": "soon"}),
            json!({"model": ""}),
            json!({"theme": "neon"}),
            json!({"voice": []}),
            json!({"window": 5}),
            json!({"scopes": ["bogus"]}),
            json!({"scopes": ["org:x", 3]}),
            json!({"scopes": vec!["org:x"; 51]}),
            json!({"scopes": [format!("org:{}", "x".repeat(97))]}),
            json!({"read": {"u": 1}}),
            json!({"spells": ["no-such-spell"]}),
            json!({"spells": "auth-check"}),
            json!({"hidden": {"u": 1}}),
            json!({"read": {"x".repeat(513): "t"}}),
            json!({"read": (0..5001).map(|i| (i.to_string(), json!("t"))).collect::<Map<_, _>>()}),
        ] {
            assert_eq!(post(&format!("{base}/api/settings"), body, &token).0, 400);
        }
        assert_eq!(config::get().theme, "nord");
        // posts at once: none undoes another's change, and the file always parses
        config::update(|c| c.model = "before".into());
        std::thread::scope(|sc| {
            for i in 0..8 {
                let (base, token) = (&base, &token);
                sc.spawn(move || {
                    let body = if i % 2 == 0 {
                        json!({"read": {format!("u{i}"): "t"}})
                    } else {
                        json!({"model": format!("m{i}")})
                    };
                    assert_eq!(post(&format!("{base}/api/settings"), body, token).0, 200);
                });
            }
        });
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(
            (
                saved["theme"].as_str(),
                saved["read"].as_object().map(|m| m.len())
            ),
            (Some("nord"), Some(1))
        );
        assert!(saved["model"].as_str().is_some_and(|m| m.starts_with('m'))); // not reverted by a read post
        assert_eq!(
            post(
                &format!("{base}/api/settings"),
                json!({"scopes": ["team:k"]}),
                &token
            )
            .0,
            200
        );
        assert_eq!(config::get().scopes, ["team:k"]);
        assert_eq!(
            post(
                &format!("{base}/api/settings"),
                json!({"read": {"https://x/1": "t1"}, "hidden": {"https://x/2": "t2"}}),
                &token
            )
            .0,
            200
        );
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(
            (
                saved["read"]["https://x/1"].as_str(),
                saved["hidden"]["https://x/2"].as_str(),
                saved["scopes"][0].as_str()
            ),
            (Some("t1"), Some("t2"), Some("team:k"))
        );
        assert_eq!(
            post(&format!("{base}/api/settings"), json!({"read": {"u": 3}}), &token).0,
            400
        );
        let d = get(&format!("{base}/api/state"), Some(&token)).1;
        assert_eq!(d["settings"]["theme"], "nord");
        assert_eq!(d["settings"]["hidden"]["https://x/2"], "t2");
        // The welcome hint is remembered HERE, not in the webview: its origin is a new random port
        // every launch, so a localStorage flag would show the hint again on every open.
        assert_eq!(d["settings"]["hinted"], json!(false));
        post(&format!("{base}/api/settings"), json!({"hinted": true}), &token);
        let d = get(&format!("{base}/api/state"), Some(&token)).1;
        assert_eq!(d["settings"]["hinted"], json!(true));
        assert_eq!(d["options"]["interval"], json!(config::INTERVALS));
        // the key hints are on out of the box, and the switch is remembered the same way
        assert_eq!(d["settings"]["keyhints"], json!(true));
        post(
            &format!("{base}/api/settings"),
            json!({"keyhints": false}),
            &token,
        );
        let d = get(&format!("{base}/api/state"), Some(&token)).1;
        assert_eq!(d["settings"]["keyhints"], json!(false));
        // the PR pane's layout is remembered the same way
        post(
            &format!("{base}/api/settings"),
            json!({"pane": {"off": ["checks"]}}),
            &token,
        );
        let d = get(&format!("{base}/api/state"), Some(&token)).1;
        assert_eq!(d["settings"]["pane"]["off"], json!(["checks"]));
        post(
            &format!("{base}/api/settings"),
            json!({"side": {"order": ["view", "agent"]}}),
            &token,
        );
        let d = get(&format!("{base}/api/state"), Some(&token)).1;
        assert_eq!(d["settings"]["side"]["order"], json!(["view", "agent"]));
        // a malformed layout is refused whole and leaves the saved one alone
        let long: Vec<String> = (0..21).map(|i| i.to_string()).collect();
        for bad in [json!(["x"]), json!({"off": "x"}), json!({"off": long})] {
            assert_eq!(
                post(&format!("{base}/api/settings"), json!({"side": bad}), &token).0,
                400
            );
        }
        let d = get(&format!("{base}/api/state"), Some(&token)).1;
        assert_eq!(d["settings"]["side"]["order"], json!(["view", "agent"]));
    }

    #[test]
    fn a_report_route_answers_only_start_and_open_and_open_needs_a_report() {
        let _guard = crate::config::test_lock();
        let dir = tempfile::tempdir().unwrap();
        config::update(|c| c.reports = dir.path().to_path_buf());
        let (base, token, _state) = served();
        assert_eq!(
            post(&format!("{base}/api/report"), json!({"op": "delete"}), &token).0,
            400
        );
        assert_eq!(
            post(&format!("{base}/api/report"), json!({"op": "open"}), &token).0,
            404
        );
        let d = get(&format!("{base}/api/state"), Some(&token)).1;
        assert_eq!(d["knowledge"]["report"]["latest"], Value::Null);
        config::update(|c| c.reports = config::Config::default().reports);
    }

    #[test]
    fn closing_the_changelog_clears_it_and_records_the_version() {
        let _guard = crate::config::test_lock();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        config::update(|c| c.settings = Some(file.clone()));
        let (base, token, state) = served();
        state.lock().changelog = "v9.9.9\n\nnotes".into();
        assert_eq!(
            get(&format!("{base}/api/state"), Some(&token)).1["changelog"],
            "v9.9.9\n\nnotes"
        );
        assert_eq!(post(&format!("{base}/api/changelog"), json!({}), &token).0, 200);
        assert_eq!(state.lock().changelog, "");
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(saved["seen"], config::VERSION);
        // a dismiss waits for a settings write in progress, so neither save undoes the other
        config::update(|c| c.seen = String::new());
        let held = config::SAVING.lock().unwrap_or_else(|e| e.into_inner());
        std::thread::scope(|sc| {
            let dismiss = sc.spawn(|| post(&format!("{base}/api/changelog"), json!({}), &token));
            std::thread::sleep(std::time::Duration::from_millis(200));
            assert_eq!(
                config::get().seen,
                "",
                "saved while a settings write held the lock"
            );
            drop(held);
            assert_eq!(dismiss.join().unwrap().0, 200);
        });
        assert_eq!(config::get().seen, config::VERSION);
        config::update(|c| {
            c.settings = None;
            c.seen = String::new();
        });
    }

    #[test]
    fn consent_answers_one_ask_and_drops_it() {
        let (base, token, state) = served();
        state.lock().asks = vec![
            json!({"kind": "publishing", "key": "t1"}),
            json!({"kind": "agents", "key": "t1", "text": "do x"}),
        ];
        post(
            &format!("{base}/api/consent"),
            json!({"kind": "publishing", "key": "t1", "yes": false}),
            &token,
        );
        let d = get(&format!("{base}/api/state"), Some(&token)).1;
        assert_eq!(
            d["asks"],
            json!([{"kind": "agents", "key": "t1", "text": "do x"}])
        );
        assert_eq!(
            post(
                &format!("{base}/api/consent"),
                json!({"kind": "odd", "key": "t1"}),
                &token
            )
            .0,
            400
        );
    }

    #[test]
    fn code_rows_keep_every_mark_on_a_line_or_as_an_orphan() {
        let line = |n: u32, sign: &str, text: &str| Line {
            n: Some(n),
            sign: sign.into(),
            text: text.into(),
            del: None,
            marks: vec![],
        };
        let files = vec![DiffFile {
            path: "x.py".into(),
            add: 1,
            dele: 0,
            hunks: vec![Hunk {
                header: "@@ -1,2 +1,3 @@".into(),
                start: 1,
                lines: vec![line(1, " ", "a"), line(2, "+", "b"), line(3, " ", "c")],
            }],
        }];
        let marks = vec![
            Mark {
                kind: "note".into(),
                loc: "x.py:2".into(),
                text: "on b".into(),
                path: "x.py".into(),
                n: 2,
                file: 0,
                on_line: true,
            },
            Mark {
                kind: "nit".into(),
                loc: "y.py:1".into(),
                text: "elsewhere".into(),
                path: "y.py".into(),
                n: 1,
                file: usize::MAX,
                on_line: false,
            },
        ];
        let kinds: Vec<&str> = code_rows(&files, &marks)
            .iter()
            .map(|r| r["kind"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
            .leak()
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(
            kinds,
            ["file", "hunk", "line", "line", "note", "line", "gap", "orphan"]
        );
        let rows = code_rows(&files, &marks);
        assert_eq!(rows[7]["why"], "not in this diff");
    }

    #[test]
    fn applying_a_dream_leaves_the_team_checkout_alone() {
        // The apply used to pull and push every joined team around a write that cannot reach one.
        // push_dir runs `git add -A`, so a teammate's unrelated working-tree state was committed under
        // "memory: dream cleanup" — a commit nobody asked for, in a repo the dream never wrote to.
        let _g = crate::config::test_lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        crate::config::update(|c| {
            c.memory_dir = root.join("mine");
            c.teams = root.join("teams");
            c.bindings = root.join("bindings");
            c.backups = root.join("backups");
        });
        std::fs::create_dir_all(root.join("mine")).unwrap();
        std::fs::write(root.join("mine").join("general.md"), "- mine\n").unwrap();
        let team_dir = root.join("teams").join("org-t");
        std::fs::create_dir_all(team_dir.join("memory")).unwrap();
        assert!(crate::team::init_history(&team_dir)); // a real checkout, so a commit would show
        std::fs::write(team_dir.join("memory").join("general.md"), "- theirs\n").unwrap();
        let commits = |d: &std::path::Path| {
            std::process::Command::new("git")
                .args(["-C", &d.to_string_lossy(), "log", "--oneline"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
                .unwrap_or(0)
        };
        let before = commits(&team_dir);

        let state = State::new();
        start_job("dream", move || {
            Ok(dream_result((
                "tidy".into(),
                vec![("mine/general.md".into(), "- mine\n".into())],
                vec![("mine/general.md".into(), "- mine, tidied\n".into())],
            )))
        });
        for _ in 0..200 {
            if job_of("dream").is_some_and(|j| !j.lock().unwrap().running) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let body: Body = serde_json::from_value(json!({"op": "apply"})).unwrap();
        post_dream(&state, &body).unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("mine").join("general.md")).unwrap(),
            "- mine, tidied\n"
        );
        assert_eq!(
            commits(&team_dir),
            before,
            "the dream committed in a team checkout"
        );
        // and the teammate's file is still sitting there uncommitted, which is theirs to deal with
        assert_eq!(
            std::fs::read_to_string(team_dir.join("memory").join("general.md")).unwrap(),
            "- theirs\n"
        );
    }

    #[test]
    fn the_again_op_clears_the_answer_and_hands_back_the_ask() {
        // Every other test calls memory::ask_*_again directly, so a typo in this match or a missing
        // refresh of state.asks would pass. This is the route the knowledge-card row actually takes.
        let _g = crate::config::test_lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        crate::config::update(|c| {
            c.memory_dir = root.join("mine");
            c.teams = root.join("teams");
            c.bindings = root.join("bindings");
        });
        std::fs::create_dir_all(root.join("mine")).unwrap();
        std::fs::create_dir_all(root.join("teams").join("org-t").join(".git")).unwrap();
        std::fs::create_dir_all(root.join("teams").join("org-t").join("memory")).unwrap();
        memory::allow_publishing("org-t", false);
        assert!(memory::unasked().is_empty());

        let state = State::new();
        let body: Body =
            serde_json::from_value(json!({"op": "again", "kind": "publishing", "key": "org-t"})).unwrap();
        let out = post_consent(&state, &body).unwrap();
        let keys: Vec<&str> = out["asks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, ["org-t"]); // the ask is back, and the page is handed it
        assert_eq!(state.lock().asks.len(), 1); // and the server's own copy agrees
        assert_eq!(memory::unasked().len(), 1);

        let bad: Body =
            serde_json::from_value(json!({"op": "again", "kind": "nonsense", "key": "org-t"})).unwrap();
        assert_eq!(post_consent(&state, &bad).unwrap_err().0, 400);
    }

    /// `f` is the only thing that clears a read that failed: without this call a detail the network
    /// dropped sat in the cache as "still loading" until the PR itself moved.
    #[test]
    fn a_refresh_clears_the_reads_that_failed() {
        let state = State::new();
        {
            let mut inner = state.lock();
            inner.details.insert(("failed".into(), "1".into()), None);
            inner.details.insert(
                ("landed".into(), "1".into()),
                Some(crate::types::Detail::default()),
            );
        }
        let body: Body = serde_json::from_value(json!({})).unwrap();
        post_refresh(&state, &body).unwrap();
        let inner = state.lock();
        assert_eq!(inner.details.len(), 1, "the failed read is gone");
        assert!(inner
            .details
            .contains_key(&("landed".to_string(), "1".to_string())));
    }

    #[test]
    fn dream_result_promises_only_what_the_apply_will_do() {
        // The rows, the deletion count and the full diff all came off the raw answer, so the page
        // could show a team file losing nine lines and then not touch it. `v` is the view somebody
        // opens because they want to be careful, which makes it the worst place to say that.
        let before = vec![
            ("mine/general.md".to_string(), "- mine\n".to_string()),
            (
                "team:org-t/general.md".to_string(),
                (0..9).map(|i| format!("- theirs {i}\n")).collect::<String>(),
            ),
        ];
        let new = vec![
            ("mine/general.md".to_string(), "- mine, tidied\n".to_string()),
            ("team:org-t/general.md".to_string(), String::new()),
        ];
        let r = dream_result(("tidy".into(), before, new));
        let names: Vec<&str> = r["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["mine/general"]);
        assert_eq!(r["lost"].as_u64(), Some(0)); // the team's emptied file is not a deletion
        assert_eq!(r["theirs"].as_u64(), Some(1)); // and the page says it was read and left alone
        let detail = r["detail"].as_str().unwrap();
        assert!(detail.contains("mine, tidied"), "{detail}");
        assert!(!detail.contains("theirs"), "{detail}");
        assert!(!r["new"]
            .as_object()
            .unwrap()
            .contains_key("team:org-t/general.md"));
    }

    #[test]
    fn dream_result_counts_what_is_lost_and_hides_the_bodies_from_the_page() {
        let before = vec![
            ("mine/a.md".to_string(), "x\ny\n".to_string()),
            ("mine/general.md".to_string(), "g\n".to_string()),
        ];
        let new = vec![
            ("mine/a.md".to_string(), "x\n".to_string()),
            ("mine/general.md".to_string(), String::new()),
        ];
        let r = dream_result(("tidy".into(), before, new));
        assert_eq!(
            (r["summary"].as_str(), r["lost"].as_u64()),
            (Some("tidy"), Some(1))
        );
        let files: Vec<(String, bool)> = r["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["name"].as_str().unwrap().into(),
                    f["deleted"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            files,
            [("mine/general".to_string(), true), ("mine/a".to_string(), false)]
        );
        assert!(r["detail"].as_str().unwrap().contains("-y"));
        assert!(r["new"].is_object());
        assert_eq!(
            dream_detail(
                "s",
                &[("a.md".into(), "x\n".into())],
                &[("a.md".into(), "x\n".into())]
            )
            .trim(),
            "s"
        );
    }

    #[test]
    fn jobs_run_once_at_a_time_per_name_and_report_errors() {
        start_job("test-fail", || anyhow::bail!("boom\nlast line"));
        for _ in 0..200 {
            if !job("test-fail")["running"].as_bool().unwrap() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let j = job("test-fail");
        assert_eq!(
            (j["running"].as_bool(), j["error"].as_str()),
            (Some(false), Some("last line"))
        );
        assert_eq!(job("never")["idle"], true);
        start_job("test-ok", || Ok(json!({"n": 1})));
        for _ in 0..200 {
            if !job("test-ok")["running"].as_bool().unwrap() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(job("test-ok")["result"]["n"], 1);
    }

    #[test]
    fn of_many_starts_at_once_exactly_one_runs() {
        // the help route relies on this answer: a start that did not happen is a 409, not an ok
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let rx = Arc::new(Mutex::new(rx));
        let started: Vec<bool> = (0..8)
            .map(|_| {
                let rx = rx.clone();
                std::thread::spawn(move || {
                    start_job("test-race", move || {
                        let _ = rx.lock().unwrap().recv_timeout(Duration::from_secs(10));
                        Ok(json!({}))
                    })
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect();
        assert_eq!(started.iter().filter(|s| **s).count(), 1, "{started:?}");
        tx.send(()).unwrap();
    }

    /// The catch_unwind in start_job only does anything while the release profile unwinds: under
    /// `panic = "abort"` this passed in dev and the shipped binary took the whole app down instead.
    #[test]
    fn a_panicking_job_leaves_the_panel_readable_instead_of_killing_the_app() {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // ponytail: the panic is the point, keep it off the log
        start_job("test-panic", || panic!("the dream went wrong"));
        for _ in 0..200 {
            if !job("test-panic")["running"].as_bool().unwrap() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        std::panic::set_hook(hook);
        let j = job("test-panic");
        assert_eq!(j["running"].as_bool(), Some(false));
        assert_eq!(j["error"].as_str(), Some("the dream went wrong"));
    }

    #[test]
    fn hosts_and_queries_parse_like_python() {
        assert_eq!(host_of("127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(host_of("localhost"), "localhost");
        assert_eq!(host_of("[::1]:80"), "::1");
        assert_eq!(host_of("evil.example.com:80"), "evil.example.com");
        let qs = parse_query("url=https%3A%2F%2Fx%2F1&scope=marks&a=b+c");
        assert_eq!(
            (q(&qs, "url"), q(&qs, "scope"), q(&qs, "a"), q(&qs, "none")),
            ("https://x/1", "marks", "b c", "")
        );
    }
}
