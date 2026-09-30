//! Tunables, paths and env overrides. Port of dashy/config.py.
//!
//! ponytail: one `Config` behind a global RwLock. Python had module globals that `load()`, the
//! settings screen and `--demo` all rewrote; here the same thing is `config::update(|c| ...)`.
//! Readers take a cheap clone with `config::get()`, so no lock is held across any real work.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};

use serde::{Deserialize, Serialize};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const MODELS: &[&str] = &["opus", "sonnet", "fable", "haiku"];
pub const EFFORTS: &[&str] = &["", "low", "medium", "high", "xhigh", "max"];
pub const DEPTHS: &[&str] = &["adaptive", "low", "medium", "high"];
pub const VOICES: &[&str] = &["review", "caveman", "bot"];
pub const HUNTERS: &[&str] = &["ponytail", "security", "tests", "perf", "humanizer", "spaghetti"];
/// What sort of change a review tags a PR as; the graph groups by it.
pub const KINDS: &[&str] = &[
    "feature",
    "fix",
    "security",
    "perf",
    "maintenance",
    "refactor",
    "docs",
    "tests",
    "deps",
];
pub const INTERVALS: &[u64] = &[60, 120, 300, 600, 900];
/// The refresh interval anything may set, whatever the picker offers: 30s to a day.
pub const INTERVAL_MIN: u64 = 30;
pub const INTERVAL_MAX: u64 = 86400;
/// The page carries the palettes; this is what the picker offers.
pub const THEMES: &[&str] = &["pencil", "dashy", "dracula", "gruvbox", "nord"];
pub const SUBS: &[&str] = &["all", "open", "off"];
/// Hours of REVIEWED history to show; `None` = all.
pub const WINDOWS: &[Option<u64>] = &[Some(1), Some(3), Some(6), Some(24), Some(168), Some(720), None];
pub const SPLASH_MIN: f64 = 1.0;

/// Verdict -> the status string every row and log reader shows.
pub fn status(verdict: &str) -> Option<&'static str> {
    match verdict {
        "approve" => Some("✓ approved"),
        "request_changes" => Some("✗ changes requested"),
        "comment" => Some("~ commented"),
        _ => None,
    }
}

/// Provider -> (base url, env var holding the api key). See llm.rs.
pub fn endpoints() -> HashMap<&'static str, (String, &'static str)> {
    HashMap::from([
        (
            "openrouter",
            (
                env_or("PRS_OPENROUTER_URL", "https://openrouter.ai/api/v1"),
                "OPENROUTER_API_KEY",
            ),
        ),
        (
            "local",
            (
                env_or("PRS_LOCAL_URL", "http://localhost:1234/v1"),
                "PRS_LOCAL_KEY",
            ),
        ),
    ])
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn env_or(var: &str, default: &str) -> String {
    std::env::var(var)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_path(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(default))
}

/// Everything the app reads at runtime. Defaults come from the environment, then `load()` lays the
/// saved settings over them, then flags and the settings screen change them through `update()`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub models: Vec<String>,
    pub model: String,
    pub effort: String,
    pub depth: String,
    pub voice: Vec<String>,
    pub hunter: Vec<String>,
    /// Spells on the quick list: the sidebar and the right-click menu. Names of files in spells_dir.
    pub spells: Vec<String>,
    /// Text file appended to the review prompt.
    pub instructions: String,
    /// The OLD single team checkout; migrated into `teams`.
    pub team: PathBuf,
    /// One checkout per team, dir name = slug.
    pub teams: PathBuf,
    /// general.md + one md per repo.
    pub memory_dir: PathBuf,
    /// jsonl, one review per line.
    pub log: PathBuf,
    pub debug_log: PathBuf,
    /// The solo locations, kept so K can show where memory lives and leaving has a home.
    pub local_memory: PathBuf,
    pub local_log: PathBuf,
    pub interval: u64,
    pub notify: bool,
    /// LAN presence (lan.rs): announce this gitdashy and list the others. The ☰ menu toggles it.
    pub lan: bool,
    pub theme: String,
    pub sub: String,
    pub window: Option<u64>,
    pub drafts: bool,
    /// Post each finding as a comment on the line it is about, beside the body. Off by default: it
    /// changes what lands on someone else's PR, and on a repo that requires conversation resolution
    /// every nit becomes a thread that gates the merge button.
    pub inline: bool,
    /// Toggled-on sources for the TEAM section: "org:<owner>" or "team:<key>". Empty = no TEAM section.
    pub scopes: Vec<String>,
    /// url -> the updatedAt that was read, so a PR that moves goes unread again. Kept here rather than in
    /// localStorage because the webview's origin changes every launch (see `hinted`). The page keeps the newest.
    pub read: HashMap<String, String>,
    /// url -> the updatedAt it was hidden at, same shape as `read`: a PR that moves past it shows again.
    pub hidden: HashMap<String, String>,
    /// The welcome hint has been shown. ponytail: config, not localStorage: the GUI serves itself on
    /// a fresh random port every launch, so the webview's origin, and its storage with it, is new
    /// each time. Anything that must be remembered across launches belongs on this side.
    pub hinted: bool,
    /// Show the key hint on every button and settings row. On by default; the sheet and the rail's
    /// View group both switch it.
    pub keyhints: bool,
    /// The PR pane's layout: "order", "off", "shut" and "out" each list section names. Config for the same reason as `hinted`.
    pub pane: HashMap<String, Vec<String>>,
    /// The left sidebar's layout, same shape as `pane`.
    pub side: HashMap<String, Vec<String>>,
    /// The version whose release notes were last shown; "" before the first launch that recorded one.
    pub seen: String,
    /// Runtime picks land here. `None` (demo) means never write.
    pub settings: Option<PathBuf>,
    /// Pre-reviews of your own PRs.
    pub self_dir: PathBuf,
    /// Friday reports, one HTML file per day written.
    pub reports: PathBuf,
    /// Reviews that finished and are waiting to be posted. See held.rs.
    pub held_dir: PathBuf,
    /// Dated learning events for the Knowledge chart, one JSON object per line.
    pub learning: PathBuf,
    pub backups: PathBuf,
    pub bindings: PathBuf,
    /// Which repos auto-review is armed for. Its own store: see autorev.rs.
    pub autorev: PathBuf,
    /// Which repo holds each repo's database. See dbrepo.rs.
    pub dbrepo: PathBuf,
    /// One .md per spell, named after it. See spells.rs.
    pub spells_dir: PathBuf,
    /// Mirrors `gitdashy init` registered.
    pub registry: PathBuf,
    pub corpus_home: PathBuf,
    /// --demo: fakes instead of GitHub, the model and the network.
    pub demo: bool,
    pub debug: bool,
}

impl Default for Config {
    fn default() -> Self {
        let split = |v: &str| -> Vec<String> {
            std::env::var(v)
                .unwrap_or_default()
                .split(',')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        };
        let memory_dir = env_path("PRS_MEMORY", ".prs_memory");
        let log = env_path("PRS_LOG", ".prs_reviewed.jsonl");
        Config {
            models: MODELS
                .iter()
                .map(|s| s.to_string())
                .chain(split("PRS_MODELS"))
                .collect(),
            model: env_or("PRS_MODEL", "opus"),
            effort: std::env::var("PRS_EFFORT").unwrap_or_else(|_| "medium".into()),
            depth: env_or("PRS_DEPTH", "adaptive"),
            voice: {
                let v = std::env::var("PRS_VOICE").unwrap_or_else(|_| "review".into());
                v.split(',').filter(|s| !s.is_empty()).map(String::from).collect()
            },
            hunter: split("PRS_HUNTER"),
            spells: Vec::new(),
            instructions: std::env::var("PRS_INSTRUCTIONS").unwrap_or_default(),
            team: env_path("PRS_TEAM", ".prs_team"),
            teams: env_path("PRS_TEAMS", ".prs_teams"),
            local_memory: memory_dir.clone(),
            local_log: log.clone(),
            memory_dir,
            log,
            debug_log: env_path("PRS_DEBUG_LOG", ".prs_debug.log"),
            interval: 300,
            notify: std::env::var("PRS_NOTIFY").map(|v| v != "0").unwrap_or(true),
            lan: std::env::var("PRS_LAN").map(|v| v != "0").unwrap_or(true),
            theme: env_or("PRS_THEME", "pencil"),
            sub: "all".into(),
            window: Some(24),
            drafts: false,
            inline: std::env::var("PRS_INLINE").map(|v| v != "0").unwrap_or(false),
            scopes: Vec::new(),
            read: HashMap::new(),
            hidden: HashMap::new(),
            hinted: false,
            keyhints: true,
            pane: HashMap::new(),
            side: HashMap::new(),
            seen: String::new(),
            settings: Some(env_path("PRS_SETTINGS", ".prs_settings.json")),
            self_dir: home().join(".prs_reviews"),
            reports: home().join(".prs_reports"),
            held_dir: home().join(".prs_held"),
            // ponytail: nowhere in a test build. memory::append records an event and dozens of tests call it
            // for real; the first run of this wrote 21 fake events into the real ~/.prs_learning.jsonl. An
            // empty path makes record() a no-op, so a test that wants events names a temp file itself.
            learning: if cfg!(test) {
                PathBuf::new()
            } else {
                home().join(".prs_learning.jsonl")
            },
            backups: home().join(".prs_backups"),
            bindings: env_path("PRS_BINDINGS", ".prs_bindings"),
            autorev: env_path("PRS_AUTOREVIEW", ".prs_autoreview"),
            dbrepo: env_path("PRS_DBREPO", ".prs_dbrepo"),
            spells_dir: env_path("PRS_SPELLS", ".prs_spells"),
            registry: home().join(".prs_mirrors"),
            corpus_home: home().join(".agent-corpus"),
            demo: false,
            debug: false,
        }
    }
}

/// What the settings file holds: the keys the dashboard can change. Mirrors config.SAVED.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Saved {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subs: Option<String>,
    /// `Some(None)` is "all", written as JSON null.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "window_field")]
    pub window: Option<Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drafts: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hinted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyhints: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<HashMap<String, Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<HashMap<String, Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "one_or_many"
    )]
    pub voice: Option<Vec<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "one_or_many"
    )]
    pub hunter: Option<Vec<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "one_or_many"
    )]
    pub spells: Option<Vec<String>>,
}

mod window_field {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<S: Serializer>(v: &Option<Option<u64>>, s: S) -> Result<S::Ok, S::Error> {
        v.unwrap_or(None).serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<u64>>, D::Error> {
        Ok(Some(Option::<u64>::deserialize(d)?))
    }
}

/// ponytail: a saved checklist may be a string from an older version; accept either shape.
fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<V>::deserialize(d)? {
        None => None,
        Some(V::One(s)) => Some(vec![s]),
        Some(V::Many(v)) => Some(v),
    })
}

/// Keys `salvage` threw away, until someone says so. Drained by the dashboard into a notice.
static DROPPED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The settings keys a damaged file lost, taken once. The caller is expected to show them.
pub fn dropped_settings() -> Vec<String> {
    std::mem::take(&mut *DROPPED.lock().unwrap_or_else(|e| e.into_inner()))
}

impl Saved {
    /// Read a settings file; `{}` for a missing one, and whatever parses of a damaged one.
    pub fn read(path: &Path) -> Saved {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Saved::default();
        };
        serde_json::from_str(&text).unwrap_or_else(|_| Saved::salvage(&text))
    }

    /// Field by field, keeping the ones that parse.
    ///
    /// ponytail: serde stops at the FIRST value it cannot read, and the whole file was thrown away
    /// with it. `"interval": "300"` — a number written as a string by a hand edit, an older version
    /// or a merge — silently took the model, the depth and every other setting with it, and the next
    /// save (a settings change, or mark_seen after an update) wrote the defaults over the file. The
    /// one bad field is dropped instead, and `apply` then holds what is left to the same rules as
    /// the settings screen.
    fn salvage(text: &str) -> Saved {
        let Ok(fields) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(text) else {
            log::warn!("settings: not a JSON object, so none of it was read");
            return Saved::default();
        };
        let (mut kept, mut dropped) = (serde_json::Map::new(), Vec::new());
        for (key, value) in fields {
            let one = serde_json::Value::Object([(key.clone(), value.clone())].into_iter().collect());
            match serde_json::from_value::<Saved>(one) {
                Ok(_) => {
                    kept.insert(key, value);
                }
                Err(_) => dropped.push(key),
            }
        }
        if !dropped.is_empty() {
            log::warn!(
                "settings: ignored {}; read the rest of the file",
                dropped.join(", ")
            );
            // ponytail: the log reaches a file under --debug and nowhere else, and the next save
            // writes the default over what was dropped. Held here so the dashboard can say it once,
            // on screen, which is the only place the person who edited the file will look.
            DROPPED.lock().unwrap_or_else(|e| e.into_inner()).extend(dropped);
        }
        serde_json::from_value(serde_json::Value::Object(kept)).unwrap_or_default()
    }
}

static CONFIG: OnceLock<RwLock<Config>> = OnceLock::new();

fn cell() -> &'static RwLock<Config> {
    CONFIG.get_or_init(|| RwLock::new(Config::default()))
}

/// Whether LAN presence is on, without cloning the config: lan.rs asks on every packet.
pub fn lan() -> bool {
    cell().read().unwrap_or_else(|e| e.into_inner()).lan
}

/// A snapshot of the current config.
pub fn get() -> Config {
    cell().read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Change the config in place.
pub fn update(f: impl FnOnce(&mut Config)) {
    let mut c = cell().write().unwrap_or_else(|e| e.into_inner());
    f(&mut c);
}

/// Saved settings override the defaults; an env var or CLI flag still wins over the file.
/// Also normalises the checklists: a box that no longer exists is dropped, and voice is never empty.
pub fn load() {
    update(|c| {
        let saved = c.settings.as_deref().map(Saved::read).unwrap_or_default();
        apply(c, saved, &|v| std::env::var_os(v).is_some());
    });
}

/// A saved file over a Config. Pure, and `env` is passed in, so this is the same code the tests run.
///
/// ponytail: a value outside what the settings screen would accept is IGNORED and the default stands,
/// the way normalise() already drops a checkbox that no longer exists. The file is not always written
/// by us — it is edited by hand, merged, or left behind by an older version — and `"interval": 0` cost
/// a refresh every ten seconds against the rate limit. The default may come from an env var, so this
/// leaves the value alone rather than writing a constant over it.
///
/// ponytail: this was the body of `load()`, which writes a process-global behind a lock and so was
/// never driven by a test — every `if let Some(v)` here could be deleted and the suite stayed green.
/// It is a function now because that is the only way the precedence below can be asserted.
pub fn apply(c: &mut Config, saved: Saved, env: &dyn Fn(&str) -> bool) {
    if let (Some(v), false) = (saved.model.filter(|v| model_ok(v)), env("PRS_MODEL")) {
        c.model = v.trim().to_string(); // trimmed on the way in, as post_settings does
    }
    if let Some(v) = saved.interval.filter(|v| interval_ok(*v)) {
        c.interval = v;
    }
    if let Some(v) = saved.subs {
        c.sub = v;
    }
    if let Some(v) = saved.window {
        c.window = v;
    }
    if let Some(v) = saved.drafts {
        c.drafts = v;
    }
    // ponytail: the env var wins, same as PRS_NOTIFY. `--inline` is how a run is opted in, and a
    // saved `false` from the last session must not switch it back off under the flag.
    if let (Some(v), false) = (saved.inline, env("PRS_INLINE")) {
        c.inline = v;
    }
    if let Some(v) = saved.scopes {
        c.scopes = v;
    }
    if let Some(v) = saved.read {
        c.read = v;
    }
    if let Some(v) = saved.hidden {
        c.hidden = v;
    }
    if let Some(v) = saved.hinted {
        c.hinted = v;
    }
    if let Some(v) = saved.keyhints {
        c.keyhints = v;
    }
    if let Some(v) = saved.pane {
        c.pane = v;
    }
    if let Some(v) = saved.side {
        c.side = v;
    }
    if let Some(v) = saved.seen {
        c.seen = v;
    }
    if let (Some(v), false) = (
        saved.depth.filter(|v| DEPTHS.contains(&v.as_str())),
        env("PRS_DEPTH"),
    ) {
        c.depth = v;
    }
    if let (Some(v), false) = (
        saved.effort.filter(|v| EFFORTS.contains(&v.as_str())),
        env("PRS_EFFORT"),
    ) {
        c.effort = v;
    }
    if let (Some(v), false) = (saved.notify, env("PRS_NOTIFY")) {
        c.notify = v;
    }
    // ponytail: only PRS_LAN=0 or 1 is an on/off that wins over the menu. A list of networks says
    // WHERE, not whether, so the saved toggle still applies under it.
    if let (Some(v), false) = (
        saved.lan,
        std::env::var("PRS_LAN").is_ok_and(|v| v == "0" || v == "1"),
    ) {
        c.lan = v;
    }
    if let (Some(v), false) = (
        saved.theme.filter(|v| THEMES.contains(&v.as_str())),
        env("PRS_THEME"),
    ) {
        c.theme = v;
    }
    if let (Some(v), false) = (saved.voice, env("PRS_VOICE")) {
        c.voice = v;
    }
    if let (Some(v), false) = (saved.hunter, env("PRS_HUNTER")) {
        c.hunter = v;
    }
    if let Some(v) = saved.spells {
        c.spells = v;
    }
    normalise(c);
}

/// What a value has to be before anything takes it, wherever it came from: the settings file in
/// `apply`, the settings screen in `web::post_settings`. One definition, so the two cannot drift.
pub fn interval_ok(v: u64) -> bool {
    (INTERVAL_MIN..=INTERVAL_MAX).contains(&v)
}

/// Model names vary (openrouter's contain a slash), so only length is checked.
pub fn model_ok(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty() && name.chars().count() <= 60
}

/// Drop checklist boxes that no longer exist; voice is never empty. The one place that rule lives.
pub fn normalise(c: &mut Config) {
    c.voice.retain(|v| VOICES.contains(&v.as_str()));
    if c.voice.is_empty() {
        c.voice = vec!["review".into()];
    }
    c.hunter.retain(|h| HUNTERS.contains(&h.as_str()));
}

/// Everything the settings can change, in the shape `save` writes.
pub fn snapshot(c: &Config) -> Saved {
    Saved {
        model: Some(c.model.clone()),
        interval: Some(c.interval),
        subs: Some(c.sub.clone()),
        window: Some(c.window),
        drafts: Some(c.drafts),
        inline: Some(c.inline),
        scopes: Some(c.scopes.clone()),
        read: Some(c.read.clone()),
        hidden: Some(c.hidden.clone()),
        hinted: Some(c.hinted),
        keyhints: Some(c.keyhints),
        pane: Some(c.pane.clone()),
        side: Some(c.side.clone()),
        seen: Some(c.seen.clone()),
        depth: Some(c.depth.clone()),
        effort: Some(c.effort.clone()),
        notify: Some(c.notify),
        lan: Some(c.lan),
        theme: Some(c.theme.clone()),
        voice: Some(c.voice.clone()),
        hunter: Some(c.hunter.clone()),
        spells: Some(c.spells.clone()),
    }
}

/// Held across every read-change-save of the settings. Without it two writers copy the config, and the
/// later save undoes the other's change: in memory, and in the file a restart reads.
/// ponytail: one global lock, settings writes are rare and quick. It covers the writers that SAVE
/// (post_settings, update::mark_seen); a plain `config::update` elsewhere does not take it, and
/// post_settings' whole-config write can still undo one that lands mid-post.
pub static SAVING: Mutex<()> = Mutex::new(());

/// Persist the settings. ponytail: `settings: None` (demo) means never write.
pub fn save(values: &Saved) -> std::io::Result<()> {
    if let Some(p) = get().settings {
        // a temp file renamed over: a write cut short never leaves half a file that reads back as {}
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(values).unwrap_or_default())?;
        std::fs::rename(tmp, p)?;
    }
    Ok(())
}

/// The lock for any test that touches a process-global, whichever module the test lives in.
///
/// ponytail: one, because twelve module locks never excluded each other and about a dozen tests failed
/// at random, each reporting an assertion that belonged to whatever had rewritten the config under it
/// (#134). It lives beside what it guards.
#[cfg(test)]
pub fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// `~`-shortened path for display.
pub fn tilde(p: &Path) -> String {
    let h = home();
    match p.strip_prefix(&h) {
        Ok(rest) if h != Path::new(".") => format!("~/{}", rest.display()),
        _ => p.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// serde stops at the first value it cannot read. One field of the wrong type must not take the
    /// rest of the file with it: what is left is written back over the file by the next save.
    #[test]
    fn a_wrong_typed_field_does_not_take_the_rest_of_the_file_with_it() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        std::fs::write(
            &p,
            r#"{"interval":"300","model":"sonnet","depth":"high","voice":"caveman","drafts":true}"#,
        )
        .unwrap();
        let s = Saved::read(&p);
        assert_eq!(s.interval, None, "the wrong-typed one is dropped");
        assert_eq!(s.model.as_deref(), Some("sonnet"));
        assert_eq!(s.depth.as_deref(), Some("high"));
        assert_eq!(s.voice, Some(vec!["caveman".into()]));
        assert_eq!(s.drafts, Some(true));

        assert_eq!(dropped_settings(), ["interval"], "and it is there to be said");
        assert!(dropped_settings().is_empty(), "taken once");

        // the shapes with their own deserializers: null is a VALUE for window, not a failure
        std::fs::write(&p, r#"{"voice":5,"window":null,"model":"opus"}"#).unwrap();
        let s = Saved::read(&p);
        assert_eq!(s.voice, None, "a number is not a checklist");
        assert_eq!(s.window, Some(None), "null is how `all` is written");
        assert_eq!(s.model.as_deref(), Some("opus"));
        assert_eq!(dropped_settings(), ["voice"]);

        // a whole file that is not an object still reads as nothing, as it did before
        std::fs::write(&p, "not json at all").unwrap();
        assert_eq!(Saved::read(&p), Saved::default());
        assert_eq!(Saved::read(&d.path().join("nope.json")), Saved::default());
    }

    #[test]
    fn saved_round_trips_and_accepts_old_shapes() {
        let s: Saved = serde_json::from_str(r#"{"voice":"caveman","window":null,"drafts":true}"#).unwrap();
        assert_eq!(s.voice, Some(vec!["caveman".into()]));
        assert_eq!(s.window, Some(None));
        let text = serde_json::to_string(&s).unwrap();
        assert!(text.contains("\"window\":null"));
        assert!(!text.contains("model"));
    }

    #[test]
    fn equipped_spells_round_trip() {
        let s: Saved = serde_json::from_str(r#"{"spells":["auth-check","test-gaps"]}"#).unwrap();
        assert_eq!(
            s.spells,
            Some(vec!["auth-check".to_string(), "test-gaps".to_string()])
        );
        let c = Config {
            spells: vec!["auth-check".into()],
            ..Default::default()
        };
        assert_eq!(snapshot(&c).spells, Some(vec!["auth-check".to_string()]));
    }

    /// PRS_INLINE outranks the saved value, the way PRS_NOTIFY does. Inline comments post on other
    /// people's PRs, so the environment has to be able to hold the switch down whatever the settings
    /// file picked up from an earlier `--inline` run.
    #[test]
    fn the_environment_outranks_a_saved_inline_setting() {
        let saved = |json: &str| serde_json::from_str::<Saved>(json).unwrap();
        let none = |_: &str| false;
        let set = |_: &str| true;

        let mut c = Config::default();
        apply(&mut c, saved(r#"{"inline":true}"#), &none);
        assert!(c.inline, "with nothing in the environment the file decides");

        // the env var is set (to whatever): the file must not switch it back
        let mut c = Config {
            inline: false,
            ..Default::default()
        };
        apply(&mut c, saved(r#"{"inline":true}"#), &set);
        assert!(!c.inline, "PRS_INLINE decides, not the saved file");

        // and a file that says nothing leaves the default alone
        let mut c = Config {
            inline: true,
            ..Default::default()
        };
        apply(&mut c, saved("{}"), &none);
        assert!(c.inline);
    }

    /// Every `if let Some(v)` in apply(): a saved file must reach the config, and a setting the
    /// file leaves out must keep the default rather than being cleared.
    #[test]
    fn a_saved_file_reaches_every_setting() {
        let none = |_: &str| false;
        let json = r#"{
            "model":"sonnet","interval":600,"subs":"open","window":168,"drafts":true,"inline":true,"scopes":["org:acme"],"read":{"u":"t"},
            "hinted":true,"keyhints":false,"seen":"2.1.0","depth":"high","effort":"max","notify":true,"lan":false,
            "theme":"nord","voice":["caveman"],"hunter":["security"],"spells":["auth-check"]
        }"#;
        let saved: Saved = serde_json::from_str(json).unwrap();
        let mut c = Config::default();
        apply(&mut c, saved, &none);
        assert_eq!(c.model, "sonnet");
        assert_eq!(c.interval, 600);
        assert_eq!(c.sub, "open");
        assert_eq!(c.window, Some(168));
        assert!(c.inline);
        assert!(c.drafts);
        assert_eq!(c.scopes, ["org:acme"]);
        assert_eq!(c.read.get("u").map(String::as_str), Some("t"));
        assert!(c.hinted);
        assert!(!c.keyhints);
        assert_eq!(c.seen, "2.1.0");
        assert_eq!(c.depth, "high");
        assert_eq!(c.effort, "max");
        assert!(c.notify);
        assert!(!c.lan, "the menu's LAN toggle survives a restart");
        assert_eq!(c.theme, "nord");
        assert_eq!(c.voice, vec!["caveman"]);
        assert_eq!(c.hunter, vec!["security"]);
        assert_eq!(c.spells, vec!["auth-check"]);
    }

    #[test]
    fn an_empty_file_changes_nothing() {
        let d = Config::default();
        let mut c = Config::default();
        apply(&mut c, Saved::default(), &|_| false);
        assert_eq!(c.model, d.model);
        assert_eq!(c.window, d.window);
        assert_eq!(c.drafts, d.drafts);
        assert_eq!(c.keyhints, d.keyhints);
        assert_eq!(c.theme, d.theme);
    }

    /// An env var or a CLI flag beats the file, and only for the settings that say so.
    #[test]
    fn the_environment_beats_the_file() {
        let set = |v: &str| ["PRS_MODEL", "PRS_THEME", "PRS_VOICE"].contains(&v);
        let json = r#"{"model":"sonnet","theme":"nord","voice":["caveman"],"subs":"open","keyhints":false}"#;
        let saved: Saved = serde_json::from_str(json).unwrap();
        let mut c = Config {
            model: "opus".into(),
            theme: "pencil".into(),
            voice: vec!["review".into()],
            ..Default::default()
        };
        apply(&mut c, saved, &set);
        assert_eq!(c.model, "opus", "PRS_MODEL was set, so the file must not win");
        assert_eq!(c.theme, "pencil");
        assert_eq!(c.voice, vec!["review"]);
        // no env var guards these, so the file still reaches them
        assert_eq!(c.sub, "open");
        assert!(!c.keyhints);
    }

    /// A file nobody validated: every value outside what the settings screen accepts is ignored and
    /// the default stands. `"interval": 0` is the one that cost something — a refresh every ten seconds.
    #[test]
    fn a_bad_saved_value_leaves_the_default() {
        let d = Config::default();
        let json = r#"{
            "model":"","interval":0,"depth":"nope","effort":"turbo","theme":"neon",
            "subs":"open"
        }"#;
        let mut c = Config::default();
        apply(&mut c, serde_json::from_str(json).unwrap(), &|_| false);
        assert_eq!(c.model, d.model);
        assert_eq!(c.interval, d.interval);
        assert_eq!(c.depth, d.depth);
        assert_eq!(c.effort, d.effort);
        assert_eq!(c.theme, d.theme);
        assert_eq!(c.sub, "open", "a good value in the same file still lands");

        // the far end of each range, which is where an off-by-one would hide
        let json = format!(
            r#"{{"model":"{}","interval":{}}}"#,
            "x".repeat(61),
            INTERVAL_MAX + 1
        );
        let mut c = Config::default();
        apply(&mut c, serde_json::from_str(&json).unwrap(), &|_| false);
        assert_eq!(c.model, d.model);
        assert_eq!(c.interval, d.interval);

        // and the edges themselves are taken
        let json = format!(r#"{{"model":"{}","interval":{}}}"#, "x".repeat(60), INTERVAL_MAX);
        let mut c = Config::default();
        apply(&mut c, serde_json::from_str(&json).unwrap(), &|_| false);
        assert_eq!(c.model, "x".repeat(60));
        assert_eq!(c.interval, INTERVAL_MAX);
    }

    #[test]
    fn normalise_drops_unknown_boxes_and_keeps_a_voice() {
        let mut c = Config {
            voice: vec!["ponytail".into()],
            hunter: vec!["ponytail".into(), "nope".into()],
            ..Default::default()
        };
        normalise(&mut c);
        assert_eq!(c.voice, vec!["review"]);
        assert_eq!(c.hunter, vec!["ponytail"]);
    }

    #[test]
    fn status_strings() {
        assert_eq!(status("approve"), Some("✓ approved"));
        assert_eq!(status("nope"), None);
    }
}
