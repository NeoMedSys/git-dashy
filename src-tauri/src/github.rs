//! Everything that talks to GitHub. Port of dashy/core/github.py.
//!
//! ponytail: ureq against the API, no `gh` binary. One dependency less to install, and every failure
//! arrives as an `Error` instead of a string parsed out of someone's stderr.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{json, Value};

use crate::config;
use crate::types::{Check, Detail, Inline, Pr, Section};

/// The REST root: $GITHUB_API or https://api.github.com. GraphQL is `/graphql` beside it
/// (on Enterprise /api/v3 and /api/graphql are siblings; replace "/api/v3" with "/api").
pub fn api_root() -> String {
    std::env::var("GITHUB_API")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://api.github.com".into())
}

// ponytail: graphql does not live under the REST root on Enterprise: /api/v3 and /api/graphql are
// siblings there, while on github.com the two share a host. One replace covers both.
pub fn graphql_url() -> String {
    api_root()
        .replace("/api/v3", "/api")
        .trim_end_matches('/')
        .to_string()
        + "/graphql"
}

/// An API failure: "<code> <path>: <message>" or "<path>: <io error>".
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct Error(pub String);

pub const SCOPE: &str = "PRS_API_REPO";
/// Every scope toggled on this session. ponytail: kept searched after it is toggled off, so the board
/// filters it away and back without a fetch; session-only, so a restart searches just what is saved.
static FETCHED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Most scopes one TEAM search carries. ponytail: a guess under GitHub's search length cap; the oldest
/// toggled-off scope drops first, the saved ones never.
const MAX_FETCHED: usize = 8;

/// `fetched` plus every saved scope it lacks, trimmed to MAX_FETCHED by dropping the oldest scope that
/// is no longer saved.
fn merge_fetched(saved: &[String], fetched: &[String]) -> Vec<String> {
    let mut f = fetched.to_vec();
    f.extend(saved.iter().filter(|s| !fetched.contains(s)).cloned());
    while f.len() > MAX_FETCHED {
        let Some(i) = f.iter().position(|s| !saved.contains(s)) else {
            break;
        };
        f.remove(i);
    }
    f
}

/// True when `scope` is already in the TEAM search, so toggling it on needs no refetch.
pub fn fetched(scope: &str) -> bool {
    FETCHED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|s| s == scope)
}
pub const SCOPE_TEAM: &str = "PRS_API_TEAM";
/// The DB repo a review may also read, "" for none. See dbrepo.rs.
pub const SCOPE_DB: &str = "PRS_API_DB";
pub const SECTIONS: &[(&str, &str)] = &[
    ("MINE", "is:open author:{me}"),
    ("REVIEW REQUESTED", "is:open review-requested:{me}"),
    ("ASSIGNED", "is:open assignee:{me}"),
    // ponytail: open PRs in whatever orgs/teams are toggled on, so the board sees the team's work, not
    // only what names me. Dropped from the query when nothing is on. Last, so dedup gives it the leftovers.
    // Sorted, so the 100 it returns are the newest, not GitHub's best-match slice.
    ("TEAM", "is:open {scope} sort:updated-desc"),
    // merged PRs from the same sources and window, so the board shows what the team shipped
    ("MERGED", "is:merged {scope} sort:updated-desc"),
];

/// True for a section that searches the toggled-on sources, and so is not asked for when none is on.
fn sourced(q: &str) -> bool {
    q.contains("{scope}")
}
/// Search terms that choose WHERE to look, not what for.
const QUALIFIERS: &[&str] = &["repo:", "user:", "org:", "owner:"];

/// $GH_TOKEN or $GITHUB_TOKEN, trimmed; "" without one.
/// ponytail: the environment, and nothing else. gh's own token store is gh's, and reading it would
/// make an uninstall of gh look like a gitdashy failure.
pub fn token() -> String {
    ["GH_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty())
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

/// The owner/name a `/repos/...` path addresses, "" otherwise.
pub fn repo_of(path: &str) -> String {
    let part: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if part.len() >= 3 && part[0].eq_ignore_ascii_case("repos") && !part[1].is_empty() && !part[2].is_empty()
    {
        format!("{}/{}", part[1], part[2])
    } else {
        String::new()
    }
}

/// `path` rewritten so it can only read `repo` (and repos bound to `team`). Err(why) when it cannot be.
/// "" = unscoped.
///
/// ponytail: the reviewer's input is an untrusted diff and its output is published on that diff's PR, so
/// "read anything the token can" closes a loop: steer the read, and the answer is posted for you. The
/// read is GET-only and host-pinned already; this is the third side, and the one that was open.
/// ponytail: the scope arrives in the ENVIRONMENT, not in the argv the model writes. There is nothing a
/// prompt can say that widens it, and nothing to keep in step with the --allowedTools pattern.
/// ponytail: "" means unscoped, which is a person at a terminal. Their own `gitdashy api /user/repos`
/// is not the threat and refusing it would only teach them to work around this.
pub fn scoped(path: &str, repo: &str, team: &str, db: &str) -> Result<String, String> {
    if repo.is_empty() {
        return Ok(path.to_string());
    }
    let p = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let (head, query) = p.split_once('?').unwrap_or((&p, ""));
    // ponytail: no dot segments, in any spelling. api.github.com 404s /repos/<scope>/../../user today, so
    // the prefix check is not bypassable there, but that is the SERVER refusing, not us, and GitHub
    // Enterprise can sit behind a proxy that normalises before it forwards. A boundary that holds only
    // because the far end happens to be strict is one deployment away from not holding. Unquoted for the
    // test and never for the request, so what is sent is still exactly what was asked for.
    //
    // ponytail: split AGAIN after decoding. A segment can decode to separators of its own, so ..%2f..%2fuser
    // arrives as one segment that is not "..", passes a decode-and-compare, and is a traversal by the time
    // anything normalises it. Backslash too: a proxy on Windows may fold it to a slash.
    if head
        .split('/')
        .any(|seg| unquote(seg).split(['/', '\\']).any(|s| s == ".."))
    {
        return Err(format!("a review may not use .. in a path, and {head} does"));
    }
    let (want, low) = (format!("/repos/{repo}").to_lowercase(), head.to_lowercase());
    // ponytail: the separator matters. Bare startswith let /repos/acme/api-secrets through on a scope of
    // acme/api: a neighbouring repo, which is exactly the kind an attacker would guess at.
    if low == want || low.starts_with(&format!("{want}/")) {
        return Ok(p);
    }
    // ponytail: and a repo DECLARED to belong with this one. Resolved through bind::of rather than against
    // a list built here, so the precedence (an exact binding, then a deliberate unbind, then the owner
    // rule) is the one bind already publishes, and a repo excluded from an owner rule stays excluded. A
    // second copy of that ordering is how the two would come to disagree about one repo.
    if !team.is_empty() {
        let other = repo_of(head);
        // ponytail: the name has to BE its own key before the key is looked up. bind::key strips a `.git`
        // suffix and slug_of folds `:`, so /repos/acme/shared-lib.git/... resolved to a bound sibling and
        // was then sent verbatim: the check normalising one string and the request carrying another.
        // GitHub 404s that form today, which is the same "the server saves us" argument as the dots above.
        if !other.is_empty()
            && other.to_lowercase() != repo.to_lowercase()
            && crate::bind::key(&other) == other.to_lowercase()
            && crate::bind::of(&other) == team
        {
            return Ok(p);
        }
    }
    // and the DB repo a person pointed this repo at: the same exact-key check, against one name
    if !db.is_empty() {
        let other = repo_of(head);
        if !other.is_empty() && crate::bind::key(&other) == other.to_lowercase() && other.to_lowercase() == db
        {
            return Ok(p);
        }
    }
    // ponytail: search stays on the repo under review even when reads are wider. Several repo: qualifiers
    // would have to OR for that to be safe, and leaning a boundary on GitHub's query semantics is what
    // the refusal loop below already declines to do. A sibling is read by path, not searched.
    if low.trim_end_matches('/') == "/search/code" {
        return Ok(format!("/search/code?{}", scoped_query(query, repo)?));
    }
    if low.starts_with("/search/") {
        // ponytail: "outside it" describes a repo path, and a model reading it about /search/repositories
        // learns nothing it can act on. Say which search there is.
        return Err(format!(
            "a review may only search code, in {repo}: {head} is not /search/code"
        ));
    }
    let mut bound = if team.is_empty() {
        String::new()
    } else {
        format!(" and the repos bound to {team}")
    };
    if !db.is_empty() {
        bound += &format!(" and its DB repo {db}");
    }
    Err(format!(
        "a review may only read {repo}{bound}, and {head} is outside it"
    ))
}

/// A /search/code query string forced to `repo`. Err when it names anywhere else.
///
/// ponytail: REWRITTEN, not merely checked. A `q` carrying no qualifier at all searches every repo the
/// token can see, so refusing only the ones that name someone else would leave the default (the form a
/// model reaches for first) wide open. parse_qsl also turns `+` back into a space, which is how the
/// qualifier in "q=SECRET+user:victim" becomes visible as a term rather than hiding inside one.
pub fn scoped_query(query: &str, repo: &str) -> Result<String, String> {
    let parts = parse_qsl(query);
    let q: Vec<String> = parts
        .iter()
        .filter(|(k, _)| k == "q")
        .map(|(_, v)| v.clone())
        .collect();
    // ponytail: one pass. A foreign qualifier is refused rather than silently narrowed: the forced
    // repo: ANDs, so it would return nothing anyway, but that is GitHub's query semantics holding the
    // line rather than us, and a model handed an empty result cannot tell "nobody uses this symbol"
    // from "you asked the wrong question".
    let mut terms = Vec::new();
    let forced = format!("repo:{repo}").to_lowercase();
    for t in q.join(" ").split_whitespace() {
        let low = t.to_lowercase();
        if !QUALIFIERS.iter().any(|q| low.starts_with(q)) {
            terms.push(t.to_string());
        } else if low != forced {
            return Err(format!(
                "a review may only search {repo}, so {t} cannot be asked for"
            ));
        }
    }
    if terms.is_empty() {
        // ponytail: a qualifier on its own is a 422 from GitHub, which reads as the scoping being broken.
        return Err("a code search needs something to search for, not just a repo".into());
    }
    terms.push(format!("repo:{repo}"));
    let mut pairs = vec![("q".to_string(), terms.join(" "))];
    pairs.extend(parts.into_iter().filter(|(k, _)| k != "q"));
    Ok(urlencode(&pairs))
}

/// Python's urllib.parse.unquote: percent-decoding, a broken escape left as it was.
fn unquote(s: &str) -> String {
    urlencoding::decode(s)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| s.to_string())
}

/// Python's parse_qsl: `+` is a space, blank values are dropped.
fn parse_qsl(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            if v.is_empty() {
                return None;
            }
            Some((unquote(&k.replace('+', " ")), unquote(&v.replace('+', " "))))
        })
        .collect()
}

/// Python's urlencode: quote_plus on both sides, so a space is `+` and `:` and `/` are escaped.
fn urlencode(pairs: &[(String, String)]) -> String {
    let enc = |s: &str| urlencoding::encode(s).replace("%20", "+");
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn host_of(url: &str) -> String {
    url.parse::<ureq::http::Uri>()
        .ok()
        .and_then(|u| u.host().map(|h| h.to_lowercase()))
        .unwrap_or_default()
}

/// One API call, the response text. `body` None for no body. Err on anything but 2xx.
pub fn call(
    path: &str,
    method: &str,
    body: Option<&serde_json::Value>,
    accept: &str,
    timeout_secs: u64,
) -> Result<String, Error> {
    let root = api_root();
    let url = if path.starts_with("http") {
        path.to_string()
    } else {
        format!("{root}{path}")
    };
    let tok = token();
    let mut req = ureq::http::Request::builder()
        .method(method)
        .uri(&url)
        .header("Accept", accept)
        .header("X-GitHub-Api-Version", "2022-11-28");
    // ponytail: the token goes to the API host and nowhere else. A review reads untrusted diffs and can
    // choose the path it asks for, so an absolute URL in there must not be a way to post the token out.
    if !tok.is_empty() && host_of(&url) == host_of(&root) {
        req = req.header("Authorization", format!("Bearer {tok}"));
    }
    if body.is_some() {
        req = req.header("Content-Type", "application/json");
    }
    log::debug!("{method} {url}");
    // ponytail: no redirects at all. ureq would copy every header onto a redirected request, so the host
    // check above would guard only the request this code builds, not the one ureq ends up sending. The
    // API never redirects for the calls made here, so a 3xx is simply an error, and the token stays put.
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(timeout_secs)))
            .build(),
    );
    let sent = match body {
        Some(b) => req
            .body(b.to_string())
            .map_err(|e| Error(format!("{path}: {e}")))
            .and_then(|r| agent.run(r).map_err(|e| Error(format!("{path}: {e}")))),
        None => req
            .body(())
            .map_err(|e| Error(format!("{path}: {e}")))
            .and_then(|r| agent.run(r).map_err(|e| Error(format!("{path}: {e}")))),
    };
    let mut resp = sent?;
    let status = resp.status();
    let text = resp
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .lossy_utf8(true)
        .read_to_string()
        .map_err(|e| Error(format!("{path}: {e}")))?;
    if status.is_success() {
        return Ok(text);
    }
    let code = status.as_u16();
    let detail = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(String::from))
        .unwrap_or_default();
    let hint = if (code == 401 || code == 403) && tok.is_empty() {
        ": no token: export GH_TOKEN=… (scope: repo)"
    } else {
        ""
    };
    let reason = if detail.is_empty() {
        status.canonical_reason().unwrap_or("").to_string()
    } else {
        detail
    };
    Err(Error(format!("{code} {path}: {reason}{hint}")))
}

pub fn api(path: &str, timeout_secs: u64) -> Result<serde_json::Value, Error> {
    serde_json::from_str(&call(
        path,
        "GET",
        None,
        "application/vnd.github+json",
        timeout_secs,
    )?)
    .map_err(|e| Error(format!("{path}: {e}")))
}

/// GraphQL, returning `data`. Partial data survives partial errors (a missing scope drops fields).
pub fn gql(query: &str, timeout_secs: u64) -> Result<serde_json::Value, Error> {
    let url = graphql_url();
    let text = call(
        &url,
        "POST",
        Some(&json!({ "query": query })),
        "application/vnd.github+json",
        timeout_secs,
    )?;
    let d: Value = serde_json::from_str(&text).map_err(|e| Error(format!("{url}: {e}")))?;
    match d.get("data") {
        Some(data) if !data.is_null() => Ok(data.clone()),
        _ => Err(Error(
            d.get("errors")
                .and_then(|e| e.get(0))
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("graphql returned no data")
                .to_string(),
        )),
    }
}

static ME: Mutex<String> = Mutex::new(String::new());

/// Your login if a fetch has already asked for it, "" before. Never goes to the network.
pub fn me_cached() -> String {
    ME.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Your login, cached for the process.
/// ponytail: `@me` is gh/UI sugar the API does not resolve, so the search needs the name.
pub fn me() -> Result<String, Error> {
    let cached = ME.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if !cached.is_empty() {
        return Ok(cached);
    }
    let login = gql("{ viewer { login } }", 60)?
        .pointer("/viewer/login")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| Error("graphql returned no viewer".into()))?;
    *ME.lock().unwrap_or_else(|e| e.into_inner()) = login.clone();
    Ok(login)
}

// One page of one section's search: the row fields plus the review decision, reviewers, head commit and CI
// state that no list endpoint returns, and pageInfo to walk the next page.
// ponytail: no `... on Team { slug }`. That field needs read:org, and a token without it failed the WHOLE
// query, so CI, status and reviewers all vanished. Team review requests are simply not shown.
const NODE: &str = "{ pageInfo { hasNextPage endCursor } nodes { ... on PullRequest { number title url updatedAt isDraft additions deletions
    author { login } authorAssociation repository { nameWithOwner name } headRefOid reviewDecision
    commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
    reviewRequests(first: 20) { totalCount nodes { requestedReviewer { ... on User { login } } } }
    latestReviews(first: 20) { nodes { author { login } state } } } } }";

fn decision(d: &str) -> &'static str {
    match d {
        "APPROVED" => "✓ approved",
        "CHANGES_REQUESTED" => "✗ changes requested",
        "REVIEW_REQUIRED" => "· awaiting review",
        _ => "",
    }
}

fn review_glyph(s: &str) -> &'static str {
    match s {
        "APPROVED" => "✓",
        "CHANGES_REQUESTED" => "✗",
        "COMMENTED" => "~",
        "PENDING" => "·",
        _ => "·",
    }
}

/// PRs one request asks for, and the most one section keeps.
/// ponytail: 20, measured: GitHub gives a query about 10s and answers a 502 past it. In a busy org 50 PRs
/// with checks, diff stats and reviews ran past that and 25 took up to 7.5s. Pages, not fewer fields.
const PAGE: usize = 20;
const MOST: usize = 100;
/// Seconds one page may take. GitHub gives up at about 10 and answers 502, so waiting longer only holds the
/// tick: five sections of five pages at 60s each could keep `fetching` up for minutes.
const PAGE_SECS: u64 = 20;

/// The search string of every section to ask for, by its SECTIONS index.
pub fn searches(who: &str, scope: &str) -> Vec<(usize, String)> {
    SECTIONS
        .iter()
        .enumerate()
        .filter(|(_, (_, q))| !(sourced(q) && scope.is_empty()))
        .map(|(i, (_, q))| {
            (
                i,
                format!("is:pr {}", q.replace("{me}", who).replace("{scope}", scope)),
            )
        })
        .collect()
}

/// One page of one search, as a GraphQL document.
pub fn page(search: &str, after: &str) -> String {
    // a JSON string is a valid GraphQL string literal, so escaping is structural
    let after = if after.is_empty() {
        String::new()
    } else {
        format!(", after: {}", Value::String(after.into()))
    };
    format!(
        "{{ s: search(query: {}, type: ISSUE, first: {PAGE}{after}) {NODE} }}",
        Value::String(search.into())
    )
}

/// Every page of `search` up to MOST, as `{"nodes": [...]}`. `get` runs one document (gql, or a test's).
/// A failed first page is the section's error; a failed later page keeps what arrived, since the
/// newest PRs come first and an error line would hide them.
pub fn search_all(search: &str, get: impl Fn(&str) -> Result<Value, Error>) -> Result<Value, Error> {
    let mut nodes: Vec<Value> = Vec::new();
    let mut after = String::new();
    while nodes.len() < MOST {
        let s = match get(&page(search, &after)) {
            // search is non-null in GitHub's schema, so gql errs first today; a null must still never
            // read as an empty section
            Ok(d) if d["s"].is_null() && nodes.is_empty() => {
                return Err(Error("search returned no data".into()))
            }
            Ok(d) => d["s"].clone(),
            Err(e) if nodes.is_empty() => return Err(e),
            Err(e) => {
                log::debug!(
                    "search page after {} failed, keeping {}: {}",
                    after,
                    nodes.len(),
                    e.0
                );
                break;
            }
        };
        nodes.extend(s["nodes"].as_array().cloned().unwrap_or_default());
        after = str_at(&s, "/pageInfo/endCursor").to_string();
        if !s
            .pointer("/pageInfo/hasNextPage")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || after.is_empty()
        {
            break;
        }
    }
    nodes.truncate(MOST);
    Ok(json!({ "nodes": nodes }))
}

/// A name safe to put inside the query's string literal: a login, owner/name, or team key.
fn plain(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c))
}

/// The search terms for the toggled-on scopes: "org:x" as is, "team:k" as every repo and owner bound
/// to k. GitHub ORs repeated org:/repo: terms, so this stays one search. "" when nothing is on.
pub fn scope_terms(
    scopes: &[String],
    repos: &HashMap<String, String>,
    owners: &HashMap<String, String>,
) -> String {
    let mut terms: Vec<String> = Vec::new();
    let mut add = |t: String| {
        if !terms.contains(&t) {
            terms.push(t);
        }
    };
    for s in scopes {
        match s.split_once(':') {
            Some(("org", o)) if plain(o) && !o.contains('/') => add(format!("org:{o}")),
            Some(("team", k)) => {
                // ponytail: sorted, so the query text (and a test) does not shuffle with HashMap order
                let mut bound: Vec<String> = repos
                    .iter()
                    .filter(|(_, t)| *t == k)
                    .map(|(r, _)| format!("repo:{r}"))
                    .chain(
                        owners
                            .iter()
                            .filter(|(_, t)| *t == k)
                            .map(|(o, _)| format!("org:{o}")),
                    )
                    .filter(|t| plain(t.split_once(':').map(|x| x.1).unwrap_or("")))
                    .collect();
                bound.sort();
                bound.into_iter().for_each(&mut add);
            }
            _ => {}
        }
    }
    terms.join(" ")
}

/// The TEAM and MERGED searches held to the history window, like REVIEWED: only PRs touched in the last `window`
/// hours. `None` is all time; "" stays "" so nothing toggled still means no search.
pub fn within(scope: &str, window: Option<u64>, now: chrono::DateTime<chrono::Utc>) -> String {
    match window {
        Some(h) if !scope.is_empty() => {
            let since = now - chrono::Duration::hours(h as i64);
            format!("{scope} updated:>={}", since.format("%Y-%m-%dT%H:%M:%SZ"))
        }
        _ => scope.to_string(),
    }
}

/// What the scope chips offer: every joined team, then every owner seen on the board, in the log or in
/// a binding, then any saved scope none of those name, so a toggled-on one can always be toggled off.
/// ponytail: not the viewer's org list, that needs read:org and the token usually lacks it.
pub fn scope_options(
    saved: &[String],
    sections: &[Section],
    teams: &[String],
    repos: &HashMap<String, String>,
    owners: &HashMap<String, String>,
) -> Vec<String> {
    let mut orgs: Vec<String> = sections
        .iter()
        .flat_map(|s| s.prs.iter().flatten())
        .map(|p| p.repo().split('/').next().unwrap_or("").to_lowercase())
        .chain(
            repos
                .keys()
                .map(|r| r.split('/').next().unwrap_or("").to_string()),
        )
        .chain(owners.keys().cloned())
        .filter(|o| plain(o) && !o.contains('/'))
        .collect();
    orgs.sort();
    orgs.dedup();
    teams
        .iter()
        .map(|t| format!("team:{t}"))
        .chain(orgs.into_iter().map(|o| format!("org:{o}")))
        .chain(saved.iter().cloned())
        .fold(Vec::new(), |mut v: Vec<String>, o| {
            if !v.contains(&o) {
                v.push(o);
            }
            v
        })
}

fn str_at<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or("")
}

/// Row status for my own PR from its reviewDecision + pending review requests.
pub fn own_status(node: &Value) -> String {
    let d = str_at(node, "/reviewDecision");
    let pending = node
        .pointer("/reviewRequests/totalCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if d == "CHANGES_REQUESTED" && pending > 0 {
        return "↻ re-review requested".into(); // I pushed and asked again, reviewer has not looked yet
    }
    decision(d).into()
}

/// CI glyph for the head commit: ✓ green, ✗ failed, ● running, "" when the repo has no checks.
pub fn checks(node: &Value) -> String {
    let Some(c) = node
        .pointer("/commits/nodes")
        .and_then(Value::as_array)
        .and_then(|n| n.first())
    else {
        return String::new();
    };
    match str_at(c, "/commit/statusCheckRollup/state") {
        "SUCCESS" => "✓",
        "FAILURE" | "ERROR" => "✗",
        "PENDING" | "EXPECTED" => "●",
        _ => "",
    }
    .into()
}

/// '✓bob ·alice': everyone asked to review or who did, with their latest state (· = not yet).
pub fn reviewers(node: &Value) -> String {
    let mut out: Vec<(String, String)> = Vec::new();
    fn set(out: &mut Vec<(String, String)>, who: &str, state: &str) {
        match out.iter_mut().find(|(w, _)| w == who) {
            Some(e) => e.1 = state.to_string(),
            None => out.push((who.to_string(), state.to_string())),
        }
    }
    for n in node
        .pointer("/latestReviews/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let who = str_at(n, "/author/login");
        if n.get("author").is_some_and(|a| !a.is_null()) && !who.is_empty() {
            set(&mut out, who, str_at(n, "/state"));
        }
    }
    for n in node
        .pointer("/reviewRequests/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let who = str_at(n, "/requestedReviewer/login");
        // ponytail: a fresh request supersedes an older review, EXCEPT a comment. GitHub clears the
        // request when a review approves or requests changes, so a reviewer in BOTH lists really was
        // asked again. Commenting clears nothing, so the standing request is the ORIGINAL one, and
        // stomping it made every comment invisible to everyone but the person who left it.
        // Compare the STATE, not the glyph: DISMISSED has no glyph and must not read as a comment.
        if !who.is_empty() && out.iter().find(|(w, _)| w == who).map(|(_, s)| s.as_str()) != Some("COMMENTED")
        {
            set(&mut out, who, "PENDING");
        }
    }
    out.iter()
        .map(|(who, s)| format!("{}{who}", review_glyph(s)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Logins with access to repo, [] when they cannot be listed (no admin, offline).
pub fn collaborators(repo: &str) -> Vec<String> {
    if config::get().demo {
        return ["alice", "bob", "carol", "dave", "erin"]
            .iter()
            .map(|s| s.to_string())
            .collect();
    }
    let Ok(Value::Array(rows)) = api(&format!("/repos/{repo}/collaborators?per_page=100"), 30) else {
        return Vec::new();
    };
    let logins: Option<Vec<String>> = rows
        .iter()
        .map(|c| c.get("login").and_then(Value::as_str).map(String::from))
        .collect();
    logins.unwrap_or_default()
}

static MY_REPOS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// `owner/name` of every repo you own, collaborate on or reach through an org, most recently pushed first.
/// [] when they cannot be listed (offline, no token).
/// ponytail: cached for the process once a listing succeeds, and capped at 1000; a repo made after launch
/// shows after a restart. Refetch on a timer if that bites.
pub fn my_repos() -> Vec<String> {
    if config::get().demo {
        return [
            "acme/api",
            "acme/web",
            "acme/db",
            "acme/infra",
            "tools/cli",
            "tools/docs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
    }
    let mut cache = MY_REPOS.lock().unwrap_or_else(|e| e.into_inner());
    if !cache.is_empty() {
        return cache.clone();
    }
    let mut repos = Vec::new();
    let mut after = String::new();
    for _ in 0..10 {
        let cursor = if after.is_empty() {
            String::new()
        } else {
            format!(", after: {}", json!(after))
        };
        let query = format!(
            "{{ viewer {{ repositories(first: 100{cursor}, affiliations: [OWNER, COLLABORATOR, ORGANIZATION_MEMBER], \
             ownerAffiliations: [OWNER, COLLABORATOR, ORGANIZATION_MEMBER], orderBy: {{field: PUSHED_AT, direction: DESC}}) \
             {{ nodes {{ nameWithOwner }} pageInfo {{ hasNextPage endCursor }} }} }} }}"
        );
        let Ok(data) = gql(&query, 30) else {
            return repos;
        };
        let page = &data["viewer"]["repositories"];
        repos.extend(
            page["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|n| n["nameWithOwner"].as_str().map(String::from)),
        );
        match page["pageInfo"]["endCursor"].as_str() {
            Some(c) if page["pageInfo"]["hasNextPage"].as_bool() == Some(true) => after = c.to_string(),
            _ => break,
        }
    }
    *cache = repos.clone();
    repos
}

/// Ask login to review PR number; the error text, or "" on success.
pub fn request_review(repo: &str, number: u64, login: &str) -> String {
    if config::get().demo {
        std::thread::sleep(Duration::from_secs(1));
        return if login == "dave" {
            "dave is on leave (demo error)".into()
        } else {
            String::new()
        };
    }
    let body = json!({ "reviewers": [login] });
    match call(
        &format!("/repos/{repo}/pulls/{number}/requested_reviewers"),
        "POST",
        Some(&body),
        "application/vnd.github+json",
        30,
    ) {
        Ok(_) => String::new(),
        Err(e) => e.0,
    }
}

/// ponytail: the header is scoped to this remote. An unscoped extraHeader would hand the token to ANY
/// http remote the checkout later talks to.
pub const GITHUB: &str = "https://github.com/";

/// Env that lets a git clone reach a private repo with the API token: GIT_CONFIG_COUNT/KEY_0/VALUE_0.
///
/// ponytail: GIT_CONFIG_* in the environment, not `-c` in argv: argv is world-readable in `ps` for the
/// length of a clone. It does not survive into the new checkout, so `persist_auth()` writes it there.
pub fn git_auth() -> HashMap<String, String> {
    let h = git_header();
    if h.is_empty() {
        return HashMap::new();
    }
    auth_env(&h)
}

fn auth_env(h: &str) -> HashMap<String, String> {
    // ponytail: the empty value FIRST resets the accumulated extraHeader list (git's documented
    // reset), so the token persist_auth() wrote into .git/config at clone time cannot ride along
    // beside this one. That checkout's copy never expires on its own: when the token behind it is
    // rotated or revoked, every pull after it fails "Authentication failed" and no re-login fixes
    // it, because the stale header is in the repo, not in the login. Two headers in one request is
    // the other half: github answers the pair with invalid credentials whichever one is current.
    HashMap::from([
        ("GIT_CONFIG_COUNT".to_string(), "2".to_string()),
        (
            "GIT_CONFIG_KEY_0".to_string(),
            format!("http.{GITHUB}.extraHeader"),
        ),
        ("GIT_CONFIG_VALUE_0".to_string(), String::new()),
        (
            "GIT_CONFIG_KEY_1".to_string(),
            format!("http.{GITHUB}.extraHeader"),
        ),
        ("GIT_CONFIG_VALUE_1".to_string(), format!("Authorization: {h}")),
    ])
}

/// "Basic <b64 x-access-token:TOKEN>" or "".
///
/// ponytail: Basic, not Bearer. The REST API takes a PAT either way; the git endpoint answers Bearer
/// with "remote: invalid credentials" and only takes the token as a Basic password, the form gh's
/// own credential helper sends. One place, so the clone and the persisted config cannot drift.
pub fn git_header() -> String {
    header_for(&token())
}

fn header_for(tok: &str) -> String {
    if tok.is_empty() {
        return String::new();
    }
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{tok}"))
    )
}

/// Append the auth header to a fresh checkout's .git/config (chmod 600 first).
///
/// ponytail: appended by hand rather than `git config`, which would put the token back in argv, the
/// thing git_auth() exists to avoid. chmod first: the secret is never on disk world-readable.
pub fn persist_auth(dest: &std::path::Path) {
    persist_header(dest, &git_header());
}

fn persist_header(dest: &std::path::Path, h: &str) {
    let cfg = dest.join(".git").join("config");
    if h.is_empty() || !cfg.is_file() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o600));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&cfg) {
        let _ = write!(f, "[http \"{GITHUB}\"]\n\textraHeader = Authorization: {h}\n");
    }
}

fn verdict_event(verdict: &str) -> &'static str {
    match verdict {
        "approve" => "APPROVE",
        "request_changes" => "REQUEST_CHANGES",
        _ => "COMMENT",
    }
}

/// One entry per SECTIONS plus REVIEWED (from log::reviewed). Never fails: a failed fetch is an err per section.
pub fn fetch() -> Vec<Section> {
    if config::get().demo {
        return crate::demo::sections();
    }
    let cfg = config::get();
    let searched = {
        let mut f = FETCHED.lock().unwrap_or_else(|e| e.into_inner());
        *f = merge_fetched(&cfg.scopes, &f);
        f.clone()
    };
    let scope = within(
        &{
            let (repos, owners) = crate::bind::maps();
            scope_terms(&searched, &repos, &owners)
        },
        cfg.window,
        chrono::Utc::now(),
    );
    let first_line = |e: &Error| {
        e.0.trim()
            .lines()
            .next()
            .filter(|l| !l.is_empty())
            .unwrap_or("github unreachable")
            .to_string()
    };
    let data = match me() {
        Ok(who) => {
            // ponytail: one request per section, all at once. One query holding every section ran past
            // GitHub's time limit in a busy org, and its 502 blanked MINE along with TEAM.
            let got: Vec<(usize, Result<Value, Error>)> = std::thread::scope(|t| {
                let running: Vec<_> = searches(&who, &scope)
                    .into_iter()
                    .map(|(i, q)| (i, t.spawn(move || search_all(&q, |doc| gql(doc, PAGE_SECS)))))
                    .collect();
                running
                    .into_iter()
                    .map(|(i, h)| {
                        (
                            i,
                            h.join().unwrap_or_else(|_| Err(Error("search panicked".into()))),
                        )
                    })
                    .collect()
            });
            let mut data = serde_json::Map::new();
            let mut errs: HashMap<usize, String> = HashMap::new();
            for (i, r) in got {
                match r {
                    Ok(v) => {
                        data.insert(format!("s{i}"), v);
                    }
                    Err(e) => {
                        errs.insert(i, first_line(&e));
                        data.insert(format!("s{i}"), Value::Null);
                    }
                }
            }
            if errs.keys().any(|i| sourced(SECTIONS[*i].1)) {
                // ponytail: a scope that broke its search must not outlive being toggled off, so the next
                // tick searches only what is saved
                *FETCHED.lock().unwrap_or_else(|e| e.into_inner()) = config::get().scopes;
                // fresh: `cfg` predates an untoggle
            }
            let mut out = sections_of(&Value::Object(data));
            for s in out.iter_mut() {
                if let Some(i) = SECTIONS.iter().position(|(n, _)| *n == s.name) {
                    if let Some(e) = errs.remove(&i) {
                        s.err = Some(e);
                    }
                }
            }
            out
        }
        Err(e) => {
            let err = first_line(&e);
            let mut out: Vec<Section> = SECTIONS
                .iter()
                .filter(|(_, q)| !sourced(q) || !scope.is_empty())
                .map(|(name, _)| Section {
                    name: name.to_string(),
                    prs: None,
                    err: Some(err.clone()),
                })
                .collect();
            out.push(Section {
                name: "REVIEWED".into(),
                prs: Some(crate::log::reviewed()),
                err: None,
            });
            return out;
        }
    };
    let mut out = data;
    // ponytail: not deduped, a reviewed PR may still be open above
    out.push(Section {
        name: "REVIEWED".into(),
        prs: Some(crate::log::reviewed()),
        err: None,
    });
    out
}

/// The SECTIONS from a dashboard query's `data`: deduped across sections, newest first.
pub fn sections_of(data: &Value) -> Vec<Section> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (i, (name, q)) in SECTIONS.iter().enumerate() {
        match data.get(format!("s{i}")) {
            None if sourced(q) => continue, // nothing toggled on, so it was never asked for
            Some(Value::Null) => {
                // a failed search; fetch puts its message on it. An empty section must never stand in for
                // a rejected one
                out.push(Section {
                    name: name.to_string(),
                    prs: None,
                    err: Some("search failed".into()),
                });
                continue;
            }
            _ => {}
        }
        let mut prs: Vec<Pr> = Vec::new();
        for n in data
            .pointer(&format!("/s{i}/nodes"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let url = str_at(n, "/url");
            // ponytail: dedup across sections, first section wins
            if n.is_null() || !n.is_object() || seen.contains(&url.to_string()) {
                continue;
            }
            seen.push(url.to_string());
            let mut p: Pr = serde_json::from_value(n.clone()).unwrap_or_default();
            p.checks = checks(n);
            // ponytail: absent field reads like a failed call: no head, not ""
            p.head = str_at(n, "/headRefOid").to_string();
            // ponytail: reviewers on EVERY section, not just MINE. A "~alice" only ever painted on my
            // own rows, so a comment on someone else's PR was visible to nobody looking at it. status
            // stays MINE-only: own_status reads reviewDecision as "what is blocking ME", which is not
            // the question an assigned or requested row asks.
            p.reviewers = reviewers(n);
            if *name == "MINE" {
                p.status = own_status(n);
            }
            prs.push(p);
        }
        prs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out.push(Section {
            name: name.to_string(),
            prs: Some(prs),
            err: None,
        });
    }
    out
}

fn check_state(raw: &str) -> &'static str {
    match raw {
        "SUCCESS" | "COMPLETED" | "NEUTRAL" => "ok",
        "SKIPPED" => "skip",
        "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" => "fail",
        _ => "run",
    }
}

fn detail_query(owner: &str, name: &str, number: u64) -> String {
    format!(
        "{{ repository(owner: {}, name: {}) {{ pullRequest(number: {number}) {{
    headRefName additions deletions changedFiles
    commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ contexts(first: 20) {{ nodes {{
      ... on CheckRun {{ name conclusion status }}
      ... on StatusContext {{ context state }} }} }} }} }} }} }} }} }} }}",
        Value::String(owner.into()),
        Value::String(name.into())
    )
}

/// Branch, diff size and CI checks for one PR. None on any failure.
///
/// ponytail: only ever for the selected row, and one query rather than a pull + a checks call. A pane
/// is decoration: if it cannot be had, the row is still right, so nothing here fails loudly.
pub fn detail(repo: &str, number: u64) -> Option<Detail> {
    if config::get().demo {
        return crate::demo::detail(repo, number);
    }
    let (owner, name) = repo.split_once('/').unwrap_or((repo, ""));
    let d = gql(&detail_query(owner, name, number), 30).ok()?;
    let pr = d.pointer("/repository/pullRequest")?;
    if !pr.is_object() {
        return None;
    }
    Some(detail_of(pr))
}

/// The pane's Detail from a `pullRequest` node.
pub fn detail_of(pr: &Value) -> Detail {
    let mut checks = Vec::new();
    for c in contexts(pr) {
        let name = [str_at(c, "/name"), str_at(c, "/context")]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("");
        let raw = [
            str_at(c, "/conclusion"),
            str_at(c, "/state"),
            str_at(c, "/status"),
        ]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_uppercase();
        if !name.is_empty() {
            checks.push(Check {
                name: name.to_string(),
                state: check_state(&raw).to_string(),
            });
        }
    }
    checks.truncate(8);
    Detail {
        branch: str_at(pr, "/headRefName").to_string(),
        add: pr.get("additions").and_then(Value::as_i64),
        del: pr.get("deletions").and_then(Value::as_i64),
        files: pr.get("changedFiles").and_then(Value::as_i64),
        checks,
    }
}

/// The check runs and status contexts on a PR's head commit, [] when it has none.
pub fn contexts(pr: &Value) -> Vec<&Value> {
    pr.pointer("/commits/nodes")
        .and_then(Value::as_array)
        .and_then(|n| n.first())
        .and_then(|c| c.pointer("/commit/statusCheckRollup/contexts/nodes"))
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

/// The review as GitHub takes it. Separate from the call so the shape can be read in a test without
/// a network: it is the whole of what this feature changes about the request.
fn review_payload(verdict: &str, body: &str, head: &str, inline: &[Inline]) -> Value {
    let mut b = json!({ "event": verdict_event(verdict), "body": body });
    // ponytail: `head` is required for the comments and not merely nice to have. Without a commit to
    // pin them to they would attach to whatever the PR's latest commit is, which for a held review is
    // not the diff the lines were read off. No sha, no comments — the body still goes up.
    if !inline.is_empty() && !head.is_empty() {
        let comments: Vec<Value> = inline
            .iter()
            .map(|c| json!({ "path": c.path, "line": c.line, "body": c.body }))
            .collect();
        b["commit_id"] = json!(head);
        b["comments"] = json!(comments);
    }
    b
}

/// Whether a review by `login` with exactly this body is already on the PR.
///
/// ponytail: an Err from `post_review` means "we did not hear that it landed", which is not the same
/// as "it did not land". A timeout after GitHub accepted the review leaves a hold whose release
/// would post it a second time on someone else's PR (#164). This is the question that tells the two
/// apart, and it is asked of GitHub rather than inferred from the error.
///
/// ponytail: never fails, and false is the answer it gives when it cannot tell. A missed detection
/// posts, which is what happens today; a false detection would drop a review nobody ever sees. The
/// so it defaults to false.
///
/// ponytail: `login` is passed in rather than read from `me()` here, so the matching can be tested
/// without a process-global login cache deciding the outcome.
///
/// ponytail: PAGED, and paged to the end rather than reading the first hundred. GitHub lists reviews
/// OLDEST first, so the review that just landed is on the LAST page — on a PR with more than a
/// hundred reviews, reading one page finds every review except the one being asked about, and posts
/// the duplicate on exactly the busy PR where it is most visible.
///
/// ponytail: `state` is deliberately not checked. PENDING cannot come from here, because every post
/// sets `event`; a DISMISSED review of this body is still a review that was posted, and posting it
/// again because someone dismissed it is the duplicate this exists to prevent.
pub fn mine_already_on(repo: &str, number: u64, login: &str, body: &str) -> bool {
    if config::get().demo || login.is_empty() || body.is_empty() {
        return false;
    }
    // ponytail: a ceiling, so a repo that answers oddly cannot turn one keypress into an unbounded
    // walk. A hundred pages of reviews on one PR is not a case worth staying correct for.
    const PAGES: u32 = 10;
    const PER: usize = 100;
    for page in 1..=PAGES {
        let Ok(Value::Array(rows)) = api(
            &format!("/repos/{repo}/pulls/{number}/reviews?per_page={PER}&page={page}"),
            30,
        ) else {
            return false;
        };
        // ponytail: the WHOLE body, not a prefix. Every review this posts opens with the same few
        // lines for a given verdict, so a prefix would call a fresh review a duplicate of an old one.
        if rows.iter().any(|r| {
            r.pointer("/user/login").and_then(Value::as_str) == Some(login)
                && r.get("body").and_then(Value::as_str) == Some(body)
        }) {
            return true;
        }
        if rows.len() < PER {
            break; // a short page is the last one
        }
    }
    false
}

/// Post the verdict on the PR, with `inline` as comments on the lines they are about. Err on failure.
///
/// ponytail: `comments` is validated as ONE thing, so one line GitHub will not take costs the body too.
pub fn post_review(
    repo: &str,
    number: u64,
    verdict: &str,
    body: &str,
    head: &str,
    inline: &[Inline],
) -> Result<(), Error> {
    let b = review_payload(verdict, body, head, inline);
    call(
        &format!("/repos/{repo}/pulls/{number}/reviews"),
        "POST",
        Some(&b),
        "application/vnd.github+json",
        60,
    )
    .map(|_| ())
}

/// Post a plain comment on the PR. Err on failure.
pub fn comment(repo: &str, number: u64, body: &str) -> Result<(), Error> {
    let b = json!({ "body": body });
    call(
        &format!("/repos/{repo}/issues/{number}/comments"),
        "POST",
        Some(&b),
        "application/vnd.github+json",
        60,
    )
    .map(|_| ())
}

/// The desktop's own opener, through the plugin this app already ships with.
///
/// ponytail: no longer a command line of our own. The Windows arm was `cmd /c start "" <url>`, and
/// cmd re-parses what it is handed: an `&` in a URL ended the command and started another one. The
/// plugin hands the string to ShellExecuteW instead, so on Windows there is no command line to parse
/// at all; on Linux it still tries xdg-open first, then gio, gnome-open and kde-open.
///
/// ponytail: still off this thread. The macOS path waits for `open` to return, and every caller here
/// is a row's keypress, a popup's button or the launch of the GUI — none of them may block on a
/// browser. The plugin reaps whatever it starts, so nothing is left behind either (#124).
pub fn open_in_browser(url: &str) {
    let url = url.to_string();
    std::thread::spawn(move || {
        if let Err(e) = tauri_plugin_opener::open_url(&url, None::<&str>) {
            log::warn!("could not open {url}: {e}");
        }
    });
}

/// A file this app wrote, opened with whatever the desktop uses for it.
///
/// ponytail: the path half of the same door. Both land in the same launcher, but this one stats the
/// file first, so a report that has been deleted says so in one line instead of arriving at the
/// desktop as a path to nothing.
pub fn open_file(path: &std::path::Path) {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        if let Err(e) = tauri_plugin_opener::open_path(&path, None::<&str>) {
            log::warn!("could not open {}: {e}", path.display());
        }
    });
}

const CLIPBOARDS: &[&[&str]] = &[
    &["wl-copy"],
    &["xclip", "-selection", "clipboard"],
    &["xsel", "--clipboard", "--input"],
    &["pbcopy"],
];

/// Python's shutil.which: is `name` an executable on PATH?
fn which(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
        .unwrap_or(false)
}

/// Run `cmd` with `input` on stdin, output discarded; Some(exit ok) or None when it hung past `secs`.
/// ponytail: spawn and poll, kill on expiry. The stdlib has no timeout on `wait`.
pub(crate) fn run_quiet(cmd: &mut Command, input: &str, secs: u64) -> Option<bool> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let start = Instant::now();
    loop {
        if let Ok(Some(st)) = child.try_wait() {
            return Some(st.success());
        }
        if start.elapsed() > Duration::from_secs(secs) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Clipboard via the first tool on PATH (wl-copy, xclip, xsel, pbcopy), else the OSC 52 escape on stdout.
/// Returns what did it ("xclip", "terminal"). ponytail: shell out or one escape, no clipboard library.
pub fn copy(text: &str) -> String {
    if config::get().demo {
        return "demo".into();
    }
    for cmd in CLIPBOARDS {
        if which(cmd[0]) {
            let mut c = Command::new(cmd[0]);
            c.args(&cmd[1..]);
            if run_quiet(&mut c, text, 5) == Some(true) {
                return cmd[0].to_string();
            }
            // ponytail: a tool that failed (xclip with no DISPLAY) or hung: try the next one, else the escape
        }
    }
    let mut out = std::io::stdout();
    let _ = write!(
        out,
        "\x1b]52;c;{}\x07",
        base64::engine::general_purpose::STANDARD.encode(text)
    );
    let _ = out.flush();
    "terminal".into()
}

/// chars; a diff bigger than this is cut, since a context window is not free
pub const PR_CONTEXT_MAX: usize = 200_000;
const CONTEXT: &[(&str, &str)] = &[
    ("title", "title"),
    ("body", "body"),
    ("additions", "additions"),
    ("deletions", "deletions"),
    ("changed_files", "changedFiles"),
    ("base", "baseRefName"),
    ("head", "headRefName"),
    ("user", "author"),
    ("labels", "labels"),
];

/// The PR and its diff as one blob for a model that cannot read GitHub itself. Cut at 200k chars.
pub fn context(repo: &str, number: u64) -> Result<String, Error> {
    let pr = api(&format!("/repos/{repo}/pulls/{number}"), 120)?;
    let diff = diff(repo, number)?;
    Ok(context_text(&pr, &diff, PR_CONTEXT_MAX))
}

/// Python's str()/json.dumps of a value: strings bare, lists as JSON with ", " between items.
fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", ")
        ),
        other => other.to_string(),
    }
}

/// The context blob from a REST pull object and its diff, cut at `max` chars.
pub fn context_text(pr: &Value, diff: &str, max: usize) -> String {
    let flat = |key: &str| -> Option<Value> {
        match key {
            "user" => pr.pointer("/user/login").cloned(),
            "base" => pr.pointer("/base/ref").cloned(),
            "head" => pr.pointer("/head/ref").cloned(),
            "labels" => Some(Value::Array(
                pr.get("labels")
                    .and_then(Value::as_array)
                    .map(|l| {
                        l.iter()
                            .map(|x| x.get("name").cloned().unwrap_or(Value::Null))
                            .collect()
                    })
                    .unwrap_or_default(),
            )),
            _ => pr.get(key).cloned(),
        }
    };
    let lines: Vec<String> = CONTEXT
        .iter()
        .filter_map(|(key, name)| {
            flat(key)
                .filter(|v| !v.is_null())
                .map(|v| format!("{name}: {}", py_str(&v)))
        })
        .collect();
    let text = format!("{}\n\n{diff}", lines.join("\n"));
    if text.chars().count() > max {
        text.chars().take(max).collect::<String>() + "\n\n[diff truncated]"
    } else {
        text
    }
}

/// The sha the PR's head points at right now, "" when it cannot be read.
///
/// ponytail: `diff()` is `GET /pulls/{n}`, which always answers with the PR as it stands — the sha
/// `diff::fetch` is keyed on is a CACHE key and never reaches GitHub. Anything that pins a commit to
/// the lines it read has to ask what the head is now and compare, or it pins one commit to another
/// commit's line numbers.
pub fn head_sha(repo: &str, number: u64) -> String {
    if config::get().demo {
        return String::new();
    }
    api(&format!("/repos/{repo}/pulls/{number}"), 30)
        .ok()
        .and_then(|v| v.pointer("/head/sha").and_then(Value::as_str).map(String::from))
        .unwrap_or_default()
}

/// The raw unified diff of a PR (Accept: application/vnd.github.v3.diff).
pub fn diff(repo: &str, number: u64) -> Result<String, Error> {
    if config::get().demo {
        // PORT-NOTE: demo::diff_text(repo, number) does not exist in src/demo.rs yet (Python's demo swapped
        // diff.fetch for a canned DIFF). Until it does, the demo diff is empty here.
        let _ = (repo, number);
        return Ok(String::new());
    }
    call(
        &format!("/repos/{repo}/pulls/{number}"),
        "GET",
        None,
        "application/vnd.github.v3.diff",
        120,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_of_reads_the_addressed_repo() {
        for (path, want) in [
            ("/repos/acme/api/pulls/7", "acme/api"),
            ("repos/acme/api", "acme/api"),
            ("/search/code", ""),
            ("/user/repos", ""),
            ("/repos/acme", ""),
            ("/repos", ""),
        ] {
            assert_eq!(repo_of(path), want);
        }
    }

    #[test]
    fn scoped_lets_the_repo_under_review_through() {
        for (path, want) in [
            ("/repos/acme/api/pulls/7", "/repos/acme/api/pulls/7"),
            ("repos/acme/api/pulls/7", "/repos/acme/api/pulls/7"),
            ("/repos/ACME/API/pulls/7", "/repos/ACME/API/pulls/7"),
            ("/repos/acme/api", "/repos/acme/api"),
            (
                "/repos/acme/api/contents/x.py?ref=feat",
                "/repos/acme/api/contents/x.py?ref=feat",
            ),
        ] {
            assert_eq!(scoped(path, "acme/api", "", "").unwrap(), want);
        }
        assert_eq!(scoped("/user/repos", "", "", "").unwrap(), "/user/repos");
    }

    #[test]
    fn scoped_refuses_everything_outside_it() {
        for path in [
            "/repos/some-other-org/private-repo/contents/.env",
            "/user/repos?per_page=100",
            "/repos/acme/api-secrets/contents/.env",
            "/repos/acme/apifoo",
            "/orgs/acme/members",
            "/search/repositories?q=acme",
            "/gists",
        ] {
            let err = scoped(path, "acme/api", "", "").unwrap_err();
            assert!(err.contains("acme/api"), "{err}");
        }
        assert!(scoped("/search/repositories?q=acme", "acme/api", "", "")
            .unwrap_err()
            .contains("not /search/code"));
        let err = scoped(
            "/repos/acme/shared-lib/contents/x.py",
            "acme/api",
            "acme/platform",
            "",
        )
        .unwrap_err();
        assert!(err.contains("bound to acme/platform") && err.contains("outside it"));
    }

    #[test]
    fn scoped_lets_the_db_repo_through_and_nothing_beside_it() {
        assert!(scoped(
            "/repos/acme/schema/contents/001.sql",
            "acme/api",
            "",
            "acme/schema"
        )
        .is_ok());
        assert!(scoped("/repos/Acme/Schema/git/trees/main", "acme/api", "", "acme/schema").is_ok());
        // the bare repo, which the prompt tells the model to start with
        assert!(scoped("/repos/acme/schema", "acme/api", "", "acme/schema").is_ok());
        assert!(scoped("/repos/acme/other", "acme/api", "", "acme/schema")
            .unwrap_err()
            .contains("its DB repo acme/schema"));
        for path in [
            "/repos/acme/schema-secrets/contents/x",
            "/repos/acme/schema.git/contents/x",
            "/repos/acme/other/contents/x",
        ] {
            assert!(scoped(path, "acme/api", "", "acme/schema").is_err(), "{path}");
        }
        assert!(scoped("/repos/acme/schema/contents/x", "acme/api", "", "").is_err());
    }

    #[test]
    fn a_dot_segment_is_refused_whatever_its_spelling() {
        for path in [
            "/repos/acme/api/../../user",
            "/repos/acme/api/%2e%2e/%2e%2e/user",
            "/repos/acme/api/%2E%2E/user",
            "/repos/acme/api/contents/src/../../../../repos/other/x",
            // a segment that decodes to separators of its own: one segment going in, a traversal coming out
            "/repos/acme/api/..%2f..%2f..%2fuser",
            "/repos/acme/api%2f..%2fother",
            "/repos/acme/api/..%5c..%5cuser",
        ] {
            assert!(scoped(path, "acme/api", "acme-platform", "")
                .unwrap_err()
                .contains(".."));
        }
    }

    fn q(path: &str) -> String {
        let got = scoped(path, "acme/api", "", "").unwrap();
        parse_qsl(got.split_once('?').unwrap().1)
            .into_iter()
            .find(|(k, _)| k == "q")
            .unwrap()
            .1
    }

    #[test]
    fn scoped_forces_the_repo_into_a_code_search() {
        assert_eq!(q("/search/code?q=parseToken"), "parseToken repo:acme/api");
        assert_eq!(
            q("/search/code?q=parseToken+repo:acme/api"),
            "parseToken repo:acme/api"
        );
        assert_eq!(q("/search/code/?q=x"), "x repo:acme/api");
        for hostile in [
            "/search/code?q=AWS_SECRET+user:victim",
            "/search/code?q=x+repo:other/repo",
            "/search/code?q=x+org:victim",
            "/search/code?q=x+owner:victim",
        ] {
            assert!(scoped(hostile, "acme/api", "", "")
                .unwrap_err()
                .contains("acme/api"));
        }
        for empty in ["/search/code?q=", "/search/code", "/search/code?q=repo:acme/api"] {
            assert!(scoped(empty, "acme/api", "", "")
                .unwrap_err()
                .contains("something to search for"));
        }
        let got = scoped("/search/code?q=parseToken&per_page=5&page=2", "acme/api", "", "").unwrap();
        assert!(got.contains("per_page=5") && got.contains("page=2") && got.contains("repo%3Aacme%2Fapi"));
        assert_eq!(
            got,
            "/search/code?q=parseToken+repo%3Aacme%2Fapi&per_page=5&page=2"
        );
    }

    #[test]
    fn query_asks_for_the_three_sections_under_my_own_login() {
        let s = searches("me", "");
        assert_eq!(
            s,
            [
                (0, "is:pr is:open author:me".to_string()),
                (1, "is:pr is:open review-requested:me".to_string()),
                (2, "is:pr is:open assignee:me".to_string())
            ],
            "no TEAM or MERGED search while nothing is toggled on"
        );
        let p = page(&s[0].1, "");
        assert!(p.contains("search(query: \"is:pr is:open author:me\", type: ISSUE, first: 20)"));
        assert!(p.contains("headRefOid") && p.contains("latestReviews") && p.contains("endCursor"));
        assert!(page("x", "Y3Vy").contains("first: 20, after: \"Y3Vy\")"));
    }

    #[test]
    fn a_search_pages_by_cursor_and_keeps_what_arrived_when_a_later_page_fails() {
        let pages = std::cell::RefCell::new(Vec::new());
        let page_of = |n: usize, next: bool| {
            json!({"s": {"pageInfo": {"hasNextPage": next, "endCursor": format!("c{n}")},
                         "nodes": (0..PAGE).map(|k| node(&format!("{n}-{k}"), json!({}))).collect::<Vec<_>>()}})
        };
        // three full pages then a failure: those kept, no error, cursors passed along
        let got = search_all("q", |doc| {
            pages.borrow_mut().push(doc.to_string());
            match pages.borrow().len() {
                n @ 1..=3 => Ok(page_of(n, true)),
                _ => Err(Error("502 Bad Gateway".into())),
            }
        })
        .unwrap();
        assert_eq!(got["nodes"].as_array().unwrap().len(), 3 * PAGE);
        assert!(pages.borrow()[1].contains("after: \"c1\"") && pages.borrow()[3].contains("after: \"c3\""));
        // never more than MOST, and it stops when GitHub says there is no next page
        assert_eq!(
            search_all("q", |_| Ok(page_of(1, true))).unwrap()["nodes"]
                .as_array()
                .unwrap()
                .len(),
            MOST
        );
        let calls = std::cell::Cell::new(0);
        search_all("q", |_| {
            calls.set(calls.get() + 1);
            Ok(page_of(1, false))
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
        // a null search is an error, not an empty section
        assert!(search_all("q", |_| Ok(json!({"s": null}))).is_err());
        // a failed first page is the section's error
        assert_eq!(
            search_all("q", |_| Err(Error("502 Bad Gateway".into())))
                .unwrap_err()
                .0,
            "502 Bad Gateway"
        );
    }

    #[test]
    fn team_scope_expands_to_one_ored_search_and_skips_junk() {
        let repos = HashMap::from([
            ("acme/api".to_string(), "core".to_string()),
            ("x/y".to_string(), "other".to_string()),
        ]);
        let owners = HashMap::from([("acme".to_string(), "core".to_string())]);
        let on = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            scope_terms(
                &on(&["team:core", "org:acme", "org:a\" b", "team:nope"]),
                &repos,
                &owners
            ),
            "org:acme repo:acme/api"
        );
        assert_eq!(scope_terms(&[], &repos, &owners), "");
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-14T12:00:00Z")
            .unwrap()
            .to_utc();
        assert_eq!(
            within("org:acme", Some(24), now),
            "org:acme updated:>=2026-09-13T12:00:00Z"
        );
        assert_eq!(
            (
                within("org:acme", None, now).as_str(),
                within("", Some(24), now).as_str()
            ),
            ("org:acme", "")
        );
        let q = searches("me", "org:acme");
        assert_eq!(q[3], (3, "is:pr is:open org:acme sort:updated-desc".to_string()));
        assert_eq!(
            q[4],
            (4, "is:pr is:merged org:acme sort:updated-desc".to_string())
        );
        assert!(page(&searches("me", "org:a\"b")[3].1, "")
            .contains(r#""is:pr is:open org:a\"b sort:updated-desc""#));
        let secs = sections_of(
            &json!({"s0": {"nodes": []}, "s1": {"nodes": []}, "s2": {"nodes": []}, "s3": {"nodes": [node("t", json!({}))]}, "s4": {"nodes": []}}),
        );
        assert_eq!(
            secs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["MINE", "REVIEW REQUESTED", "ASSIGNED", "TEAM", "MERGED"]
        );
        let opts = scope_options(
            &on(&["org:gone", "org:acme"]),
            &secs,
            &["core".to_string()],
            &repos,
            &owners,
        );
        assert_eq!(opts, ["team:core", "org:a", "org:acme", "org:x", "org:gone"]);
        let failed = sections_of(&json!({"s0": null, "s1": {"nodes": []}, "s2": {"nodes": []}}));
        assert_eq!(
            (failed[0].prs.is_none(), failed[0].err.as_deref(), failed.len()),
            (true, Some("search failed"), 3)
        );
    }

    #[test]
    fn fetched_keeps_dropped_scopes_up_to_the_cap_and_never_drops_saved_ones() {
        let v = |n: std::ops::Range<usize>| n.map(|i| format!("org:o{i}")).collect::<Vec<_>>();
        // toggled off: still searched; toggled on: appended
        assert_eq!(merge_fetched(&v(1..2), &v(0..1)), v(0..2));
        // over the cap: the oldest unsaved goes first
        let merged = merge_fetched(&v(8..9), &v(0..8));
        assert_eq!(merged, [v(1..8), v(8..9)].concat());
        // everything saved: nothing dropped
        assert_eq!(merge_fetched(&v(0..10), &[]).len(), 10);
    }

    #[test]
    fn own_status_reads_the_decision() {
        assert_eq!(
            own_status(&json!({"reviewDecision": "CHANGES_REQUESTED", "reviewRequests": {"totalCount": 1}})),
            "↻ re-review requested"
        );
        assert_eq!(
            own_status(&json!({"reviewDecision": "CHANGES_REQUESTED", "reviewRequests": {"totalCount": 0}})),
            "✗ changes requested"
        );
        assert_eq!(
            own_status(&json!({"reviewDecision": "REVIEW_REQUIRED", "reviewRequests": {"totalCount": 2}})),
            "· awaiting review"
        );
        assert_eq!(own_status(&json!({})), "");
    }

    #[test]
    fn checks_glyphs() {
        let node = |s: Value| json!({"commits": {"nodes": [{"commit": {"statusCheckRollup": s}}]}});
        assert_eq!(checks(&node(json!({"state": "PENDING"}))), "●");
        assert_eq!(checks(&node(json!({"state": "FAILURE"}))), "✗");
        assert_eq!(checks(&node(json!({"state": "SUCCESS"}))), "✓");
        assert_eq!(checks(&node(Value::Null)), "");
        assert_eq!(checks(&json!({})), "");
    }

    #[test]
    fn reviewers_merges_requests_over_latest_reviews() {
        let node = json!({"latestReviews": {"nodes": [{"author": {"login": "bob"}, "state": "APPROVED"},
                                    {"author": {"login": "carol"}, "state": "CHANGES_REQUESTED"}, null]},
            "reviewRequests": {"nodes": [{"requestedReviewer": {"login": "alice"}},
                                     {"requestedReviewer": {"login": "carol"}},
                                     {"requestedReviewer": {}}, {"requestedReviewer": null}]}});
        assert_eq!(reviewers(&node), "✓bob ·carol ·alice");
        assert_eq!(reviewers(&json!({})), "");
        let commented = json!({"latestReviews": {"nodes": [{"author": {"login": "bob"}, "state": "COMMENTED"}]},
            "reviewRequests": {"nodes": [{"requestedReviewer": {"login": "bob"}}]}});
        assert_eq!(reviewers(&commented), "~bob");
        let dismissed = json!({"latestReviews": {"nodes": [{"author": {"login": "bob"}, "state": "DISMISSED"}]},
            "reviewRequests": {"nodes": [{"requestedReviewer": {"login": "bob"}}]}});
        assert_eq!(reviewers(&dismissed), "·bob");
    }

    fn node(url: &str, extra: Value) -> Value {
        let mut n = json!({"number": 7, "title": "t", "url": url, "updatedAt": "2020-01-01T00:00:00Z",
            "isDraft": false, "author": {"login": "me"}, "repository": {"nameWithOwner": "a/b", "name": "b"}});
        for (k, v) in extra.as_object().unwrap() {
            n[k] = v.clone();
        }
        n
    }

    #[test]
    fn sections_dedup_sort_and_join_ci_head_and_status() {
        let a = node(
            "a",
            json!({"updatedAt": "2020-01-01T00:00:00Z", "reviewDecision": "CHANGES_REQUESTED",
            "reviewRequests": {"totalCount": 1}, "headRefOid": "aaa",
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "FAILURE"}}}]}}),
        );
        let b = node(
            "b",
            json!({"updatedAt": "2021-01-01T00:00:00Z", "reviewDecision": "APPROVED", "additions": 12, "deletions": 3,
            "latestReviews": {"nodes": [{"author": {"login": "erin"}, "state": "COMMENTED"}]}}),
        );
        let secs =
            sections_of(&json!({"s0": {"nodes": [a, b]}, "s1": {"nodes": [a, null]}, "s2": {"nodes": []}}));
        assert_eq!(
            secs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["MINE", "REVIEW REQUESTED", "ASSIGNED"]
        );
        let mine = secs[0].prs.as_ref().unwrap();
        assert_eq!(
            mine.iter().map(|p| p.url.as_str()).collect::<Vec<_>>(),
            ["b", "a"]
        );
        assert_eq!(
            mine.iter().map(|p| p.status.as_str()).collect::<Vec<_>>(),
            ["✓ approved", "↻ re-review requested"]
        );
        assert_eq!(
            (
                mine[0].repo(),
                mine[0].number,
                mine[0].reviewers.as_str(),
                mine[0].head.as_str()
            ),
            ("a/b", 7, "~erin", "")
        );
        assert_eq!((mine[1].head.as_str(), mine[1].checks.as_str()), ("aaa", "✗"));
        // the graph sizes nodes from these, with no fetch of its own
        assert_eq!(
            (mine[0].additions, mine[0].deletions, mine[1].additions),
            (Some(12), Some(3), None)
        );
        assert_eq!(secs[1].prs.as_ref().unwrap().len(), 0);
        let row = serde_json::to_value(&mine[0]).unwrap();
        assert!(row.get("head").is_none() && row.get("checks").is_none());
        // not MINE: no status
        let secs = sections_of(
            &json!({"s0": {"nodes": []}, "s1": {"nodes": [node("b", json!({"headRefOid": "bbb"}))]}}),
        );
        let b = &secs[1].prs.as_ref().unwrap()[0];
        assert_eq!(
            (b.head.as_str(), b.checks.as_str(), b.status.as_str()),
            ("bbb", "", "")
        );
    }

    #[test]
    fn detail_normalises_checks() {
        let pr = json!({"headRefName": "feat/kb", "additions": 412, "deletions": 96, "changedFiles": 14,
          "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": [
              {"name": "ci", "conclusion": "SUCCESS"}, {"name": "lint", "conclusion": "FAILURE"},
              {"context": "e2e", "state": "PENDING"}, {"name": "odd", "conclusion": "WHAT"},
              {"conclusion": "SUCCESS"}]}}}}]}});
        let d = detail_of(&pr);
        assert_eq!(
            (d.branch.as_str(), d.add, d.del, d.files),
            ("feat/kb", Some(412), Some(96), Some(14))
        );
        let want: Vec<(&str, &str)> = vec![("ci", "ok"), ("lint", "fail"), ("e2e", "run"), ("odd", "run")];
        assert_eq!(
            d.checks
                .iter()
                .map(|c| (c.name.as_str(), c.state.as_str()))
                .collect::<Vec<_>>(),
            want
        );
        let q = detail_query("a", "b", 7);
        assert!(q.contains("repository(owner: \"a\", name: \"b\")") && q.contains("pullRequest(number: 7)"));
        assert_eq!(detail_of(&json!({})).checks.len(), 0);
    }

    #[test]
    fn context_carries_the_pr_and_its_diff_and_truncates() {
        let pr = json!({"title": "T", "body": "why", "additions": 1, "deletions": 2, "changed_files": 3,
            "user": {"login": "kim"}, "base": {"ref": "main"}, "head": {"ref": "feat"}, "labels": [{"name": "bug"}]});
        let text = context_text(&pr, "diff --git a b", PR_CONTEXT_MAX);
        assert!(text.contains("title: T\nbody: why\nadditions: 1") && text.contains("author: kim"));
        assert!(text.contains("baseRefName: main") && text.contains("headRefName: feat"));
        assert!(text.contains("labels: [\"bug\"]") && text.ends_with("\n\ndiff --git a b"));
        let text = context_text(&json!({"title": "t"}), &"x".repeat(1000), 100);
        assert!(text.ends_with("[diff truncated]") && text.len() < 200);
    }

    /// The stale header a clone left in .git/config must not survive into today's pull.
    #[test]
    fn auth_env_replaces_the_header_a_clone_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str], env: bool| {
            let mut c = std::process::Command::new("git");
            c.args(["-C", dir.path().to_str().unwrap()]).args(args);
            if env {
                c.envs(auth_env("Basic now"));
            }
            String::from_utf8(c.output().unwrap().stdout).unwrap()
        };
        run(&["init", "-q"], false);
        run(
            &[
                "config",
                "--add",
                &format!("http.{GITHUB}.extraHeader"),
                "Authorization: Basic stale",
            ],
            false,
        );
        let got = run(
            &["config", "--get-all", &format!("http.{GITHUB}.extraHeader")],
            true,
        );
        // The env values come AFTER the repo's, and the empty one sits between them: git's http
        // layer drops every header it has collected when it reads an empty extraHeader, so the
        // request carries "now" alone. `config --get-all` does not apply that rule, it only shows
        // the order, and the order is the half this can break.
        assert_eq!(
            got.lines().collect::<Vec<_>>(),
            ["Authorization: Basic stale", "", "Authorization: Basic now"]
        );
    }

    #[test]
    fn persist_auth_writes_the_token_into_the_checkout_not_argv() {
        assert_eq!(header_for(""), "");
        let h = header_for("gho_x");
        assert_eq!(h, "Basic eC1hY2Nlc3MtdG9rZW46Z2hvX3g=");
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".git").join("config");
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        std::fs::write(&cfg, "[core]\n").unwrap();
        persist_header(dir.path(), &h);
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("[http \"https://github.com/\"]\n\textraHeader = Authorization: Basic eC1hY2Nlc3MtdG9rZW46Z2hvX3g="));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        persist_header(&dir.path().join("nope"), &h); // a failed clone left no config: nothing to write
        std::fs::write(&cfg, "[core]\n").unwrap();
        persist_header(dir.path(), ""); // no token means no header anywhere
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "[core]\n");
    }

    #[test]
    fn urlencode_matches_python() {
        assert_eq!(
            urlencode(&[("q".into(), "a b repo:x/y".into())]),
            "q=a+b+repo%3Ax%2Fy"
        );
        assert_eq!(
            parse_qsl("q=a+b&x=&y=%2F"),
            vec![
                ("q".to_string(), "a b".to_string()),
                ("y".to_string(), "/".to_string())
            ]
        );
    }

    #[test]
    fn a_review_with_no_inline_comments_is_the_request_it_always_was() {
        let b = review_payload("approve", "looks good", "abc123", &[]);
        assert_eq!(b["event"], "APPROVE");
        assert_eq!(b["body"], "looks good");
        assert!(b.get("comments").is_none());
        assert!(b.get("commit_id").is_none());
    }

    #[test]
    fn inline_comments_ride_along_with_the_body_and_pin_the_commit() {
        let c = |path: &str, line: u32| Inline {
            path: path.into(),
            line,
            body: "**nit** — x".into(),
        };
        let b = review_payload(
            "request_changes",
            "the body",
            "e3c11f3",
            &[c("internal/services/user_service.go", 152)],
        );
        // the body is untouched: findings appear in it AND on the lines
        assert_eq!(b["body"], "the body");
        assert_eq!(b["commit_id"], "e3c11f3");
        assert_eq!(b["comments"][0]["path"], "internal/services/user_service.go");
        assert_eq!(b["comments"][0]["line"], 152);
        // ponytail: never sent. The mapping cannot anchor to a deletion, so RIGHT is the only side
        // there is and naming it would be a second place to keep that true.
        assert!(b["comments"][0].get("side").is_none());
    }

    #[test]
    fn without_a_head_to_pin_them_to_the_comments_are_left_off() {
        let b = review_payload(
            "comment",
            "the body",
            "",
            &[Inline {
                path: "a.rs".into(),
                line: 3,
                body: "x".into(),
            }],
        );
        assert_eq!(b["body"], "the body");
        assert!(b.get("comments").is_none());
    }

    #[test]
    fn only_your_own_review_with_the_very_same_body_counts_as_already_there() {
        let _g = crate::config::test_lock();
        let body = "the whole review, word for word";
        let api = crate::testapi::Api::start(vec![(
            "/pulls/7/reviews",
            None,
            crate::testapi::Reply::new(
                200,
                &serde_json::json!([
                    {"user": {"login": "someone-else"}, "body": body},
                    {"user": {"login": "me"}, "body": "a different review of mine"},
                    {"user": {"login": "me"}, "body": body},
                ])
                .to_string(),
            ),
        )]);
        let _p = crate::testapi::Pointed::at(&api);

        assert!(mine_already_on("acme/api", 7, "me", body));
        // the same words from someone else are not this review having landed
        assert!(!mine_already_on("acme/api", 7, "nobody", body));
        // and a body that is not on the PR is not there whoever wrote it
        assert!(!mine_already_on(
            "acme/api",
            7,
            "me",
            "a review that never went up"
        ));
        // ponytail: a prefix would call a fresh review a duplicate of an older one; every review
        // this posts opens the same way for a given verdict.
        assert!(!mine_already_on("acme/api", 7, "me", "the whole review"));
        // nothing to compare is not a match
        assert!(!mine_already_on("acme/api", 7, "me", ""));
        assert!(!mine_already_on("acme/api", 7, "", body));
    }

    /// GitHub lists reviews oldest first, so the one that just landed is on the LAST page. Reading
    /// only the first hundred finds every review except the one being asked about.
    #[test]
    fn a_match_on_a_later_page_is_still_found() {
        let _g = crate::config::test_lock();
        let body = "the review that landed";
        let filler: Vec<Value> = (0..100)
            .map(|i| serde_json::json!({"user": {"login": "me"}, "body": format!("older review {i}")}))
            .collect();
        let api = crate::testapi::Api::start(vec![
            (
                "/pulls/9/reviews?per_page=100&page=1",
                None,
                crate::testapi::Reply::new(200, &Value::Array(filler).to_string()),
            ),
            (
                "/pulls/9/reviews?per_page=100&page=2",
                None,
                crate::testapi::Reply::new(
                    200,
                    &serde_json::json!([{"user": {"login": "me"}, "body": body}]).to_string(),
                ),
            ),
        ]);
        let _p = crate::testapi::Pointed::at(&api);

        assert!(mine_already_on("acme/api", 9, "me", body));
        assert!(api.saw("GET", "page=2"), "it walked past the first page");
    }

    /// A full page that is the whole story: nothing after it, and nothing to find.
    #[test]
    fn a_walk_stops_at_the_first_short_page() {
        let _g = crate::config::test_lock();
        let api = crate::testapi::Api::start(vec![(
            "/pulls/10/reviews?per_page=100&page=1",
            None,
            crate::testapi::Reply::new(
                200,
                &serde_json::json!([{"user": {"login": "me"}, "body": "something else"}]).to_string(),
            ),
        )]);
        let _p = crate::testapi::Pointed::at(&api);

        assert!(!mine_already_on("acme/api", 10, "me", "not there"));
        assert!(!api.saw("GET", "page=2"), "a short page ends the walk");
    }

    #[test]
    fn a_reviews_listing_it_cannot_read_is_not_a_match() {
        let _g = crate::config::test_lock();
        let api = crate::testapi::Api::start(vec![(
            "/pulls/8/reviews",
            None,
            crate::testapi::Reply::new(500, r#"{"message":"server error"}"#),
        )]);
        let _p = crate::testapi::Pointed::at(&api);
        // ponytail: false, so the release posts. A missed detection is today's behaviour; a false
        // one drops a review nobody ever sees.
        assert!(!mine_already_on("acme/api", 8, "me", "body"));
    }

    #[test]
    fn verdict_events_and_host() {
        assert_eq!(verdict_event("request_changes"), "REQUEST_CHANGES");
        assert_eq!(verdict_event("approve"), "APPROVE");
        assert_eq!(host_of("https://API.github.com/x"), "api.github.com");
        assert_eq!(host_of("https://evil.example.com/collect"), "evil.example.com");
    }
}
