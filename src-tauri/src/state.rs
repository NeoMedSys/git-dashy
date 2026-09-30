//! Background refresh loop and everything the UI reads. Port of dashy/core/state.py.

use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::{debug, error, info};

use crate::types::{Detail, DiffFile, Finding, Mark, Pr, Section};
use crate::{
    autorev, config, diff, github, heartbeat, held, install, log as review_log, memory, mirror, review, team,
    update,
};

/// The desktop popup's icon. Python pointed notify-send at dashy/notify.png on disk; the binary
/// carries it and writes it out once, beside the other temp files.
pub const NOTIFY_ICON: &[u8] = include_bytes!("../ui/notify.png");

/// How long a popup waits for its Open click before notify-send is killed, so an undismissed one cannot hold a thread forever.
const NOTIFY_WAIT: Duration = Duration::from_secs(600);

/// Seconds since the epoch, as Python's time.time().
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The detail cache key: (url, updatedAt).
pub type DetailKey = (String, String);
/// The diff cache key: (repo, number, head, findings signature, diff generation).
pub type DiffKey = (String, u64, String, Vec<(String, String, String)>, u64);
/// One PR's parsed diff and the marks anchored onto it.
pub type DiffCache = (Vec<DiffFile>, Vec<Mark>);

/// The row's status while a spell runs. end_spell restores the row only while it still says this.
const CASTING: &str = "casting spell...";

/// Seconds a wake waits after the previous tick, so a burst of finished reviews refetches once.
const WAKE_GAP: f64 = 10.0;

/// An event the refresh loop sleeps on: `set` wakes it, `wait` returns true when it was set.
#[derive(Default)]
pub struct Wake {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Wake {
    pub fn set(&self) {
        *self.flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.cv.notify_all();
    }
    pub fn clear(&self) {
        *self.flag.lock().unwrap_or_else(|e| e.into_inner()) = false;
    }
    pub fn is_set(&self) -> bool {
        *self.flag.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// True when set (now or within `d`), false when the time ran out.
    pub fn wait(&self, d: Duration) -> bool {
        let guard = self.flag.lock().unwrap_or_else(|e| e.into_inner());
        if *guard {
            return true;
        }
        let (guard, _) = self.cv.wait_timeout(guard, d).unwrap_or_else(|e| e.into_inner());
        *guard
    }
}

/// Shared between the refresh thread, the review threads and the HTTP handlers.
#[derive(Default)]
pub struct Inner {
    pub sections: Vec<Section>,
    pub fetched_at: Option<f64>,
    pub fetching: bool,
    /// Ticks finished, landed or failed, since start. `f` spins until the count it was answered with.
    pub ticks: u64,
    /// Why the last refresh failed, "" while ticks are landing. See run_loop.
    pub error: String,
    pub auto: bool,
    /// The REVIEW REQUESTED urls with no verdict or review in flight, as of the last tick.
    pub pending: Vec<String>,
    /// url -> status string of a running or finished review this session started.
    pub reviews: HashMap<String, String>,
    /// urls with a review or pre-review in flight. See in_flight().
    pub running: HashSet<String>,
    /// url -> when it started, so a row in flight can say how long it has been.
    pub since: HashMap<String, f64>,
    /// Newer released version, refreshed with each fetch.
    pub update: String,
    /// ponytail: since this dashboard STARTED, and not persisted. The question a badge answers is
    /// "has anything landed that I have not looked at", and a dashboard the operator leaves running
    /// for days is exactly where that goes unnoticed. Persisting it would mean a file that changes
    /// whenever the team does, inside a directory push_dir commits, for a number that is only ever
    /// read by the header of the process that wrote it.
    pub arrived: HashMap<String, usize>,
    pub details: HashMap<DetailKey, Option<Detail>>,
    pub detailing: HashSet<DetailKey>,
    pub diffs: HashMap<DiffKey, Option<DiffCache>>,
    pub diffing: HashSet<DiffKey>,
    /// RR urls present when auto was switched on; None while auto is off.
    pub auto_baseline: Option<HashSet<String>>,
    /// The auto-review scope as of the last tick, so the tick can see what just became covered.
    /// None until the first tick, which counts as "everything was covered" — `set_auto` has already
    /// baselined by then.
    pub auto_scope: Option<autorev::Scope>,
    /// ponytail: the PR's updatedAt (or head) when a finished status was last seen. A verdict
    /// describes one revision; once the PR moves, it is stale and must stop masking what GitHub now says.
    pub seen_at: HashMap<String, String>,
    /// url -> when a status landed, so an older in-flight fetch cannot baseline it.
    pub done_at: HashMap<String, f64>,
    /// urls wanted from me at the last fetch; None until the first fetch lands.
    pub known: Option<HashSet<String>>,
    /// ponytail: one draft sweep at a time; see start_sweep.
    pub sweeping: bool,
    /// The launch-time consent questions the page still shows (web::launch_asks).
    pub asks: Vec<serde_json::Value>,
    /// Lines the page shows once and acknowledges.
    pub notices: Vec<String>,
    /// Release notes since the last version run, shown once after an update. See update::changelog.
    pub changelog: String,
    /// The session token the server answers on; a re-exec after an update keeps it.
    pub token: String,
    pub wake: Arc<Wake>,
}

impl Inner {
    fn rr_urls(&self) -> Vec<String> {
        self.sections
            .iter()
            .filter(|s| s.name == "REVIEW REQUESTED")
            .flat_map(|s| s.prs.iter().flatten())
            .map(|p| p.url.clone())
            .collect()
    }
    /// Review-requested urls with no verdict or review in flight.
    /// The RR urls auto would actually start, so the number someone consents to is the number that
    /// will run. ponytail: scope-filtered — the "Also review the N already listed?" prompt counted
    /// PRs in repos auto is not armed for, and that is the count the answer is given against.
    pub fn pending_rr(&self, armed: &dyn Fn(&str) -> bool) -> Vec<String> {
        self.rr_prs()
            .into_iter()
            .filter(|p| !self.reviews.contains_key(&p.url) && armed(p.repo()) && trusted(p))
            .map(|p| p.url)
            .collect()
    }
    /// Take up a new auto-review scope: whatever it newly covers joins the baseline, so widening
    /// never fires a batch — whoever widened it, and from whichever process.
    ///
    /// ponytail: a method rather than four lines in tick(), because the test for it was otherwise
    /// a copy of those lines asserting that `None.as_mut()` does nothing, which no change to this
    /// crate could ever fail.
    pub fn absorb_scope(&mut self, scope: &autorev::Scope) {
        // no previous scope is the first tick: everything counted as covered, and set_auto has
        // already baselined by then, so nothing here is new
        let prev = self.auto_scope.replace(scope.clone()).unwrap_or_default();
        let newly = newly_covered(&self.rr_prs(), &prev, scope);
        if let Some(b) = self.auto_baseline.as_mut() {
            b.extend(newly);
        }
    }

    fn rr_prs(&self) -> Vec<Pr> {
        self.sections
            .iter()
            .filter(|s| s.name == "REVIEW REQUESTED")
            .flat_map(|s| s.prs.iter().flatten())
            .cloned()
            .collect()
    }
}

/// The urls that just came into scope: listed now, not covered before, covered now.
///
///
/// They join the baseline, so widening never fires a batch — whoever widened it. `a` asks before
/// reviewing a backlog and shows the count; arming a repo goes through no such gate, and the writer
/// may be another process entirely, which is why this is the tick's job and not the route's.
///
/// ponytail: a function, because the block it came from is inside tick(), which fetches. The route
/// used to do this and the CLI path fired the batch anyway.
fn newly_covered(rr: &[Pr], prev: &autorev::Scope, now: &autorev::Scope) -> Vec<String> {
    rr.iter()
        .filter(|p| !prev.armed(p.repo()) && now.armed(p.repo()))
        .map(|p| p.url.clone())
        .collect()
}

/// The review-requested PRs auto should start on this tick.
///
/// Four conditions, and each has cost a bug: new since auto was switched on (or it reviews the
/// backlog you were already ignoring), no verdict and none in flight (or it reviews the same head
/// twice), in a repo auto is armed for (or one `a` reviews every repo the token can see), and opened
/// by someone with standing in that repo (see `trusted`).
///
/// ponytail: a function because the loop it came from is inside tick(), which fetches. Nothing could
/// drive it, and `armed` is the third condition to be added there — the first two were never tested.
fn auto_starts(
    rr: Vec<Pr>,
    baseline: &HashSet<String>,
    reviews: &HashMap<String, String>,
    armed: &dyn Fn(&str) -> bool,
) -> Vec<Pr> {
    rr.into_iter()
        .filter(|p| {
            !baseline.contains(&p.url) && !reviews.contains_key(&p.url) && armed(p.repo()) && trusted(p)
        })
        .collect()
}

/// Opened by the owner, an org member or a collaborator of the base repo.
///
/// ponytail: an outsider's fork PR is a diff a stranger wrote, fed to a model that may post under your
/// name — "approve this" in the diff is an approval from you if the repo auto-posts. Auto skips it; `r`
/// still reviews it, under the narrowed read review::scope gives an outsider. Fails CLOSED: an empty
/// association (a query that did not carry it) is not standing.
fn trusted(p: &Pr) -> bool {
    review::TRUSTED.contains(&p.author_association.as_str())
}

/// Drop every entry the predicate names, so one PR keeps one entry, not one per push or review.
pub fn evict<K: Eq + std::hash::Hash, V>(cache: &mut HashMap<K, V>, drop: impl Fn(&K) -> bool) {
    cache.retain(|k, _| !drop(k));
}

/// What a finished discussion turn leaves in the saved review: the answer or the failure appended, or the
/// revision set beside the verdict. `now` is the review as it is on disk when the turn lands.
///
/// ponytail: None in, None out. A review dropped while the agent was answering is gone, and writing the
/// answer back would put the file back with it -- the drop undone by the thing that was waiting on it.
pub fn land(
    now: Option<held::Held>,
    turn: std::result::Result<(Option<String>, Option<crate::types::Verdict>), String>,
    at: f64,
) -> Option<held::Held> {
    let mut h = now?;
    match turn {
        Ok((Some(answer), _)) => h.thread.push(held::Turn {
            who: "agent".into(),
            text: answer,
            at,
        }),
        Ok((None, Some(v))) => h.proposed = Some(v),
        Ok(_) => {}
        Err(text) => h.thread.push(held::Turn {
            who: "error".into(),
            text,
            at,
        }),
    }
    Some(h)
}

/// Fill one detail cache entry from `read`, and free its key whatever `read` does.
///
/// ponytail: the write and the key are one step, here, because a panic that skipped the removal left
/// a key nothing starts again — want_detail returns early for anything still in flight, so that
/// revision's pane said loading for the rest of the session. Treat a panic as a failed read; `f`
/// clears those. Taking the read as an argument is what lets a test panic on purpose.
fn fill_detail(me: &State, key: DetailKey, read: impl FnOnce() -> Option<Detail>) {
    let got = catch_unwind(AssertUnwindSafe(read)).unwrap_or_else(|e| {
        error!("detail {} failed: {}", key.0, panic_text(&*e));
        None
    });
    let mut inner = me.lock();
    // ponytail: drop what we knew about this PR at any other revision, so the cache cannot grow one
    // entry per push for a branch someone is iterating on.
    evict(&mut inner.details, |k| k.0 == key.0);
    inner.details.insert(key.clone(), got);
    inner.detailing.remove(&key);
}

/// The same for one diff.
///
/// ponytail: `diff::retry` bumps the generation only when ITS cache holds a failure, so a panic above
/// that layer — anchoring a mark, say — would be cleared by nothing at all.
fn fill_diff(me: &State, key: DiffKey, read: impl FnOnce() -> DiffCache) {
    let got = catch_unwind(AssertUnwindSafe(read))
        .map_err(|e| error!("diff {}#{} failed: {}", key.0, key.1, panic_text(&*e)))
        .ok();
    let mut inner = me.lock();
    evict(&mut inner.diffs, |k| k.0 == key.0 && k.1 == key.1);
    inner.diffs.insert(key.clone(), got);
    inner.diffing.remove(&key);
}

/// True while a review or pre-review of this PR is running.
///
/// ponytail: membership, not a suffix. The status channel also carries a truncated stderr line, so
/// sniffing for "..." meant an error whose last line happened to end in one would pin the UI at a 50ms
/// timeout, count as a running agent, block the key from ever retrying that PR, and survive the stale
/// sweep, permanently, on wording nobody controls. The set is written by the two functions that start
/// work and cleared by the two that finish it; nothing has to be parsed.
pub fn in_flight(inner: &Inner, url: &str) -> bool {
    inner.running.contains(url)
}

/// The last non-empty line of an error, clipped to `max` chars; `fallback` when there is none.
pub fn last_line(text: &str, max: usize, fallback: &str) -> String {
    let line = text.trim().lines().last().unwrap_or(fallback);
    line.chars().take(max).collect()
}

/// What a panic said, for the header.
fn panic_text(e: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = e.downcast_ref::<&str>() {
        s.to_string()
    } else {
        "panic".into()
    }
}

/// Re-mirror every repo `gitdashy init` registered. Never raises: a bad entry must not stop a refresh.
pub fn refresh_mirrors() {
    for (into, repo, root, _loader) in install::registered() {
        // ponytail: refresh what is THERE. init creates the mirror, so a refresh never has cause to make
        // one, and makedirs would otherwise rebuild the tree of a repo you deleted and write memory
        // back into it. Asking about `into` itself needs no recorded root and holds for any --into shape.
        // ponytail: skip it, never unregister it. A path that is not there right now is as often an
        // unplugged drive, a share between mounts or a home not decrypted yet as a deleted repo. From
        // here the two look the same, and the tombstone stops the refresh until someone runs init
        // again, which nothing tells them to do. Only `gitdashy init --forget` writes one.
        if !into.is_dir() || (!root.as_os_str().is_empty() && !root.is_dir()) {
            continue;
        }
        // already pulled above; and this must not touch the network
        let report = catch_unwind(AssertUnwindSafe(|| mirror::sync(&into, &repo, false, false)));
        match report {
            Ok(line) => debug!("mirror sync {}: {}", into.display(), line),
            Err(e) => error!("mirror sync {} failed: {}", into.display(), panic_text(&*e)),
        }
    }
}

#[derive(Clone, Default)]
pub struct State(pub Arc<Mutex<Inner>>);

impl State {
    pub fn new() -> State {
        Default::default()
    }

    /// The Inner, poisoned or not: a review thread that panicked must not take the UI with it.
    pub fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn waker(&self) -> Arc<Wake> {
        self.lock().wake.clone()
    }

    /// The selected PR's detail, or None while it is being fetched.
    ///
    /// ponytail: keyed by (url, updatedAt), not url. A PR that moved has a different branch head, diff
    /// size and CI result, and the pane kept showing the old ones until a restart: the same staleness
    /// the pre-review path had, for the same reason: caching what changes as though it does not.
    /// ponytail: fetched once per PR, off the draw thread. The page polls many times a second; anything
    /// that talks to the network from there would stutter the whole dashboard on every request.
    pub fn want_detail(&self, pr: &Pr) -> Option<Detail> {
        let key: DetailKey = (pr.url.clone(), pr.updated_at.clone());
        {
            let mut inner = self.lock();
            if let Some(got) = inner.details.get(&key) {
                return got.clone();
            }
            if inner.detailing.contains(&key) {
                return None;
            }
            inner.detailing.insert(key.clone());
        }
        let (me, repo, number) = (self.clone(), pr.repo().to_string(), pr.number);
        std::thread::spawn(move || fill_detail(&me, key, || github::detail(&repo, number)));
        None
    }

    /// (files, marks) for one PR's diff, or None while it is being read.
    ///
    /// ponytail: off the draw thread for the same reason want_detail is: the diff is a network read,
    /// so a slow one stuttered the whole dashboard and a timing-out one froze it. It ANCHORS here too,
    /// not in the pane: anchor() tags the lines it marks, so calling it twice over one parse appends
    /// every note twice, and the pane redraws constantly.
    pub fn want_diff(
        &self,
        repo: &str,
        number: u64,
        head: &str,
        findings: &[Finding],
    ) -> Option<(Vec<DiffFile>, Vec<Mark>)> {
        if repo.is_empty() {
            return None;
        }
        // ponytail: the findings are part of the key. A re-review changes what is marked without moving
        // the head, and the pane would have gone on showing the previous round's marks.
        let sig: Vec<(String, String, String)> = findings
            .iter()
            .map(|f| (f.kind.clone(), f.loc.clone(), f.text.clone()))
            .collect();
        // ponytail: the GENERATION is in the key. Without it f cleared diff's cache while this cache went
        // on answering from the failed read above it, so one blip pinned "no diff to show" for the
        // rest of the session: the retry reached the layer nobody was asking.
        let key: DiffKey = (
            repo.to_string(),
            number,
            head.to_string(),
            sig,
            diff::generation(),
        );
        {
            let mut inner = self.lock();
            if let Some(got) = inner.diffs.get(&key) {
                return got.clone();
            }
            if inner.diffing.contains(&key) {
                return None;
            }
            inner.diffing.insert(key.clone());
        }
        let (me, repo, head, findings) = (
            self.clone(),
            repo.to_string(),
            head.to_string(),
            findings.to_vec(),
        );
        std::thread::spawn(move || fill_diff(&me, key, || diff::load(&repo, number, &head, &findings)));
        None
    }

    /// Forget the reads that FAILED, so the next look tries them again; keep the ones that landed.
    ///
    /// ponytail: the twin of diff::retry, for the caches on this side. A failed detail read was cached
    /// as None and nothing here was ever cleared, so one dropped request pinned that revision's pane on
    /// loading until the PR moved. Failures only, or `f` would refetch a pane that is already right.
    pub fn retry_reads(&self) {
        let mut inner = self.lock();
        inner.details.retain(|_, v| v.is_some());
        inner.diffs.retain(|_, v| v.is_some());
    }

    /// include_existing: review what is already listed too, not just what shows up later.
    pub fn set_auto(&self, on: bool, include_existing: bool) {
        // ponytail: the scope is read HERE too, so the next tick sees no widening. Without it, a
        // scope armed from the CLI seconds earlier still looked new to that tick, which folded the
        // very PRs just consented to into the baseline — fewer ran than the count asked about.
        let scope = autorev::scope();
        let wake = {
            let mut inner = self.lock();
            inner.auto = on;
            inner.auto_scope = Some(scope);
            inner.auto_baseline = if !on {
                None
            } else if include_existing {
                Some(HashSet::new())
            } else {
                Some(inner.rr_urls().into_iter().collect())
            };
            inner.wake.clone()
        };
        if on && include_existing {
            wake.set(); // refetch now so the listed PRs start without waiting for the next tick
        }
    }

    /// The thread's landing: the row's status, nothing spinning, nothing kept.
    fn finish(&self, url: &str, status: String) {
        {
            let mut inner = self.lock();
            inner.reviews.insert(url.to_string(), status);
            inner.running.remove(url);
            inner.seen_at.remove(url);
            inner.since.remove(url);
            inner.done_at.insert(url.to_string(), now());
        }
        self.waker().set(); // refetch so an approved PR drops off the list
    }

    /// Claim `url` for a review. False when one is already running: the caller must not spawn.
    ///
    /// ponytail: the claim IS the check. Testing in_flight() and then calling this took the lock twice,
    /// and every request has its own thread, so two POSTs for one URL both passed and both spawned:
    /// two "on its way" comments, two paid model runs, two posted verdicts.
    fn begin(&self, url: &str, status: &str) -> bool {
        let mut inner = self.lock();
        if !inner.running.insert(url.to_string()) {
            return false;
        }
        inner.reviews.insert(url.to_string(), status.to_string());
        inner.since.insert(url.to_string(), now());
        true
    }

    /// Run one job for a row and land it a status, whatever the job does.
    ///
    /// ponytail: the row spins until a status arrives, so a failure the work does not catch would
    /// leave it spinning for the rest of the session with nothing to press. On the calling thread,
    /// not spawned — `start` is the spawning half, and a test can drive this one.
    fn run_job(&self, url: &str, what: &str, work: impl FnOnce() -> anyhow::Result<String>) {
        let status = match catch_unwind(AssertUnwindSafe(work)) {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                error!("{what} {url} failed: {e:#}");
                format!("error: {e}").chars().take(88).collect()
            }
            Err(e) => {
                error!("{what} {url} failed: {}", panic_text(&*e));
                format!("error: {}", panic_text(&*e)).chars().take(88).collect()
            }
        };
        info!("{what} {url} -> {status}");
        self.finish(url, status);
    }

    /// Claim the row, run `work` off this thread, land a status. False when something is already
    /// running for that PR, in which case nothing was started.
    ///
    /// ponytail: one definition for the three actions that start work on a row. Written out three
    /// times, their error handling had drifted: the held post logged nothing at all, not even what it
    /// landed. Every one of them reports its result now. Only the review logs a START, because only it
    /// has something to say there — which model it is about to spend.
    fn start(
        &self,
        url: &str,
        spinner: &str,
        what: &'static str,
        work: impl FnOnce() -> anyhow::Result<String> + Send + 'static,
    ) -> bool {
        if !self.begin(url, spinner) {
            return false;
        }
        let (me, url) = (self.clone(), url.to_string());
        std::thread::spawn(move || me.run_job(&url, what, work));
        true
    }

    /// False when a review of this PR is already running, and nothing was started.
    /// `ran` says whose decision this was: you pressed `r`, or auto started it. A repo can settle
    /// the two differently, so the review has to carry it all the way to the post.
    /// `ask` is what the person starting it typed for this one review, "" for none.
    pub fn start_review(&self, pr: &Pr, ran: autorev::Ran, ask: &str) -> bool {
        let model = config::get().model;
        let (pr, url, ask) = (pr.clone(), pr.url.clone(), ask.to_string());
        self.start(&url, "reviewing...", "review", move || {
            info!("review {} with {}", pr.url, model);
            review::review(&pr, &model, ran, &ask)
        })
    }

    /// Cast a spell. The row says "casting spell..." while it runs and then goes back to what it said before,
    /// because a spell is not a review: a reviewed PR stays reviewed, and auto still sees an unreviewed one.
    /// begin() is not used because the status it replaces has to be read under its lock.
    pub fn start_spell(&self, pr: &Pr, name: &str, text: &str) -> bool {
        // the status it had, read under the same lock that claims the row
        let before = {
            let mut inner = self.lock();
            if !inner.running.insert(pr.url.clone()) {
                return false;
            }
            inner.since.insert(pr.url.clone(), now());
            inner.reviews.insert(pr.url.clone(), CASTING.to_string())
        };
        let model = config::get().model;
        let (me, pr, name, text) = (self.clone(), pr.clone(), name.to_string(), text.to_string());
        std::thread::spawn(move || {
            let url = pr.url.clone();
            let failed =
                match catch_unwind(AssertUnwindSafe(|| review::cast_spell(&pr, &model, &name, &text))) {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(format!("{e:#}")),
                    Err(e) => Some(panic_text(&*e)),
                };
            info!("spell {name} on {url} -> {}", failed.as_deref().unwrap_or("cast"));
            me.end_spell(&url, before, failed);
        });
        true
    }

    /// The row after a spell: what it said before, or the error when the cast failed. A failure shows over a
    /// verdict too, until the next refresh, because a row is the one place a failed cast can be seen. A status
    /// something else wrote while the spell ran is left alone.
    fn end_spell(&self, url: &str, before: Option<String>, failed: Option<String>) {
        {
            let mut inner = self.lock();
            let untouched = inner.reviews.get(url).is_some_and(|s| s == CASTING);
            match (failed, before) {
                _ if !untouched => {}
                (Some(e), _) => {
                    let status = format!("error: {e}").chars().take(88).collect();
                    inner.reviews.insert(url.to_string(), status);
                }
                (None, Some(s)) => {
                    inner.reviews.insert(url.to_string(), s);
                }
                (None, None) => {
                    inner.reviews.remove(url);
                }
            }
            inner.running.remove(url);
            inner.since.remove(url);
        }
        self.waker().set();
    }

    /// Forget a held review's status, so the row goes back to what the board says it is.
    ///
    /// ponytail: the hold writes its verdict into `reviews` through finish(), which is how the row
    /// shows "waiting to post". Dropping the file without clearing that left the row reading as
    /// reviewed and auto skipping the PR — recoverable only by a push or a restart, because nothing
    /// else removes an entry except a head that moved.
    pub fn forget_review(&self, url: &str) {
        let mut inner = self.lock();
        inner.reviews.remove(url);
        inner.done_at.remove(url);
    }

    /// Post a review that was waiting, with the row spinning while it goes.
    ///
    /// ponytail: a thread and `begin`/`finish`, like every other review action. Posting is two
    /// network calls; doing it in the handler would hold the UI, and the row would say nothing was
    /// happening.
    pub fn start_post_held(&self, h: held::Held) -> bool {
        let url = h.pr.url.clone();
        self.start(&url, "posting...", "post held", move || review::post_held(&h))
    }

    /// Claim a row for a short write that must not interleave with a turn: accepting or keeping a revision.
    /// False when anything is already running on it.
    pub fn claim(&self, url: &str, label: &str) -> bool {
        self.begin(url, label)
    }

    /// Let go of a claim, leaving `status` on the row.
    pub fn release(&self, url: &str, status: String) {
        self.finish(url, status)
    }

    /// Set a row's review status without touching whether anything is running on it.
    pub fn set_status(&self, url: &str, status: String) {
        self.lock().reviews.insert(url.to_string(), status);
    }

    /// Whether anything is running on this PR's row: a review, a post, or a discussion.
    pub fn busy(&self, url: &str) -> bool {
        in_flight(&self.lock(), url)
    }

    /// One discussion turn on a held review, or a request for a revision when `message` is None.
    /// False when something is already running on the row, and nothing was started.
    ///
    /// ponytail: the row's own claim, the one a review and a post take. A post that started while the
    /// agent was still answering would put up a verdict the conversation was about to change, and a
    /// second turn resumed into the same session at once would interleave in it.
    /// ponytail: your message is on disk before the model sees it, so the conversation shows it at once;
    /// the answer is appended to the file as it is when the answer lands, and a review dropped in the
    /// meantime is not brought back.
    pub fn start_talk(&self, on: review::Talk, h: held::Held, message: Option<String>) -> bool {
        let url = h.pr.url.clone();
        let label = if message.is_some() {
            "discussing..."
        } else {
            "revising..."
        };
        if !self.begin(&url, label) {
            return false;
        }
        // ponytail: the file as it is NOW, under the claim. The caller read it before claiming the row, and a
        // turn that landed in between was on disk but not in that copy -- appending to the copy wrote over the
        // answer that had just arrived.
        let Some(mut h) = on.get(h.pr.repo(), h.pr.number) else {
            self.finish(&url, String::new());
            self.forget_review(&url);
            return false;
        };
        if let Some(m) = &message {
            h.thread.push(held::Turn {
                who: "you".into(),
                text: m.clone(),
                at: now(),
            });
            if let Err(e) = on.put(&h) {
                error!("discussion of {url} not saved: {e:#}");
                self.finish(&url, on.status(&h.verdict));
                return false;
            }
        }
        let me = self.clone();
        std::thread::spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| match &message {
                Some(m) => review::discuss(&h, m).map(|a| (Some(a), None)),
                None => review::revise(&h).map(|v| (None, Some(v))),
            }));
            let (repo, n) = (h.pr.repo().to_string(), h.pr.number);
            let turn = match outcome {
                Ok(Ok(t)) => Ok(t),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(e) => Err(panic_text(&*e)),
            };
            let Some(now_held) = land(on.get(&repo, n), turn, now()) else {
                // dropped while the agent was answering: nothing to add the answer to
                me.finish(&url, String::new());
                me.forget_review(&url);
                return;
            };
            if let Err(e) = on.put(&now_held) {
                error!("discussion of {url} not saved: {e:#}");
            }
            me.finish(&url, on.status(&now_held.verdict));
        });
        true
    }

    /// Pre-review one of MY PRs. Posts nothing; the file it writes is found again by its name.
    /// False when a review of this PR is already running, and nothing was started.
    pub fn start_self_review(&self, pr: &Pr) -> bool {
        let model = config::get().model;
        let (pr, url) = (pr.clone(), pr.url.clone());
        self.start(&url, "pre-reviewing...", "self-review", move || {
            // ponytail: the path is not kept: it is derivable
            review::self_review(&pr, &model).map(|(status, _dest)| status)
        })
    }

    /// Refresh forever on this thread. A tick that fails is reported and retried; it never ends the thread.
    ///
    /// ponytail: the guard is here rather than on each call this makes, because the ways a tick can
    /// fail are not enumerable: it reads a log other machines append to and merge, a registry, a
    /// settings file and several subprocesses. One unreadable line in the review log used to unwind out
    /// of this thread, and nothing restarts it: fetching stayed true, no key reaches a thread that
    /// is gone, and f only sets an event nobody waits on any more. The dashboard was over, silently.
    pub fn run_loop(&self) {
        let wake = self.waker();
        loop {
            let t0 = now();
            // ponytail: from THIS attempt, not from fetched_at, when the tick failed. That holds the last
            // SUCCESSFUL fetch, so the moment one tick failed the deadline was already in the past: the
            // loop fell straight out of the wait and retried every second, hammering the API for as long
            // as the failure lasted. It also covers the FIRST tick, where fetched_at is still None.
            // ponytail: the fetch's own time otherwise, so the header's countdown agrees.
            let base = if self.refresh(t0) {
                self.lock().fetched_at.unwrap_or(t0)
            } else {
                t0
            };
            // ponytail: `base + interval` is evaluated per slice, and `interval` is the reason.
            // Hoisting it into a variable above the loop is the natural way to write this and silently
            // breaks `i`: the settings screen changes the interval and does NOT wake the loop, so
            // dropping 30m to 1m waited out the remaining 29 instead of refetching now.
            // `ticked` is for the wake gap below: a failed tick's base is its START, not its end.
            let ticked = now();
            while !wake.wait(Duration::from_secs(1)) && now() < base + config::get().interval as f64 {
                // 1s slices, so a change to either side takes effect within the second
            }
            // ponytail: every finished review wakes the loop, and a tick is pulls, ls-remote, backup,
            // mirrors and a fetch. Auto-review finishing ten PRs ran ten of them back to back (#73). Holding
            // a wake until WAKE_GAP after the last tick lands a burst as one. Ceiling: `f` right after a
            // tick waits out the rest of the gap; a fetch-only wake is the upgrade if that is felt.
            let hold = ticked + WAKE_GAP - now();
            if hold > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(hold));
            }
            wake.clear();
        }
    }

    /// One tick, and the failure path: true when it landed.
    fn refresh(&self, t0: f64) -> bool {
        let failed = match catch_unwind(AssertUnwindSafe(|| self.tick_inner(t0))) {
            Ok(Ok(())) => {
                self.lock().ticks += 1;
                return true;
            }
            Ok(Err(e)) => format!("{e:#}"),
            Err(e) => panic_text(&*e),
        };
        error!("tick failed: {failed}"); // ponytail: the whole point: a failed tick is a row, not the end
        let mut inner = self.lock();
        inner.ticks += 1;
        inner.fetching = false;
        inner.error = last_line(&failed, 60, "error");
        false
    }

    pub fn wake(&self) {
        self.waker().set();
    }

    /// Wake the loop, and the `ticks` count that answers it: a tick that ran after this call.
    pub fn wake_answered_by(&self) -> u64 {
        let n = answered_by(&self.lock()); // read BEFORE waking: a tick the wake starts must not count as already running
        self.wake();
        n
    }

    /// Pool and cross-check drafts on a thread of their own. Returns at once; never raises.
    ///
    /// ponytail: NOT on the refresh thread. It was, immediately after the pull and before
    /// `github::fetch()`, with `fetching` already true and a 300s model timeout, so the first sweep
    /// after an upgrade, with a whole backlog newly poolable, blocked the PR list for as long as the
    /// model took. Catching the exception was never the risk; the wait was. The comment claimed the
    /// list must not depend on a model and the code put one in front of it.
    /// ponytail: one at a time. A slow sweep must not have a second one started on top of it by the
    /// next tick, both writing the same pool files and both pushing the same checkout.
    pub fn start_sweep(&self) {
        {
            let mut inner = self.lock();
            if inner.sweeping {
                return;
            }
            inner.sweeping = true;
        }
        let me = self.clone();
        std::thread::spawn(move || {
            let model = config::get().model;
            if let Err(e) = catch_unwind(AssertUnwindSafe(|| memory::sweep(&model))) {
                error!("draft sweep failed: {}", panic_text(&*e)); // surfaced in the debug log, never on the header
            }
            me.lock().sweeping = false;
        });
    }

    /// What has arrived since anyone last looked, and forget it. Under the lock: the refresh thread
    /// adds to this while the UI reads it, and an arrival landing between a copy and a clear was
    /// silently dropped.
    pub fn take_arrivals(&self) -> HashMap<String, usize> {
        std::mem::take(&mut self.lock().arrived)
    }

    /// One refresh: pull, mirror, fetch, sweep stale verdicts, start auto reviews, notify.
    pub fn tick(&self, t0: f64) {
        self.refresh(t0);
    }

    fn tick_inner(&self, t0: f64) -> anyhow::Result<()> {
        debug!("tick");
        let interval = config::get().interval;
        self.lock().fetching = true; // ponytail: the failure path clears this under the lock; both sides now agree
                                     // ponytail: FIRST, and before the pull it is a claim about. Anything reading this while the tick
                                     // runs should be told a dashboard is here, or it does the pull itself, which is the one thing
                                     // the beat exists to prevent. Written every tick rather than once at startup, so a dashboard
                                     // that stopped refreshing (suspended, wedged) stops counting as one.
        heartbeat::beat(interval);
        // ponytail: the lines around the pull, with your own facts excluded; see memory::arrivals.
        // ponytail: the snapshot must not be able to cost the pull. One unreadable team file (a
        // permission, a half-written merge, a non-UTF-8 byte a teammate pushed) must not skip that
        // tick's git entirely, for the sake of a badge. The badge is the optional half.
        let was: HashMap<String, HashSet<(Option<String>, String)>> = match catch_unwind(|| {
            team::joined()
                .into_iter()
                .map(|k| (k.clone(), memory::team_lines(&k)))
                .collect()
        }) {
            Ok(was) => was,
            Err(e) => {
                error!("could not count team facts before the pull: {}", panic_text(&*e));
                HashMap::new()
            }
        };
        // ponytail: the tick takes the LOCK too. It used to beat and then pull unguarded, on the
        // argument that a beat is enough, but a hook sync that claimed the lock while no dashboard was
        // up keeps pulling after one starts, and the first tick then rebased the same checkout beside
        // it. "Only one process ever pulls a given checkout" was in the README and was not true.
        // ponytail: the lock is not waited for. A tick that cannot have it skips the pull and takes the
        // next one; the alternative is blocking the refresh thread, and everything after this line
        // (the PR list, the mirrors) has nothing to do with the team's git.
        if heartbeat::claim() {
            let pulled = catch_unwind(team::pull); // newest team log + memory before we read them
            heartbeat::unclaim();
            if let Err(e) = pulled {
                std::panic::resume_unwind(e);
            }
        }
        // ponytail: guarded on THIS side too. The pull is what brings in the unreadable file, so the
        // read after it is the likelier of the two to meet one, and everything below here, the sweep,
        // the mirrors and the PR list, was riding on a counter.
        let grew: HashMap<String, usize> = match catch_unwind(AssertUnwindSafe(|| {
            was.iter()
                .map(|(k, before)| (k.clone(), memory::arrivals(k, before)))
                .filter(|(_, n)| *n > 0)
                .collect()
        })) {
            Ok(grew) => grew,
            Err(e) => {
                error!("could not count what the pull brought: {}", panic_text(&*e));
                HashMap::new()
            }
        };
        if !grew.is_empty() {
            let mut inner = self.lock(); // ponytail: read on the UI thread, like every other cross-thread field
            for (key, n) in grew {
                *inner.arrived.entry(key).or_insert(0) += n;
            }
        }
        self.start_sweep();
        memory::history(); // ponytail: before the backup, so the first commit is memory as it arrived,
        memory::backup("tick"); // and so the Memory row can say "no history" before a write, not after
        refresh_mirrors(); // ponytail: here, not in a session hook: no global config, no timeout budget
        let mut data = github::fetch();
        let stale = review_log::mark_rereviews(&mut data);
        let newer = update::update_available();
        if self.lock().fetched_at.is_none() {
            // let the splash breathe on the first load
            let left = config::SPLASH_MIN - (now() - t0);
            if left > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(left));
            }
        }
        // ponytail: beaten AGAIN, at the end. The first beat is stamped at tick start and alive() allows
        // one interval plus GRACE, but the next tick starts at fetched_at + interval, so any tick
        // longer than GRACE made a healthy dashboard read as dead until it came round again, and one
        // slow team pull is enough at a 120s git timeout. So the tick beats again at the end.
        heartbeat::beat(interval);
        // ponytail: one read of the store for the whole tick, and OUTSIDE the lock — asking per row
        // would reopen the file once per PR, two rows in one tick could disagree if a click landed
        // between them, and none of that IO needs the state mutex held.
        let scope = autorev::scope();
        let new: Vec<Pr> = {
            let mut inner = self.lock();
            inner.sections = data.clone();
            inner.fetched_at = Some(now());
            inner.update = newer;
            inner.fetching = false;
            inner.error = String::new(); // this tick landed, so whatever the last one said is over
            for u in &stale {
                // forget the old verdict so r / auto can review the new push
                // ponytail: same guard as the sweep below. `stale` is read off the REVIEWED section of
                // THIS fetch, so a fetch that started before our verdict landed still holds the previous
                // entry, calls the head we just reviewed a new push, drops the verdict, and auto starts
                // the identical review a second later.
                if !in_flight(&inner, u) && inner.done_at.get(u).copied().unwrap_or(0.0) <= t0 {
                    inner.reviews.remove(u);
                    inner.seen_at.remove(u);
                }
            }
            // ponytail: and forget it for ANY row whose PR has moved since. `stale` comes from
            // log::mark_rereviews, which only ever names REVIEW REQUESTED urls, so a finished
            // pre-review masked GitHub's decision on a MINE row until restart, and a colleague
            // approving your PR never showed. One rule for both: a verdict describes one revision.
            // ponytail: baselined on the first fetch that STARTED after the work finished. Posting a
            // review bumps updatedAt itself, so the value we held is already stale, and a fetch that
            // was in flight when the verdict landed carries the pre-post value, which is why the
            // start time is compared rather than merely "the next fetch".
            for s in &data {
                if s.name == "REVIEWED" {
                    // ponytail: not a live row. Its updatedAt is the log timestamp, which never equals
                    // the live row's value, so sweeping it dropped every verdict on the next tick.
                    continue;
                }
                for p in s.prs.iter().flatten() {
                    let u = &p.url;
                    if !inner.reviews.contains_key(u) || in_flight(&inner, u) {
                        continue;
                    }
                    // ponytail: by head on a REVIEW REQUESTED row, like mark_rereviews. updatedAt there
                    // comes from the lagging search index (the tick after a verdict can still read the
                    // pre-post value and the next the post-post one) and a reply on the thread bumps
                    // it too; both dropped the verdict and auto reviewed the same head again. On MINE
                    // rows updatedAt stays: a colleague's approval must be allowed to unmask GitHub.
                    // A PR without the field simply never goes stale.
                    let at = if s.name == "REVIEW REQUESTED" {
                        // no head means the graphql call failed; not a reason to call it moved
                        if p.head.is_empty() {
                            continue;
                        }
                        p.head.clone()
                    } else {
                        if p.updated_at.is_empty() {
                            continue;
                        }
                        p.updated_at.clone()
                    };
                    if inner.done_at.get(u).copied().unwrap_or(0.0) > t0 {
                        // ponytail: this fetch STARTED before the work finished, so it carries the
                        // pre-post updatedAt. Baselining on it would sweep our own verdict on the
                        // next tick, which the comment below claimed to avoid and did not.
                        continue;
                    }
                    match inner.seen_at.get(u) {
                        None => {
                            inner.seen_at.insert(u.clone(), at);
                        }
                        Some(seen) if *seen != at => {
                            inner.reviews.remove(u);
                            inner.seen_at.remove(u);
                        }
                        Some(_) => {}
                    }
                }
            }
            inner.pending = inner.pending_rr(&|r| scope.armed(r));
            // ponytail: HERE, not in the route that writes the store. `gitdashy auto` is a separate
            // process, so the dashboard learns about a widened scope by reading the file on the next
            // tick — and suppressing the batch in post_auto left the CLI path firing the very batch
            // the route was careful to avoid. The tick is where every writer converges.
            inner.absorb_scope(&scope);
            match (&inner.auto, &inner.auto_baseline) {
                (true, Some(baseline)) => {
                    // a team's memory repo is approved by a person, never reviewed by auto
                    let theirs = team::team_repos();
                    auto_starts(inner.rr_prs(), baseline, &inner.reviews, &|r| {
                        scope.armed(r) && !team::in_repos(&theirs, r)
                    })
                }
                _ => Vec::new(),
            }
        };
        for p in &new {
            // already running is not an error here: auto only skips it
            self.start_review(p, autorev::Ran::Auto, "");
        }
        let asks: Vec<&Section> = data
            .iter()
            .filter(|s| s.name == "REVIEW REQUESTED" || s.name == "ASSIGNED")
            .collect();
        // a failed section would look like every PR left, then came back
        if asks.iter().all(|s| s.err.as_deref().unwrap_or("").is_empty()) {
            let wanted: HashMap<String, (&Pr, &str)> = asks
                .iter()
                .flat_map(|s| {
                    s.prs
                        .iter()
                        .flatten()
                        .map(move |p| (p.url.clone(), (p, s.name.as_str())))
                })
                .collect();
            let (known, notify_on) = (self.lock().known.clone(), config::get().notify);
            if let (Some(known), true) = (known, notify_on) {
                let mut fresh: Vec<&String> = wanted.keys().filter(|u| !known.contains(*u)).collect();
                fresh.sort();
                for u in fresh {
                    let (p, section) = wanted[u];
                    notify(p, section);
                }
            }
            self.lock().known = Some(wanted.keys().cloned().collect());
        }
        Ok(())
    }
}

/// Where notify-send finds the icon: written out of the binary once.
fn notify_icon() -> PathBuf {
    let path = std::env::temp_dir().join("gitdashy-notify.png");
    if !path.is_file() {
        let _ = std::fs::write(&path, NOTIFY_ICON);
    }
    path
}

/// The notify-send argv for a PR that just asked for me. None on a payload missing a field (a deleted author).
pub fn notify_cmd(pr: &Pr, section: &str) -> Option<Vec<String>> {
    let what = if section == "REVIEW REQUESTED" {
        "wants a review"
    } else {
        "assigned you"
    };
    let author = pr.author.as_ref()?;
    let icon = notify_icon();
    Some(
        [
            "notify-send",
            "-a",
            "gitdashy",
            "-u",
            "normal",
            "-c",
            "im.received",
            "-A",
            "open=Open PR",
            "-i",
        ]
        .into_iter()
        .map(String::from)
        .chain([
            icon.to_string_lossy().into_owned(),
            format!("#{} {}", pr.number, pr.title),
            format!("<b>{}</b> · {} {}", pr.repository.name, author.login, what),
        ])
        .collect(),
    )
}

/// Desktop popup with an Open button. Silent if notify-send is missing or the payload is odd (deleted author).
pub fn notify(pr: &Pr, section: &str) {
    let Some(argv) = notify_cmd(pr, section) else {
        return;
    };
    let url = pr.url.clone();
    // ponytail: -A blocks until dismissed, so wait in a thread, killed after NOTIFY_WAIT; notify-send only (Linux)
    std::thread::spawn(move || {
        // a popup is decoration; the refresh loop must outlive it
        let out = review::run_timed(std::process::Command::new(&argv[0]).args(&argv[1..]), NOTIFY_WAIT);
        if let Ok(out) = out {
            if out.trim() == "open" {
                github::open_in_browser(&url);
            }
        }
    });
}

/// A tick already running when `f` lands started before it, and the woken one only runs after it.
/// ponytail: a tick past `wake.clear()` that has not set `fetching` yet counts as answering; it has not fetched anything yet.
fn answered_by(inner: &Inner) -> u64 {
    inner.ticks + 1 + inner.fetching as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A message sent from a copy read before a turn landed must not write over that turn: the claim re-reads.
    #[test]
    fn a_message_from_a_stale_copy_keeps_the_answer_that_landed_first() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        config::update(|c| {
            c.demo = true;
            c.held_dir = d.path().join("held");
        });
        let state = State::default();
        let pr = Pr {
            number: 7,
            url: "https://github.com/acme/api/pull/7".into(),
            repository: crate::types::Repository {
                name_with_owner: "acme/api".into(),
                name: "api".into(),
            },
            ..Default::default()
        };
        let stale = held::Held {
            pr: pr.clone(),
            model: "opus".into(),
            session: "5e3ae8e0-544e-4128-88af-fe301d354aae".into(),
            ..Default::default()
        };
        // on disk, an answer landed after `stale` was read
        let mut landed = stale.clone();
        landed.thread.push(held::Turn {
            who: "agent".into(),
            text: "the answer that landed".into(),
            at: 1.0,
        });
        held::put(&landed).unwrap();

        assert!(state.start_talk(review::Talk::Held, stale, Some("and then?".into())));
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while state.busy(&pr.url) {
            assert!(std::time::Instant::now() < until, "the turn never finished");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let texts: Vec<String> = held::get("acme/api", 7)
            .unwrap()
            .thread
            .into_iter()
            .map(|t| t.text)
            .collect();
        assert_eq!(texts[0], "the answer that landed", "{texts:?}");
        assert_eq!(texts[1], "and then?");
        assert_eq!(texts.len(), 3, "and the new answer after it: {texts:?}");
    }

    #[test]
    fn a_turn_lands_on_the_review_as_it_is_and_never_brings_a_dropped_one_back() {
        let h = held::Held::default();
        assert!(
            land(None, Ok((Some("an answer".into()), None)), 1.0).is_none(),
            "dropped stays dropped"
        );
        assert!(land(None, Err("boom".into()), 1.0).is_none());

        let answered = land(Some(h.clone()), Ok((Some("an answer".into()), None)), 1.0).unwrap();
        assert_eq!(
            (answered.thread[0].who.as_str(), answered.thread[0].text.as_str()),
            ("agent", "an answer")
        );

        let failed = land(Some(h.clone()), Err("claude: timed out".into()), 1.0).unwrap();
        assert_eq!(
            failed.thread[0].who, "error",
            "a failed turn is kept, so the conversation says so"
        );

        let v = crate::types::Verdict {
            verdict: "comment".into(),
            ..Default::default()
        };
        let revised = land(Some(h), Ok((None, Some(v.clone()))), 1.0).unwrap();
        assert_eq!(revised.proposed, Some(v));
        assert!(
            revised.thread.is_empty(),
            "a revision waits beside the verdict, not in the thread"
        );
    }

    use crate::types::{Login, Repository};

    fn pr(url: &str) -> Pr {
        Pr {
            number: 7,
            title: "T".into(),
            url: url.into(),
            updated_at: "2020-01-01T00:00:00Z".into(),
            author: Some(Login { login: "me".into() }),
            author_association: "MEMBER".into(),
            repository: Repository {
                name_with_owner: "a/b".into(),
                name: "b".into(),
            },
            ..Default::default()
        }
    }

    fn section(name: &str, prs: Option<Vec<Pr>>, err: Option<&str>) -> Section {
        Section {
            name: name.into(),
            prs,
            err: err.map(String::from),
        }
    }

    #[test]
    fn a_press_during_a_tick_waits_for_the_next_one() {
        let mut inner = Inner {
            ticks: 5,
            ..Default::default()
        };
        assert_eq!(answered_by(&inner), 6);
        inner.fetching = true;
        assert_eq!(answered_by(&inner), 7);
    }

    #[test]
    fn evict_drops_every_entry_the_predicate_names() {
        let mut cache: HashMap<DetailKey, Option<Detail>> = HashMap::new();
        cache.insert(("u".into(), "1".into()), None);
        cache.insert(("u".into(), "2".into()), None);
        cache.insert(("v".into(), "1".into()), None);
        evict(&mut cache, |k| k.0 == "u");
        assert_eq!(cache.len(), 1);
        assert!(cache.contains_key(&("v".to_string(), "1".to_string())));
    }

    /// However a job ends, the row gets a status: it spins until one lands, and nothing else writes one.
    #[test]
    fn a_job_lands_the_row_a_status_however_it_ends() {
        let s = State::new();
        let status = |s: &State| s.lock().reviews.get("u").cloned().unwrap_or_default();
        // claimed first, as start() claims it: otherwise "it stopped spinning" asserts nothing
        let claim = |s: &State| assert!(s.begin("u", "working..."), "the row was free");

        claim(&s);
        s.run_job("u", "job", || Ok("✓ approved".into()));
        assert_eq!(status(&s), "✓ approved");
        claim(&s);
        s.run_job("u", "job", || Err(anyhow::anyhow!("no token")));
        assert_eq!(status(&s), "error: no token");
        claim(&s);
        s.run_job("u", "job", || panic!("boom"));
        assert_eq!(status(&s), "error: boom");
        assert!(!s.lock().running.contains("u"), "and the row stops spinning");

        // the row is one line: a long failure is clipped to fit it, not wrapped into it
        claim(&s);
        s.run_job("u", "job", || Err(anyhow::anyhow!("{}", "x".repeat(200))));
        assert_eq!(status(&s).chars().count(), 88);
    }

    /// A spell is not a review: the row goes back to what it said, so a reviewed PR stays reviewed and
    /// auto, which skips any PR with a status, still reviews an unreviewed one.
    #[test]
    fn a_spell_leaves_the_row_as_it_found_it() {
        let s = State::new();
        s.lock().reviews.insert("done".into(), "✓ approved".into());
        for (url, before, failed, after) in [
            ("done", Some("✓ approved".to_string()), None, Some("✓ approved")),
            ("new", None, None, None),
            ("new", None, Some("no token".to_string()), Some("error: no token")),
        ] {
            assert!(s.begin(url, CASTING));
            s.end_spell(url, before, failed);
            let inner = s.lock();
            assert_eq!(inner.reviews.get(url).map(String::as_str), after, "{url}");
            assert!(!inner.running.contains(url));
        }
    }

    /// A verdict that lands while a spell runs is the row's news: the spell's restore must not undo it.
    #[test]
    fn a_status_written_during_a_spell_survives_its_end() {
        let s = State::new();
        assert!(s.begin("u", CASTING));
        s.lock().reviews.insert("u".into(), "✓ approved".into());
        s.end_spell("u", None, Some("no token".into()));
        let inner = s.lock();
        assert_eq!(inner.reviews.get("u").map(String::as_str), Some("✓ approved"));
        assert!(!inner.running.contains("u"), "and the row stops spinning");
    }

    /// The line the fix turns on: whatever the read does, its key must not stay in flight. Nothing
    /// starts a read that is still running, so the pane would say loading until the PR moved.
    #[test]
    fn a_panicking_read_frees_its_key_and_counts_as_a_failure() {
        let s = State::new();
        let key: DetailKey = ("u".into(), "1".into());
        s.lock().detailing.insert(key.clone());
        fill_detail(&s, key.clone(), || panic!("boom"));
        {
            let inner = s.lock();
            assert!(!inner.detailing.contains(&key), "the key must not stay in flight");
            assert!(
                matches!(inner.details.get(&key), Some(None)),
                "a panic is a read that failed"
            );
        }

        let key: DiffKey = ("acme/api".into(), 7, "head".into(), Vec::new(), 0);
        s.lock().diffing.insert(key.clone());
        fill_diff(&s, key.clone(), || panic!("boom"));
        let inner = s.lock();
        assert!(!inner.diffing.contains(&key), "the key must not stay in flight");
        assert!(matches!(inner.diffs.get(&key), Some(None)));
    }

    /// `f` is "go and look again": the reads that failed are dropped so the next look retries them,
    /// and the ones that landed stay, so pressing it does not refetch the whole pane.
    #[test]
    fn a_refresh_forgets_the_reads_that_failed_and_keeps_the_rest() {
        let s = State::new();
        let diff_key = |n: u64| -> DiffKey { ("acme/api".into(), n, "head".into(), Vec::new(), 0) };
        {
            let mut inner = s.lock();
            inner
                .details
                .insert(("landed".into(), "1".into()), Some(Detail::default()));
            inner.details.insert(("failed".into(), "1".into()), None);
            inner.diffs.insert(diff_key(1), Some((Vec::new(), Vec::new())));
            inner.diffs.insert(diff_key(2), None);
        }
        s.retry_reads();
        let inner = s.lock();
        assert_eq!(inner.details.len(), 1, "the failed detail is gone");
        assert!(inner
            .details
            .contains_key(&("landed".to_string(), "1".to_string())));
        assert_eq!(inner.diffs.len(), 1, "the failed diff is gone");
        assert!(inner.diffs.contains_key(&diff_key(1)));
    }

    #[test]
    fn in_flight_is_the_set_not_a_suffix() {
        // an error whose wording ends in "..." must not count as a running agent
        let mut inner = Inner::default();
        inner
            .reviews
            .insert("u".into(), "error: timed out waiting...".into());
        assert!(!in_flight(&inner, "u"));
        inner.running.insert("u".into());
        assert!(in_flight(&inner, "u"));
    }

    #[test]
    fn notify_cmd_pins_the_payload_contract() {
        let cmd = notify_cmd(&pr("u"), "ASSIGNED").unwrap();
        assert_eq!(cmd[0], "notify-send");
        assert!(cmd.contains(&"-A".to_string()));
        assert_eq!(cmd[cmd.len() - 2], "#7 T");
        assert_eq!(cmd[cmd.len() - 1], "<b>b</b> · me assigned you");
        assert!(notify_cmd(&pr("u"), "REVIEW REQUESTED")
            .unwrap()
            .last()
            .unwrap()
            .contains("wants a review"));
        let mut gone = pr("u");
        gone.author = None;
        assert!(notify_cmd(&gone, "ASSIGNED").is_none()); // a deleted account; notify() swallows this
    }

    /// A PR in a named repo, so the armed-scope leg can be aimed.
    fn pr_in(url: &str, repo: &str) -> Pr {
        let mut p = pr(url);
        p.repository.name_with_owner = repo.into();
        p.repository.name = repo.split('/').next_back().unwrap_or(repo).into();
        p
    }

    fn started(
        rr: Vec<Pr>,
        baseline: &[&str],
        reviewed: &[&str],
        armed: &dyn Fn(&str) -> bool,
    ) -> Vec<String> {
        let b: HashSet<String> = baseline.iter().map(|s| s.to_string()).collect();
        let r: HashMap<String, String> = reviewed
            .iter()
            .map(|s| (s.to_string(), "✓".to_string()))
            .collect();
        auto_starts(rr, &b, &r, armed)
            .into_iter()
            .map(|p| p.url)
            .collect()
    }

    #[test]
    fn auto_starts_what_is_new_and_unreviewed_in_an_armed_repo() {
        let every = |_: &str| true;
        let rr = || vec![pr_in("new", "a/b"), pr_in("old", "a/b"), pr_in("done", "a/b")];
        assert_eq!(started(rr(), &[], &[], &every), ["new", "old", "done"]);
        assert_eq!(started(rr(), &["old"], &[], &every), ["new", "done"]);
        assert_eq!(started(rr(), &["old"], &["done"], &every), ["new"]);
    }

    /// The whole point of the store: one `a` must not review every repo the token can see.
    #[test]
    fn auto_skips_a_repo_it_is_not_armed_for() {
        let only_b = |repo: &str| repo == "a/b";
        let rr = vec![pr_in("mine", "a/b"), pr_in("theirs", "other/thing")];
        assert_eq!(started(rr, &[], &[], &only_b), ["mine"]);
    }

    /// A stranger's diff is not fed to a model that may post under your name: auto skips anyone without
    /// standing in the repo, and an association the query did not carry counts as none.
    #[test]
    fn auto_skips_a_pr_from_an_outsider() {
        let every = |_: &str| true;
        let by = |url: &str, assoc: &str| Pr {
            author_association: assoc.into(),
            ..pr(url)
        };
        let rr = vec![
            by("owner", "OWNER"),
            by("member", "MEMBER"),
            by("collab", "COLLABORATOR"),
            by("fork", "CONTRIBUTOR"),
            by("first", "FIRST_TIME_CONTRIBUTOR"),
            by("stranger", "NONE"),
            by("unknown", ""),
        ];
        assert_eq!(started(rr, &[], &[], &every), ["owner", "member", "collab"]);
    }

    /// The "Also review the N already listed?" prompt is the number someone consents against, so it
    /// counts what auto will actually start, not every review request on the board.
    #[test]
    fn the_pending_count_only_counts_repos_auto_is_armed_for() {
        let st = State::new();
        st.lock().sections = vec![section(
            "REVIEW REQUESTED",
            Some(vec![
                pr_in("mine", "a/b"),
                pr_in("theirs", "other/thing"),
                pr_in("done", "a/b"),
            ]),
            None,
        )];
        st.lock().reviews.insert("done".into(), "✓ approved".into());
        let every = |_: &str| true;
        let only_b = |repo: &str| repo == "a/b";
        assert_eq!(st.lock().pending_rr(&every), ["mine", "theirs"]);
        assert_eq!(st.lock().pending_rr(&only_b), ["mine"]);
    }

    /// An unplugged drive is not a deleted repo, and the two look the same from here. A refresh skips
    /// what it cannot see; only `gitdashy init --forget` writes a tombstone. Both halves of the check:
    /// the mirror path missing, and the mirror there with the repo it mirrors gone.
    #[test]
    fn a_mirror_whose_path_is_missing_stays_registered() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        crate::config::update(|c| c.registry = d.path().join("mirrors"));
        let gone = d
            .path()
            .join("unplugged")
            .join("repo")
            .join(".agent")
            .join("team");
        assert!(install::register(
            &gone,
            "acme/api",
            std::path::Path::new(""),
            std::path::Path::new("")
        ));
        // the other half: the mirror is there, the repo it was made from is not
        let there = d.path().join("mounted").join(".agent").join("team");
        std::fs::create_dir_all(&there).unwrap();
        assert!(install::register(
            &there,
            "acme/web",
            &d.path().join("deleted-checkout"),
            std::path::Path::new("")
        ));

        refresh_mirrors();
        let known = install::registered();
        assert_eq!(known.len(), 2, "a path we cannot see must not be forgotten");
        assert!(known.iter().any(|e| e.0 == gone));
        assert!(known.iter().any(|e| e.0 == there));
    }

    /// The invariant the CLI path broke: the dashboard learns about a widened scope by reading the
    /// file on the next tick, so whoever widened it, what is already listed there must not start.
    #[test]
    fn widening_the_scope_baselines_what_is_already_listed() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::config::test_lock();
        crate::config::update(|c| c.autorev = d.path().join("autorev"));
        // the third repo is covered by NEITHER scope: it must not join the baseline just because
        // the previous scope did not cover it either
        let rr = vec![
            pr_in("mine", "a/b"),
            pr_in("theirs", "other/thing"),
            pr_in("untouched", "third/one"),
        ];

        crate::autorev::set("a/b", true);
        let narrow = crate::autorev::scope();
        crate::autorev::set("other/thing", true);
        let wide = crate::autorev::scope();

        assert_eq!(newly_covered(&rr, &narrow, &wide), ["theirs"]);
        // and with that url in the baseline, the tick starts nothing
        assert_eq!(
            started(rr.clone(), &["theirs"], &[], &|r| wide.armed(r)),
            ["mine"]
        );
        assert_eq!(
            started(rr, &["theirs", "mine"], &[], &|r| wide.armed(r)),
            Vec::<String>::new()
        );
    }

    #[test]
    fn narrowing_the_scope_covers_nothing_new() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::config::test_lock();
        crate::config::update(|c| c.autorev = d.path().join("autorev"));
        let rr = vec![pr_in("mine", "a/b"), pr_in("theirs", "other/thing")];
        let everywhere = crate::autorev::scope(); // nothing armed
        crate::autorev::set("a/b", true);
        let narrow = crate::autorev::scope();
        assert_eq!(newly_covered(&rr, &everywhere, &narrow), Vec::<String>::new());
    }

    /// The genuine None branch: absorb_scope on a State that has never taken one. Passing two
    /// defaults to newly_covered() never reached `unwrap_or_default()` at all.
    #[test]
    fn the_first_scope_a_state_takes_covers_nothing_new() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::config::test_lock();
        crate::config::update(|c| c.autorev = d.path().join("autorev"));
        crate::autorev::set("a/b", true);

        let st = State::new();
        st.lock().sections = vec![section(
            "REVIEW REQUESTED",
            Some(vec![pr_in("mine", "a/b")]),
            None,
        )];
        st.set_auto(true, true); // an empty baseline, and everything listed consented to
        assert!(st.lock().auto_scope.is_some(), "set_auto takes the scope up too");

        let mut inner = st.lock();
        assert!(inner.auto_baseline.clone().unwrap().is_empty());
        inner.absorb_scope(&crate::autorev::scope());
        assert!(
            inner.auto_baseline.clone().unwrap().is_empty(),
            "the scope did not move, so nothing was baselined out of the consent just given"
        );
    }

    /// Arming while auto is off must leave the baseline None, so a later `a` still snapshots
    /// normally rather than finding a half-filled set.
    #[test]
    fn absorbing_a_scope_does_nothing_while_auto_is_off() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::config::test_lock();
        crate::config::update(|c| c.autorev = d.path().join("autorev"));
        let st = State::new();
        st.lock().sections = vec![section(
            "REVIEW REQUESTED",
            Some(vec![pr_in("mine", "a/b"), pr_in("theirs", "other/thing")]),
            None,
        )];
        crate::autorev::set("a/b", true);
        st.lock().absorb_scope(&crate::autorev::scope());
        crate::autorev::set("other/thing", true);
        st.lock().absorb_scope(&crate::autorev::scope());
        assert!(
            st.lock().auto_baseline.is_none(),
            "auto is off, so there is no baseline to fill"
        );

        st.set_auto(true, false); // a later `a` still snapshots normally
        assert_eq!(st.lock().auto_baseline.clone().unwrap().len(), 2);
    }

    /// The scope must ADVANCE, not just be read. Left on the first one, a repo armed at one tick
    /// stays "newly covered" at every later tick, so PRs arriving in it keep joining the baseline
    /// and auto never starts anything there again.
    #[test]
    fn a_repo_stops_being_new_once_its_scope_has_been_taken_up() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::config::test_lock();
        crate::config::update(|c| c.autorev = d.path().join("autorev"));
        crate::autorev::set("a/b", true);

        let st = State::new();
        st.lock().sections = vec![section(
            "REVIEW REQUESTED",
            Some(vec![pr_in("mine", "a/b")]),
            None,
        )];
        st.set_auto(true, true);

        crate::autorev::set("other/thing", true); // widen
        st.lock().sections = vec![section(
            "REVIEW REQUESTED",
            Some(vec![pr_in("mine", "a/b"), pr_in("theirs", "other/thing")]),
            None,
        )];
        st.lock().absorb_scope(&crate::autorev::scope());
        assert_eq!(
            st.lock().auto_baseline.clone().unwrap().len(),
            1,
            "what other/thing already had joins the baseline once"
        );

        // a NEW PR arrives in that same repo on a later tick; the scope has not moved, so it starts
        st.lock().sections = vec![section(
            "REVIEW REQUESTED",
            Some(vec![
                pr_in("mine", "a/b"),
                pr_in("theirs", "other/thing"),
                pr_in("fresh", "other/thing"),
            ]),
            None,
        )];
        st.lock().absorb_scope(&crate::autorev::scope());
        let seen = st.lock().auto_baseline.clone().unwrap();
        assert!(
            !seen.contains("fresh"),
            "a repo armed earlier is not newly covered again; its new PRs must start"
        );
    }

    /// The seam the consent count sat on: `a` reads the scope too, so a repo armed from the CLI
    /// seconds earlier does not look new to the next tick and get baselined out of what was asked.
    #[test]
    fn what_was_just_consented_to_is_not_baselined_away() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::config::test_lock();
        crate::config::update(|c| c.autorev = d.path().join("autorev"));
        crate::autorev::set("a/b", true);

        let st = State::new();
        st.lock().sections = vec![section(
            "REVIEW REQUESTED",
            Some(vec![pr_in("mine", "a/b"), pr_in("theirs", "other/thing")]),
            None,
        )];
        crate::autorev::set("other/thing", true); // the CLI widens it
        st.set_auto(true, true); // then `a`, with the count that widening produced

        st.lock().absorb_scope(&crate::autorev::scope());
        assert!(
            st.lock().auto_baseline.clone().unwrap().is_empty(),
            "the tick must not baseline what the prompt had already counted"
        );
    }

    #[test]
    fn auto_starts_nothing_when_nothing_is_armed() {
        let none = |_: &str| false;
        assert_eq!(
            started(vec![pr_in("mine", "a/b")], &[], &[], &none),
            Vec::<String>::new()
        );
    }

    #[test]
    fn set_auto_baselines_the_listed_rr_and_off_clears_it() {
        let st = State::new();
        st.lock().sections = vec![section(
            "REVIEW REQUESTED",
            Some(vec![pr("old"), pr("done")]),
            None,
        )];
        st.lock().reviews.insert("done".into(), "✓ approved".into());
        assert_eq!(st.lock().pending_rr(&|_| true), ["old"]);
        st.set_auto(true, false);
        assert_eq!(st.lock().auto_baseline.clone().unwrap().len(), 2);
        assert!(!st.lock().wake.is_set());
        st.set_auto(true, true);
        assert!(st.lock().auto_baseline.clone().unwrap().is_empty());
        assert!(st.lock().wake.is_set()); // refetch now so the listed PRs start
        st.set_auto(false, false);
        let inner = st.lock();
        assert!(!inner.auto && inner.auto_baseline.is_none());
    }

    /// The seam a release sits on: a review already in flight must refuse, not answer ok and do
    /// nothing. Dropping this bool left the flash saying "posting…" while the file stayed put.
    #[test]
    fn a_release_refuses_while_a_review_of_that_pr_is_running() {
        let _g = crate::config::test_lock();
        let d = tempfile::tempdir().unwrap();
        crate::config::update(|c| {
            c.demo = true;
            c.held_dir = d.path().join("held");
        });
        let h = held::Held {
            pr: pr_in("u", "a/b"),
            model: "opus".into(),
            verdict: crate::types::Verdict {
                verdict: "approve".into(),
                ..Default::default()
            },
            hello: String::new(),
            at: 100.0,
            ..Default::default()
        };
        held::put(&h).unwrap();

        let st = State::new();
        assert!(st.begin("u", "reviewing..."), "something else takes the row");
        assert!(!st.start_post_held(h.clone()), "the release refuses");
        assert!(held::get("a/b", 7).is_some(), "and the review is still waiting");
    }

    #[test]
    fn take_arrivals_empties_the_badge() {
        let st = State::new();
        st.lock().arrived.insert("t1".into(), 3);
        assert_eq!(st.take_arrivals().get("t1"), Some(&3));
        assert!(st.take_arrivals().is_empty());
    }

    #[test]
    fn the_header_gets_the_last_line_clipped() {
        assert_eq!(last_line("boom\nsecond line\n", 60, "?"), "second line");
        assert_eq!(last_line("   ", 60, "Error"), "Error");
        assert_eq!(last_line(&"x".repeat(80), 60, "?").len(), 60);
    }

    #[test]
    fn wake_wait_returns_true_when_set_and_false_on_timeout() {
        let w = Wake::default();
        assert!(!w.wait(Duration::from_millis(5)));
        w.set();
        assert!(w.wait(Duration::from_millis(5)));
        w.clear();
        assert!(!w.is_set());
    }

    #[test]
    fn sweep_runs_one_at_a_time() {
        let st = State::new();
        st.lock().sweeping = true;
        st.start_sweep(); // returns at once: the flag says one is already running
        assert!(st.lock().sweeping);
    }
}
