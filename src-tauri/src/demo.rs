//! --demo: fake PRs, reviews and memory so the dashboard can be shown without a token. Port of dashy/demo.py.
//! ponytail: no monkey-patching. Modules check `config::get().demo` at the points Python swapped, and
//! this file holds the fixtures and the temp dirs the demo points config at.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Duration, SecondsFormat, Utc};

use crate::config;
use crate::types::{Check, Detail, Finding, LogEntry, Login, Pr, Repository, Section, Verdict};

/// One canned PR row. `hours` is how long ago it was updated.
pub fn pr(n: u64, title: &str, repo: &str, author: &str, hours: f64, draft: bool) -> Pr {
    pr_at(n, title, repo, author, hours, draft, now())
}

fn pr_at(n: u64, title: &str, repo: &str, author: &str, hours: f64, draft: bool, now: DateTime<Utc>) -> Pr {
    let when = now - Duration::seconds((hours * 3600.0) as i64);
    Pr {
        number: n,
        title: title.into(),
        url: format!("https://github.com/{repo}/pull/{n}"),
        is_draft: draft,
        // a wide spread of diff sizes, so the graph's node sizes visibly differ
        additions: Some([6, 30, 90, 240, 600, 1400, 3200][n as usize % 7]),
        deletions: Some([2, 12, 40, 110, 300, 700, 1500][n as usize % 7]),
        repository: Repository {
            name_with_owner: repo.into(),
            name: repo.split('/').nth(1).unwrap_or("").into(),
        },
        author: Some(Login { login: author.into() }),
        author_association: "MEMBER".into(),
        updated_at: when.to_rfc3339_opts(SecondsFormat::Secs, false),
        ..Default::default()
    }
}

/// The instant the demo was installed; every age is relative to it, as Python's `now` was.
fn now() -> DateTime<Utc> {
    static NOW: OnceLock<DateTime<Utc>> = OnceLock::new();
    *NOW.get_or_init(Utc::now)
}

/// The rows the fake fetch serves. MINE is behind a lock because a demo `+` edits its reviewers.
struct Fixtures {
    mine: Mutex<Vec<Pr>>,
    rr: Vec<Pr>,
    late: Pr,
    assigned: Vec<Pr>,
    late_assigned: Pr,
    /// (row, verdict) seeded into the log, oldest first.
    seed: Vec<(Pr, Verdict)>,
}

fn fixtures() -> &'static Fixtures {
    static F: OnceLock<Fixtures> = OnceLock::new();
    F.get_or_init(|| {
        let now = now();
        let mut m1 = pr_at(101, "feat: add retry to webhook client", "acme/api", "alice", 2.0, false, now);
        m1.status = "· awaiting review".into();
        m1.reviewers = "✓bob ·carol".into();
        m1.checks = "✓".into();
        // long title: overflows most terminals, shows the marquee
        let m2 = pr_at(
            98,
            "WIP: migrate to pydantic v2 and drop the hand-rolled validators in the ingest and export paths",
            "acme/api",
            "alice",
            30.0,
            true,
            now,
        );
        // a comment on someone else's PR: visible to everyone, not just erin
        let mut r1 = pr_at(212, "Fix off-by-one in pagination", "acme/web", "bob", 1.0, false, now);
        r1.checks = "✗".into();
        r1.reviewers = "~erin ·me".into();
        let mut r2 = pr_at(207, "perf: cache user lookups in session middleware", "acme/web", "carol", 5.0, false, now);
        r2.checks = "●".into();
        let r3 = pr_at(55, "chore(infra)!: rotate signing keys and bump KMS alias", "acme/infra", "dave", 48.0, false, now);
        let finding = |kind: &str, loc: &str, text: &str| {
            serde_json::json!({"kind": kind, "loc": loc, "text": text})
        };
        let seed = vec![
            // older review, folds under the newer one
            (
                pr_at(180, "Refactor auth middleware", "acme/api", "frank", 26.0, false, now),
                Verdict {
                    verdict: "request_changes".into(),
                    summary: "Splits auth middleware into token parsing and policy checks.".into(),
                    body: "- `api/auth.py:40` policy check runs before the token is validated".into(),
                    kind: "refactor".into(),
                    ..Default::default()
                },
            ),
            (
                pr_at(180, "Refactor auth middleware", "acme/api", "frank", 3.0, false, now),
                Verdict {
                    verdict: "approve".into(),
                    summary: "Splits auth middleware into token parsing and policy checks.".into(),
                    body: "LGTM. Clean split, existing tests still cover both paths.".into(),
                    kind: "refactor".into(),
                    breaking: true,
                    findings: vec![
                        finding("note", "api/auth.py:40", "policy.check now runs on the parsed user; the old order is gone"),
                        finding("nit", "api/handlers.py:12", "the retry helper is unused after this change"),
                    ],
                    db: Some(serde_json::json!({
                        "tables": [
                            {"name": "sessions", "change": "altered", "refs": ["users"], "columns": [
                                {"name": "policy_version", "change": "added", "note": "int NOT NULL, no default"},
                                {"name": "token_hash", "change": "read", "note": "text"},
                            ]},
                            {"name": "users", "change": "read", "columns": [
                                {"name": "role", "change": "read", "note": "text"},
                            ]},
                            {"name": "legacy_tokens", "change": "dropped", "columns": []},
                        ],
                        "risks": [
                            {"kind": "lock", "loc": "migrations/0042_policy.sql:3", "text": "NOT NULL without default rewrites sessions and fails on existing rows"},
                            {"kind": "mismatch", "loc": "api/auth.py:57", "text": "still selects legacy_tokens, which the migration drops"},
                        ],
                    })),
                    ..Default::default()
                },
            ),
            (
                pr_at(44, "Add S3 lifecycle rules", "acme/infra", "grace", 5.0, false, now),
                Verdict {
                    verdict: "request_changes".into(),
                    summary: "Expires logs after 30 days, moves backups to Glacier.".into(),
                    body: "- `infra/s3.tf:31` rule also matches the `backups/` prefix, would delete backups after 30d\n\
                           - no plan output attached"
                        .into(),
                    kind: "maintenance".into(),
                    ..Default::default()
                },
            ),
        ];
        Fixtures {
            mine: Mutex::new(vec![m1, m2]),
            rr: vec![r1, r2, r3],
            // 3rd refresh, exercises auto
            late: pr_at(213, "Hotfix: null check in export job", "acme/web", "bob", 0.0, false, now),
            assigned: vec![pr_at(300, "test: fix the flaky integration test in CI", "acme/api", "erin", 72.0, false, now)],
            // 2nd refresh, desktop notification
            late_assigned: pr_at(301, "security: bump base image to fix CVE", "acme/infra", "erin", 0.0, false, now),
            seed,
        }
    })
}

/// A finding list as the log stores it, from a verdict's raw ones.
fn findings(v: &Verdict) -> Vec<Finding> {
    v.findings
        .iter()
        .filter_map(|f| serde_json::from_value(f.clone()).ok())
        .collect()
}

/// Point config at a temp home and seed it with PRs and reviews.
///
/// ponytail: BOTH homes. team.joined() lists directories under TEAMS, so blanking the old singular
/// path alone left the demo reading whatever real teams this machine had joined, and the demo's
/// whole promise is that it touches nothing real.
pub fn install() {
    let root = std::env::temp_dir().join(format!("prs-demo-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&root);
    let log = root.join("prs-demo.jsonl");
    let memory = root.join("prs-demo-memory"); // Z dream must never rewrite the real memory
    let _ = std::fs::create_dir_all(&memory);
    config::update(|c| {
        c.demo = true;
        // never sync the demo
        c.team = root.join("team");
        c.teams = root.join("teams");
        c.settings = None; // never read or write the real settings
        c.log = log.clone();
        c.local_log = log.clone();
        c.memory_dir = memory.clone();
        c.local_memory = memory.clone();
        // and a demo pre-review lands where the pane looks for it, not in ~/.prs_reviews
        c.self_dir = root.join("prs-demo-reviews");
        c.reports = root.join("prs-demo-reports");
        c.held_dir = root.join("prs-demo-held");
        c.learning = root.join("prs-demo-learning.jsonl");
        c.backups = root.join("backups");
        c.registry = root.join("mirrors");
        c.bindings = root.join("bindings");
        c.autorev = root.join("autorev");
        c.dbrepo = root.join("dbrepo");
    });
    crate::memory::append("", "run make lint before flagging style", "");
    crate::memory::append("acme/api", "uses tabs\nuses tabs\nold CI on jenkins, ignore", "");
    let c = config::get();
    let mut text = String::new();
    for (p, v) in &fixtures().seed {
        let entry = LogEntry {
            at: p.updated_at.clone(),
            model: "opus".into(),
            pr: p.for_log(),
            depth: c.depth.clone(),
            effort: c.effort.clone(),
            head: String::new(),
            cost: None,
            ms: None,
            verdict: v.verdict.clone(),
            summary: v.summary.clone(),
            body: v.body.clone(),
            findings: findings(v),
            kind: v.kind.clone(),
            breaking: v.breaking,
            db: v.db.clone(),
            scores: v.scores.clone(),
        };
        text.push_str(&serde_json::to_string(&entry).unwrap_or_default());
        text.push('\n');
    }
    let _ = std::fs::write(&log, text);
}

fn pause(secs: u64) {
    if !cfg!(test) {
        std::thread::sleep(std::time::Duration::from_secs(secs));
    }
}

/// The fake fetch: the seeded rows, one more REVIEW REQUESTED on the 3rd refresh and one more
/// ASSIGNED on the 2nd, plus the log's REVIEWED rows.
pub fn sections() -> Vec<Section> {
    static FETCHES: AtomicUsize = AtomicUsize::new(0);
    let n = FETCHES.fetch_add(1, Ordering::SeqCst) + 1;
    pause(1);
    let f = fixtures();
    let mut rr = f.rr.clone();
    if n >= 3 {
        rr.push(f.late.clone());
    }
    let mut again = f.seed[2].0.clone();
    again.updated_at = now().to_rfc3339_opts(SecondsFormat::Secs, false);
    rr.push(again);
    let mut assigned = f.assigned.clone();
    if n >= 2 {
        assigned.push(f.late_assigned.clone());
    }
    let mine = f.mine.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let section = |name: &str, prs: Vec<Pr>| Section {
        name: name.into(),
        prs: Some(prs),
        err: None,
    };
    vec![
        section("MINE", mine),
        section("REVIEW REQUESTED", rr),
        section("ASSIGNED", assigned),
        section("REVIEWED", crate::log::reviewed()),
    ]
}

/// The fake reviewer: three verdicts then one failure, round robin, each written to the log.
pub fn review(pr: &Pr, model: &str) -> String {
    static TURN: AtomicUsize = AtomicUsize::new(0);
    pause(5);
    let verdicts = [
        Some(Verdict {
            verdict: "approve".into(),
            summary: "Fixes pagination when the page index is zero.".into(),
            body: "LGTM, regression test added.".into(),
            kind: "fix".into(),
            ..Default::default()
        }),
        Some(Verdict {
            verdict: "request_changes".into(),
            summary: "Caches user lookups for the lifetime of a session.".into(),
            body:
                "- `web/session.py:88` cache never invalidated on logout\n- missing test for cache miss path"
                    .into(),
            kind: "feature".into(),
            ..Default::default()
        }),
        Some(Verdict {
            verdict: "comment".into(),
            summary: "Rotates the signing keys and points the KMS alias at the new key.".into(),
            body: "Unsure whether old tokens must stay valid during rollover; please confirm.".into(),
            kind: "maintenance".into(),
            breaking: true,
            ..Default::default()
        }),
        None,
    ];
    match &verdicts[TURN.fetch_add(1, Ordering::SeqCst) % verdicts.len()] {
        Some(v) => {
            // demo has no review that read the settings, so it logs the current ones
            let c = config::get();
            let v = Verdict {
                depth: c.depth.clone(),
                effort: c.effort.clone(),
                ..v.clone()
            };
            crate::log::log_review(pr, model, &v, None).unwrap_or_else(|e| format!("error: {e}"))
        }
        None => "error: claude: rate limit exceeded, retry in 60s".into(),
    }
}

/// The fake pre-reviewer: writes a canned file where the pane looks for it. (status, path).
/// ponytail: faked for the same reason review is. `p` on a demo row used to spawn a real `claude -p`
/// against acme/api#101, which then called the github api, against a README that promises neither.
pub fn self_review(pr: &Pr) -> (String, PathBuf) {
    pause(4);
    let _ = std::fs::create_dir_all(config::get().self_dir);
    let dest = crate::review::self_review_path(pr.repo(), pr.number);
    let _ = std::fs::write(
        &dest,
        format!(
            "# Pre-review — {}#{}\n\n> **Not posted.** This is the demo reviewer: nothing ran, nothing was sent.\n\n\
             **Verdict (advisory):** ✗ changes requested — demo\n\n---\n\n## Findings\n\n\
             - `api/handlers.py:88` the pager reads `total` before the guard\n- no test covers the empty-result path\n",
            pr.repo(),
            pr.number
        ),
    );
    ("✗ changes requested (not posted) · 1 waiting".into(), dest)
}

/// The pane's second request. Without this it reaches for the real github api on a fake repo.
pub fn detail(repo: &str, number: u64) -> Option<Detail> {
    let n = number as i64;
    let check = |name: &str, state: &str| Check {
        name: name.into(),
        state: state.into(),
    };
    Some(Detail {
        branch: format!(
            "{}/pr-{number}",
            if repo.ends_with("api") { "alice" } else { "bob" }
        ),
        add: Some(62 + n % 40),
        del: Some(14 + n % 9),
        files: Some(3 + n % 4),
        checks: vec![
            check("unit tests", if number == 212 { "err" } else { "ok" }),
            check("typecheck", "ok"),
            check("lint", if number == 207 { "run" } else { "ok" }),
            check("preview deploy", "ok"),
        ],
    })
}

/// The fake dream: (summary, before, after), with the duplicate lines of each file folded.
pub fn dream() -> crate::memory::Dream {
    pause(4);
    let before = crate::memory::files();
    let after = before
        .iter()
        .map(|(n, t)| {
            let mut seen = Vec::new();
            for l in t.lines() {
                if !seen.contains(&l) {
                    seen.push(l);
                }
            }
            (n.clone(), seen.join("\n"))
        })
        .collect();
    (
        "merged 2 duplicate lines about tabs in acme/api\nmoved 'run make lint' to general\ndropped a stale note about the old CI"
            .into(),
        before,
        after,
    )
}

pub fn collaborators() -> Vec<String> {
    ["alice", "bob", "carol", "dave", "erin"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// `+` on a demo row: the login joins the reviewers of that MINE row. "" or the error.
pub fn request_review(number: u64, login: &str) -> String {
    pause(1);
    for p in fixtures()
        .mine
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter_mut()
    {
        if p.number == number {
            p.reviewers = format!("{} ·{login}", p.reviewers).trim().to_string();
        }
    }
    if login == "dave" {
        "dave is on leave (demo error)".into()
    } else {
        String::new()
    }
}

/// ponytail: a canned diff, so the code tab has something to anchor the seeded findings on.
pub fn diff_text(repo: &str, number: u64) -> String {
    let _ = (repo, number);
    concat!(
        "diff --git a/api/auth.py b/api/auth.py\n--- a/api/auth.py\n+++ b/api/auth.py\n@@ -36,8 +36,9 @@ def middleware(request):\n",
        "     token = request.headers.get('Authorization')\n-    policy.check(request.user)\n-    user = parse(token)\n",
        "+    user = parse(token)\n+    policy.check(user)\n+    request.user = user\n     return handle(request)\n",
        "diff --git a/infra/s3.tf b/infra/s3.tf\n--- a/infra/s3.tf\n+++ b/infra/s3.tf\n@@ -28,6 +28,9 @@ resource \"aws_s3_bucket\" \"logs\" {\n",
        "   bucket = var.name\n+  lifecycle_rule {\n+    prefix  = \"\"\n+    expiration { days = 30 }\n+  }\n }\n"
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_carries_the_github_shape() {
        let p = pr(7, "t", "acme/web", "bob", 2.0, true);
        assert_eq!(p.url, "https://github.com/acme/web/pull/7");
        assert_eq!(p.repository.name, "web");
        assert_eq!(p.author(), "bob");
        assert!(p.is_draft);
        assert!(p.updated_at.ends_with("+00:00"));
    }

    #[test]
    fn sections_match_the_python_demo() {
        let secs = sections();
        let names: Vec<&str> = secs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["MINE", "REVIEW REQUESTED", "ASSIGNED", "REVIEWED"]);
        let nums = |i: usize| {
            secs[i]
                .prs
                .as_ref()
                .unwrap()
                .iter()
                .map(|p| p.number)
                .collect::<Vec<_>>()
        };
        assert_eq!(nums(0), [101, 98]);
        assert!(nums(1).starts_with(&[212, 207, 55]) && nums(1).ends_with(&[44]));
        assert!(nums(2).starts_with(&[300]));
        let rr = &secs[1].prs.as_ref().unwrap()[0];
        assert_eq!(
            (rr.repo(), rr.author(), rr.checks.as_str()),
            ("acme/web", "bob", "✗")
        );
        // the late rows arrive on the 2nd and 3rd refresh
        let mut seen = Vec::new();
        for _ in 0..3 {
            let s = sections();
            seen.push((s[1].prs.as_ref().unwrap().len(), s[2].prs.as_ref().unwrap().len()));
        }
        assert!(seen.contains(&(5, 2)));
    }

    #[test]
    fn detail_and_diff_are_canned() {
        let d = detail("acme/api", 212).unwrap();
        assert_eq!(d.branch, "alice/pr-212");
        assert_eq!(d.checks[0].state, "err");
        assert_eq!(detail("acme/web", 207).unwrap().checks[2].state, "run");
        assert!(diff_text("acme/api", 180).contains("policy.check(user)"));
    }

    #[test]
    fn install_seeds_the_log_in_a_temp_dir() {
        // ponytail: take the lock first: this one swaps the whole global config and puts it back, so
        // without it every other test reading the config could be running in demo mode for as long as
        // this takes — which is most of what #134 was.
        let _g = crate::config::test_lock();
        let before = config::get();
        install();
        let c = config::get();
        // ponytail: put the global config back before asserting, so a failure here does not leave
        // every other test in the process running in demo mode.
        config::update(|cfg| *cfg = before);
        assert!(c.demo && c.settings.is_none());
        assert!(c.log.starts_with(std::env::temp_dir()));
        assert!(c.memory_dir.is_dir());
        let text = std::fs::read_to_string(&c.log).unwrap();
        let entries: Vec<LogEntry> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(
            entries.iter().map(|e| e.pr.number).collect::<Vec<_>>(),
            [180, 180, 44]
        );
        assert_eq!(entries[1].findings.len(), 2);
        assert_eq!(entries[0].at, entries[0].pr.updated_at);
        let _ = std::fs::remove_dir_all(c.log.parent().unwrap());
    }
}
