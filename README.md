<p align="center">
  <img src="header.png" alt="git-dashy: smarter reviews, better code" width="900">
</p>

<h1 align="center">git-dashy</h1>

<p align="center">Smarter reviews. Better code. — a desktop PR dashboard with a one-key Claude review.</p>

A desktop dashboard for the PRs you actually care about — yours, the ones waiting on your
review, the ones assigned to you — with a one-key Claude review that posts the verdict back to
GitHub.

<p align="center">
  <img src="screenshot.png" alt="github-dashy" width="900">
</p>

## Requirements

- Nothing to install but the binary: gitdashy is one Rust executable (the desktop window, the server
  behind it and the CLI the hooks call). `--browser` opens the same dashboard in your browser instead.
  On Linux the binary links the system webview, so that must be installed, even for `--browser`:
  `apt install libwebkit2gtk-4.1-0 libgtk-3-0` (dnf: `webkit2gtk4.1 gtk3`, pacman: `webkit2gtk-4.1 gtk3`).
  That needs Ubuntu 22.04 / Debian 12 or newer.
- A GitHub token in `$GH_TOKEN` or `$GITHUB_TOKEN` (scope: `repo`). Nothing shells out to `gh` —
  gitdashy talks to the API itself. (`$GITHUB_API` points it at GitHub Enterprise.)
- [`claude`](https://claude.com/claude-code) on PATH, for the review feature only

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/NeoMedSys/git-dashy/main/install.sh | sh
```

Downloads the newest release binary for your OS into `~/.local/bin/gitdashy` (override with `BIN=`).
Re-running it updates in place. To install an older release, name its tag:

```sh
REF=v2.0.0 ./install.sh
```

On NixOS, use the flake. It installs a launcher: on first run it downloads the release binary into
`~/.local/lib/gitdashy` and runs it in an FHS env that carries the webview. The app updates that file
itself, like everywhere else.

```sh
nix run github:NeoMedSys/git-dashy
```

In a NixOS flake: add the input `gitdashy.url = "github:NeoMedSys/git-dashy";` and put
`inputs.gitdashy.packages.x86_64-linux.default` in `environment.systemPackages` (the app menu entry
comes with it).

Or build it yourself (Rust stable; on Linux also `libwebkit2gtk-4.1-dev libgtk-3-dev`; Node + pnpm
for the frontend, which is built first and embedded into the binary):

```sh
git clone https://github.com/NeoMedSys/git-dashy && cd git-dashy
pnpm install && pnpm build
cargo install --path src-tauri
```

## Run

```sh
gitdashy                  # opens the desktop app, 300s refresh
gitdashy --browser        # the same dashboard in your browser
gitdashy --interval 60
gitdashy --auto           # review every review-requested PR that shows up from now on
gitdashy --model sonnet
gitdashy --effort high --depth adaptive   # claude effort level; review depth judged from the PR size
gitdashy --instructions review-rules.md   # your own text, appended to every review prompt
gitdashy --inline         # also post each finding as a comment on the line it is about
gitdashy --version        # 1.16.0
gitdashy --demo           # canned PRs, fake reviewer — no token, no claude, no real log
gitdashy --debug          # also append API calls, reviews, ticks and every swallowed traceback to ~/.prs_debug.log
gitdashy --help
gitdashy sync-memory --into .agent/team   # mirror the shared memory for an agent session in this repo
gitdashy remember "the viewer owns mask state"   # file what a coding session learned
gitdashy self-review 42   # pre-review your OWN PR; nothing is posted
```

The list is a table — `age · repo · pr · title · author · state` — and above it one tab per queue:
`all`, `mine`, `review requested`, `assigned`, `reviewed`, each with its count. **Tabs stack**: click a
second one and both queues show, each under its own header; clicking the last one off falls back to
`all`. Beside the tabs, `CI failing` and `Drafts` narrow whatever the tabs are showing, each counting only
what is on screen; press one again to clear it. `[` and `]` walk
one tab at a time and replace the pick. Beside it, `⏎`
opens a pane on the selected PR: its branch and diff size, what CI thinks, and what the last review
found, line by line. Everything in the pane is fetched for that one PR, only when you select it.

MINE rows show GitHub's review decision for your own PRs: `✓ approved`, `✗ changes requested`,
`· awaiting review`, or `↻ re-review requested` when you pushed after a changes-requested and asked
again but the reviewer has not looked yet. Next to it, one chip per reviewer: `✓bob` approved,
`✗bob` requested changes, `·bob` asked but not looked yet, `~bob` commented. Every open row also carries the CI
state of its head commit: `ci✓` green, `ci✗` failed, `ci●` still running, nothing when the repo has no checks.

`p` on one of your own rows **pre-reviews it**: the same reviewer, the same prompt, the same memory —
but nothing is posted and nothing is logged. The review is written to `~/.prs_reviews/<owner>__<repo>__<n>.md`, and `p` again reopens it — including
after a restart, because the name is derived rather than remembered. Once the PR has been updated since
that file was written, `p` offers a fresh pre-review instead of handing you one that describes the old
diff. It is a pass before you ask a person, not a substitute for one; GitHub will not let you
approve your own PR, and a verdict on your own work is a second opinion from the same head.

The pane shows whether a pre-review exists for the selected PR, when it was written, and whether the
PR has moved since — so a review of a diff you have already pushed over says so rather than reading as
current. That survives restarting gitdashy: the file's name is derived from owner, repo and number, so
nothing has to be remembered.

Its findings do not become facts on their own. They wait in a separate pool, and only a later **real**
review that lands on the same fact by itself confirms one — the pre-review and the real review are the
same model on the same diff, so counting them as two would measure how often you pre-reviewed rather
than whether the fact recurred.

The key hints sit on one row at the bottom, grouped by what they act on. Every key below also has a
button somewhere on the page.

## Keys

| key | what |
|-----|------|
| `j` / `k`, `↑` / `↓` | move |
| `o` | open the PR in your browser |
| right-click | everything you can do to that PR, as a menu — the same list the pane's **options** button opens, `2` `Tab` among them |
| `y` | copy the PR URL to the clipboard |
| `+` | on a MINE row: pick a collaborator (or type a login) and request their review |
| `p` | on a MINE row: pre-review your own PR. Nothing is posted; `p` again inspects it in the app — where you can discuss it with the agent — and offers a fresh one once the PR has changed since |
| `Enter` | show / hide the detail pane for the selected PR |
| `r` | on a REVIEW REQUESTED row: Claude reviews it and posts the verdict. Refused while a review is waiting to post on that PR; `Y` posts or drops it first |
| `R` | the same, with instructions for the agent first: what to focus on, what to leave alone. They stay on this machine and are never posted |
| `v` | read the full review of the selected PR — any row that has one, not only REVIEWED |
| `f` | refresh now |
| `a` | toggle auto mode |
| `t` | pick the REVIEWED window: 1h / 3h / 6h / 1d / 1w / 1mo / all |
| `Space` | on a REVIEWED row: unfold / fold the older reviews of that PR (stacked under the newest, collapsed by default) |
| `s` | pick summary lines: all / open PRs only / off |
| `/` | focus the filter box: title, repo, author or number; `Esc` clears it |
| `D` | show / hide draft PRs (hidden by default) |
| `X` | hide the selected PR until it moves (a push, a comment), or unhide it; the **Hidden** chip in the filter bar shows only the hidden ones |
| `m` | pick the model: opus / sonnet / fable / haiku |
| `d` | pick review depth: adaptive / low / medium / high |
| `e` | pick claude effort: default / low / medium / high / xhigh / max |
| `x` | tick how the posted review is phrased: review / caveman / bot, any mix, at least one |
| `h` | tick extra hunters, each a section of its own findings: ponytail / security / tests / perf / humanizer / spaghetti |
| `i` | pick the refresh interval: 1 / 2 / 5 / 10 / 15 min (the footer counts down to the next one) |
| `n` | the knowledge panel on **inspect**, open at this repo's facts |
| `g` | the knowledge panel on **inspect**: what the memory knows, file by file — your files and each team's. `x` removes a fact: from yours at once, from a team's by opening a pull request on the team's repo. Nothing is typed in: facts only arrive through reviews. A team's brief and `agents.md` are shown too, and `e` proposes a change to one as a pull request |
| `W` | the knowledge panel on **drafts**: what a review proposed and no second review has confirmed — `t` makes one a fact (for a draft in a team's pool, `t` opens a pull request adding it to the team's file), `x` drops it, `s` scans for drafts that are one fact worded twice (the model reads the candidates first; `esc` skips it) |
| `K` | the knowledge panel on **stats**: how fast the memory learns, as a chart — facts gained (by how: seen twice, by hand, from a teammate, or earlier), drafts proposed (by where from), team arrivals (by whose); filter by team, repo and person, per day or week. The panel's tabs are stats / inspect / drafts (`K` `g` `W`, or `[` `]` to step), with `Z` dream as an action in it (a dream has the model propose a tidier version of your memory files, reading the teams' too so yours do not repeat theirs; you see before and after, nothing is written until you accept, and team files are never rewritten); `knowledge panel` in the rail's Knowledge group opens it too |
| `Y` | on a row waiting to post: inspect the held review — discuss it with the agent, then post it or drop it. Releasing asks the PR first, so a review that did land while gitdashy was told otherwise is not posted twice; the row says `(already on the PR)` |
| `F` | follow someone: a pill in the footer for what they have been working on, from the PRs they opened or updated (see Following people) |
| `b` | bind the selected repo to a team — `1-8` picks one, `o` binds the whole owner, `x` unbinds |
| `G` | cycle board, graph and Necronomicon. The Necronomicon is memory's main points as a book: Learn (its button; nothing learns by itself, but a day after the last learn the footer's book turns into "learn" and the button shines) has the model write a few points for general and for each repo, citing the facts behind them. Points rank by how often a review proposes their facts again and fade when nothing does; ▾ deranks one, and whatever sinks far enough waits in the depths until you dig for it and ▴ raise it. Drafts not yet facts show as whispers. The graph always draws the whole board, so the filter row and the queue tab are cleared and ignored: every PR linked to its repo and author, sized by lines changed, colored by review state. Tabs regroup it by kind (the review's tag, else the title's `feat:`/`fix:` prefix), author or state; breaking PRs get a dashed red ring |
| `2` `Tab` | open the code viewer: a floating window with the diff and the review's comments on the lines they are about. Drag its header to move it, its corner to resize it, double-click the header to maximize |
| `j` `k` `D` `c` `Esc` | in the code viewer (after clicking into it): next/prev file, marks-only vs full diff, how much context, close. Click the board and its keys come back, with the viewer following the selected PR |
| `Z` | dream: Claude tidies all memory files (merge, dedupe, drop stale), you approve before anything is written |
| `L` | point the local memory directory somewhere else, or give a git repo to clone as your memory |
| `C` | point the whole team store (`~/.prs_teams`, every team) somewhere else — only while no team is joined |
| `T` | teams: `1-8` open one, `n` start one, `a` join one. Inside a team: `e` propose a change to its brief (a pull request on the team's repo), `d` describe it, `c` connect a remote, `o` cover an owner, `x` leave (see Team). The header shows `+N` beside a team that has sent facts since this dashboard started; opening this clears it |
| `u` | shown when a newer release exists — opens the update panel |
| `S` | collapse the left rail to 106px — each group keeps a digest of its own settings, and the status counts keep their dots and numbers |
| `[` `]` | previous / next queue tab |
| `?` | the shortcut window: every key, grouped. It floats over the board — drag its header, resize its corner, leave it open while you try them. Its **show key hints** switch, also in the rail's View group, prints each key on the button or settings row it belongs to; on by default |
| `Esc` | the menu: theme, notifications, refresh, quit |
| `q` | quit |

`m` `d` `e` `s` `t` `i` open a picker: `j`/`k` moves, `Enter` picks, `Esc` keeps. `x` and `h` open a checklist
that stays open while you toggle. The left rail holds the same settings as dropdowns and chips, in three
groups — Agent, View, Knowledge — so nothing needs a key. A shut group still says what it is set to, and
`S` shuts the whole rail to a column of digests that names every setting and its value.

## Installing

Reviews remember with **no setup at all** — `~/.prs_memory` appears on first use, and every review reads it
back. That half needs nothing.

What needs installing is the other half: making a regular coding session read the same knowledge.

| | what it does |
|---|---|
| `gitdashy install` | once per machine — every session reads the cross-repo facts (it explains itself and asks first) |
| `gitdashy init --into DIR --loader FILE` | once per repo — sessions there also read that repo's facts |
| `gitdashy setup` | asks who you are and what the work is for; `install --full` asks only the first, because a brief belongs to a repo and not to a machine |
| `gitdashy bind [owner/name]` | which team a repo belongs to, and so which brief its reviews read; `--list`, `--forget` |
| `gitdashy auto [owner/name]` | which repos auto mode reviews; `--owner OWNER`, `--off`, `--list`. Bare, it reports |
| `gitdashy inline [owner/name]` | which repos get findings as comments on the lines they name; `--owner OWNER`, `--off`, `--clear`, `--list`. Bare, it reports |
| `gitdashy drafts` | what a review proposed and no second review has confirmed yet; `--count` is one line for a session hook |
| `gitdashy teams [--new NAME] [--join URL] [--connect URL] [--leave KEY]` | start, join, connect or leave a team; bare, it lists what each covers |
| `gitdashy remember "..."` | already on `PATH`; a session files what it worked out |
| `gitdashy friction` | did this session hit something worth remembering? how the `Stop` hook asks; `--interrupts N --denials N` for any agent |
| `gitdashy install --full` | the whole thing — an agent corpus in every session too, from [`corpus/`](corpus/) or your own |

Reviews are unaffected by all of it: they run `--safe-mode` and read memory through the prompt.
[`docs/install.md`](docs/install.md) is the full account — every file it writes, why a symlink and not a
copy, and how to undo it.

## The review

`r` on a review-requested PR runs `claude` headless against `<repo>#<number>`, then posts the
result as an **approve**, **request changes**, or **comment**. Reviews are
appended to `~/.prs_reviewed.jsonl` (one JSON object per line) and show up in the REVIEWED section,
where the pane shows the summary and `v` the full review. A PR whose head commit moved since the verdict
is flagged `↻ re-review · was <verdict>` and can be reviewed again — a new comment alone does not count.

`--depth LEVEL` sets how hard the reviewer looks: `low` skims for obvious defects, `medium` reads the
whole diff, `high` also reads the surrounding code and traces callers, and `adaptive` (the default)
lets Claude pick from the size and risk of the diff. `--effort LEVEL` is passed straight to
`claude --effort` (low to max) and controls how much thinking the model spends. `d` and `e` pick
them at runtime; the header's `reviewer` group shows them as `depth <depth>` and `effort <effort>`.

`--voice A,B` (or `PRS_VOICE`) picks how the posted body is phrased. `review` is the normal review;
`caveman` and `bot` restate the verdict in caveman speech and as a terse machine log. Any mix, at
least one; untick `review` and the voices you left ticked are the whole review. `--hunter A,B` (or
`PRS_HUNTER`) adds lenses, each appending a section of its own findings: `ponytail` hunts only
over-engineering, `security` only security, `tests` only missing or toothless tests,
`perf` only runtime cost the change adds (work repeated per poll, render or request, N+1, growth with no bound),
`humanizer` only AI-sounding prose the change adds to the application: user-facing strings, docs
and comments, never the PR description, and `spaghetti` (the Spaghetti monster) only what someone who
did not write it cannot maintain: swallowed errors, hidden state, silenced checks, a frontend that decides.
With `spaghetti` on, the review lists each rulebook fail and warning, and gitdashy scores the PR
(`100 − 50·fails/KLOC − 10·warnings/KLOC`, KLOC being the added lines but never less than 1) into a grade
A–D, shown as a tag on the row and under SCORES in the pane. Any fail turns an approval into request changes. The whole-repo audit is the
`spaghetti-audit` spell; its writing counterpart, the `fog-audit` spell, audits the prose a PR adds.
Both spells carry their whole rulebook (`src-tauri/src/rulebooks/`); the hunter keeps to its short list.
A new starter spell reaches existing installs too, once; a starter you delete stays deleted (except,
once, a `spaghetti-audit` or `fog-audit` deleted on an install first made on 2.36.0–2.37.0). An existing
spell is never overwritten, so an older `spaghetti-audit.md` keeps no rulebook until you replace it.
The hello comment names both so the author knows why the review reads that way. `x` and `h` tick
them at runtime.

`--instructions FILE` (or `PRS_INSTRUCTIONS`) appends your own text file to the prompt — house
rules, things to always check, what to ignore. It is read fresh for every review, so you can edit it
while the dashboard is running. A missing file shows as `error:` on the row instead of reviewing
without it.

`--inline` (or `PRS_INLINE=1`) also posts each finding as a comment on the line it is about, beside
the review. It is off by default because it posts on someone else's PR: inline
comments open resolvable threads, so on a repo that requires conversation resolution a `nit` holds
the merge button until someone resolves it. The body is unchanged; each finding appears in it and on
its line.

Only a finding that names a line the diff actually carries is posted; one about a file the diff does
not touch, or a line outside the hunks, stays in the body where it already was. The comments are
anchored when the review runs and pinned to the commit it read, so releasing a held review a week
later still puts them where that diff had them. A push while the review is running means the diff no
longer matches that commit, and the review then posts without them rather than guessing.

If GitHub rejects them, nothing is posted; the row shows the error and the verdict is held for `Y`,
without the comments, so releasing it is not the same rejected request a second time.

**It is set per repo, not per machine.** `gitdashy inline owner/name` turns it on for one repo,
`--off` turns it off, `--owner OWNER` covers a whole org and `--clear` drops a rule so the target
follows again. A repo rule beats an owner rule beats the `--inline` switch, which is what anything
unnamed falls back to. The rail's **when a review finishes** panel sets the same rules, with the
switch itself above them — the one setting here that writes to someone else's PR, and now one you
can turn off without leaving the app.

For one review rather than all of them, `R` asks for instructions before it starts. They go into the
system prompt, beside the reviewer's lens and apart from the pull request, which is written by someone
else and cannot override them. They are private: kept in the held review on this machine, and never in the opening comment or the review
log a team pulls. The reviewer is told not to quote or mention them, and a review that repeats a line of them
word for word is held instead of posted, so you read it before its author does.

Reviews run with `--safe-mode`, so the reviewer sees no `CLAUDE.md`, skills, hooks or MCP servers from
your machine — only the prompt, one read-only command, and a short built-in review lens: state
ownership, observability, blast radius, timing, and the seams between systems. Without it a review would
inherit whatever instruction files sit in the directory gitdashy was launched from, so the same PR could
be reviewed differently depending on where you started the dashboard. Your own house rules are unaffected
— they go through `--instructions`, which is read per review. The memory cleanup behind `Z` is scoped the
same way.

Every REVIEWED row carries a small `depth/effort $cost time` tag showing what the review ran with, what claude
said it cost, and how long it took.

### Memory

Reviews remember, but not immediately. Each review may return up to three durable facts about the repo
(conventions, recurring pitfalls, intentional oddities). A fact is not a fact because a model wrote it —
it is a fact because it **recurred**, so the first review to mention something only writes a *draft*:

```
~/.prs_memory/drafts/<owner>__<repo>.md
  - (1) CI reports "skipping" for format-check
         ↑ how many independent reviews landed on this
```

A second review arriving at the same thing (matched loosely, so rewording still counts) promotes it into
`~/.prs_memory/<owner>__<repo>.md`, where later reviews read it. That promotion is automatic: being wrong
in your own memory costs only you, and you will meet the line again.

**A repo bound to a team drafts into the team instead.** When the team has agreed to receive drafts, a
review's draft goes to your folder of the team's draft pool, and a second independent sighting, by you or
a teammate, moves it into the team's knowledge (see *The team's draft pool* below). Nothing about that
repo is drafted into your private memory unless you ask for it with `gitdashy remember --private`.

**Drafts are never fed back into a prompt.** If they were, a reviewer would meet its own earlier guess as
evidence and agree with itself, and the count would measure repetition instead of durability. The signal
is rediscovery, so the reviewer has to arrive at it again blind.

`~/.prs_memory/general.md` goes into every review regardless of repo. All of these are plain markdown
bullet lists — `n` inspects the selected PR's repo facts and `g` opens inspect on the general ones. Nothing is
typed in there: a fact arrives when reviews find it, and `x` removes one.
`Z` dreams: Claude reads every memory file — yours and the team's, each labelled — merges duplicates, drops
stale or contradictory lines and moves repo-independent facts to that source's general file. It is told never
to move a line from yours into the team's; sharing is your call, not its. It shows a summary and per-file line
counts; `v` opens the full summary and diff. Nothing is
written until you press `y`.

### Team

**A team is a git repo — or just a directory — that pools what reviews learn.** Whoever can reach it is on
the team. There is no service and no account, and nothing here assumes GitHub: `git clone` takes any URL, and
a bare `owner/name` is expanded to a GitHub URL as a convenience and nothing more.

Press `T`. It lists your teams, and that list does three things: `1-8` opens one, `n` starts one, `a` joins
one. Opening a team shows what it has before offering anything:

```
╭── NeoMedSys  (neomedsys) ───────────────────────────────────────────────╮
│  what it is for                                        The NMS project  │
│  checkout                                       ~/.prs_teams/neomedsys  │
│  git remote                                                   none yet  │
│  used for                                       neomedsys/* · 12 repos  │
│                                                                         │
│  [e] brief  [d] describe  [c] remote  [o] cover  [x] leave  [esc] back  │
╰─────────────────────────────────────────────────────────────────────────╯
```

**used for** is the whole of it: the repos whose reviews read this team's brief and facts. `o` adds an
owner or a repo to it, here and in the team, so everyone who joins later starts with the same list.

You are never asked to compare that list against anything. When a colleague adds coverage after you
joined, gitdashy asks once, when you open the team:

```
 NeoMedSys now covers acme/* — use it here too? [y/n]
```

Either answer settles it and it is not asked again — *no* writes the same tombstone an unbind does. The
question exists because someone else's push must not silently reroute your reviews; it is a question
rather than a second list on screen because a list you have to diff against another is not readable.

`n` starts a team with a name and a line about it, **no remote and nothing hosted anywhere**, under
`~/.prs_teams`; started from a PR row it offers to cover that row's owner straight away. `a` joins one that
exists from any git URL, `owner/name`, or a bare repo on disk. The same from a shell:

```sh
gitdashy teams --new "NeoMedSys Platform" --desc "Precision-medicine platform."
gitdashy teams --team neomedsys-platform --connect git@somewhere:us/mem.git
gitdashy teams --join git@somewhere:us/mem.git
```

Checkouts live in `~/.prs_teams/<key>/` (`PRS_TEAMS` overrides), one per team. **The key is the name you
gave it, fixed at creation; the location is separate and changeable** — an origin-derived key does not exist
until a team is hosted, so bindings would have gone dead at exactly the moment a team got a URL. Its name and
description live in `team.json` inside it, so everyone who clones it sees the same ones. `--at DIR` (or the
*Where?* prompt) links the key to an **empty** directory, or one that does not exist yet. A directory that
already holds something is refused: a team's checkout is a place for the team's files, and the alternative was
adopting whatever was there — once, the parent of every project on the machine, committed as thirty gitlinks.

**You can be in several at once.** Which team applies to a repo is `gitdashy bind` (or `b` on any row), and
that decides everything: the brief its reviews read, the facts they see, and whether a fact about it may be
shared. A repo bound to nothing is private.

Each team keeps its **own review log**. A review is appended to the log of the team its repo is bound to; an
unbound repo's goes to yours, and the REVIEWED section merges them all newest-first — so a teammate's review
still appears, per team, and only for repos that team owns. Files are appended only and merge with git's
union driver, so two people reviewing at once do not conflict. **Joining does not copy your own log in**: it
holds reviews of other teams' repos and of private work.

**Your memory does not move there, and joining does not publish it.** Team memory is a separate, second
source that reviews read *alongside* yours. What reaches it is what has crossed the gate — two reviews
that did not know about each other, and a model that read both and called them the same claim — for a
repo **bound to that team**. Binding is the decision; nothing else is a keypress.

Your unconfirmed observations are pooled too, so a colleague's reviewer can be the second observation
your own machine rarely produces:

```
<team>/memory/drafts/<user>/<repo>.md    what their reviews proposed   (never read into a prompt)
<team>/memory/pool/<user>/<repo>.md      what they have accepted        (never read into a prompt)
```

Neither is read by a review, a session, the mirror or the dream — they are evidence, and the only thing
they decide is whether two people saw the same thing. Two people's pools agreeing is four independent
reviews across two humans, and inspect says so beside a team's fact with `★ 2 people found this`.

**Inspect is how you watch it and take things out.** `x` on one of your facts removes it; on a team's fact
it proposes removing it, as a pull request. Facts only arrive through reviews.

A repo bound to nothing publishes nothing — no facts, no drafts, no evidence — so a side project stays
private however many teams you are in.

If your own memory directory is itself a git repo, gitdashy pushes it too — so your facts and drafts follow
you between machines without ever passing through the team. The header's `Knowledge` group shows
`team org/review-team`, or the last git error in red.

`a` (or `--join`) takes `owner/name` (cloned over https with your token), a **git URL** — `https://…` and
`git@…` both clone with plain `git` — or a **local path**, which must be a *bare* repo, because git refuses
pushes into a checkout. So a team on a shared drive is `git init --bare /srv/shared/mem.git`, then `--connect`
from the machine that started it and `--join` from everyone else. Joining a repo that is already one of your
teams is refused whatever you call it, and a repo with no `team.json` gets one on the first join, so the next
person lands on the same key. `--connect` refuses a remote that already holds history that is not the team's
— that is a team to join — and accepts one that holds the team's own, so moving hosts still works. Remote
prompts are disabled and every clone, fetch and pull is bounded, so a repo your credentials cannot reach fails
with an error on the header instead of hanging the dashboard on an invisible password prompt; a pull that
cannot rebase is aborted rather than left in progress. `x` leaves a team, and refuses while the checkout
still holds work it has not pushed.

The whole model — every store, promotion rule and discard rule — is written up in
[`docs/memory.md`](docs/memory.md).

### The team's draft pool

The team's knowledge is two kinds of file in its repo: `memory/general.md`, true of every repo the team
covers, and `memory/<owner>__<repo>.md`, true of one. Reviews of a bound repo read both, beside your own.

What is not knowledge yet waits in the draft pool, one folder per person:

```
<team>/memory/drafts/pontus/acme__api.md    - (1) [r:7a2c] retry owns backoff
<team>/memory/drafts/martin/acme__api.md    - (1) [r:91cf] retry owns backoff
                                                  ↓ two independent reviews: 2/2
<team>/memory/acme__api.md                  - retry owns backoff
```

The folders are checked against each other on every refresh and after every review. Word overlap only
finds candidates; every candidate pair, however alike the wording, is put to the model, which answers only
*same claim* or *not*: two near-identical sentences can say opposite things. A match counts **distinct review ids**, so two reviews by one person count as much as one each by
two people, and a single review can never confirm itself. At least one of them has to be a review or a
teammate's: one session calling `gitdashy remember` twice stays a draft. On a match the fact moves into the team's file
and out of the drafts: your machine takes out your line, and your teammate's takes out theirs the next time
it sees the team already knows it.

Drafts are never read into a prompt, yours or anyone's. `W` lists yours in each team beside your private
ones: `x` drops one, and `t` proposes it as a team fact by pull request, since accepting something into
what every teammate's reviews read is a hand edit. Only recurrence moves a draft there by itself.

### What the team is building

A team writes three kinds of document and learns a fourth. They are not interchangeable, and putting one
in another's place costs every review that reads it:

| document | says | one per | written by | read by |
|---|---|---|---|---|
| **brief** `memory/project.md` | why the team builds what it builds: the project, why it matters, the constraints that make a change wrong, how the codebase is shaped | team | the team, by pull request | every review of every repo bound to the team |
| **about** `memory/about/<owner>__<repo>.md` | what one repo is: its role, what it owns and must not do, its seams | repo | the team, by pull request | reviews of that repo, right after the brief |
| **agents.md** | how agent sessions work in the team's repos | team | the team, by pull request | coding sessions only, never reviews |
| **facts** `general.md`, `<owner>__<repo>.md` | what reviews found and found again | general, or repo | nobody: reviews learn them | reviews and sessions |

A feature list or a tour of the product belongs in none of them: it goes stale with every release and
tells a reviewer nothing about whether a change is wrong. When one of the three is edited (inspect, `e`),
what it is for sits beside the text, a model can ask what it needs and propose a revision (`F2`, taken
with `F3`), and a brief without a brief's sections gets a word before the pull request opens.

`project.md` says what is being built, for whom, and under what constraints. It goes into a review ahead
of the learned facts, so a reviewer knows what the code is *for* before judging whether a change serves
it. It is declared, not learned — the promotion pipeline never touches it, the dream never rewrites it,
and it is never offered for sharing.

**`agents.md` is declared too, and it reaches agent sessions rather than reviews.** It reaches every *session*
in every repo bound to the team, through that repo's mirror, and never reaches a review: it says how to
work here, which is not something a reviewer should be told about the code it is judging. Starting or
joining a team seeds one; a team that already exists gets it by adding `memory/agents.md` to its
checkout. What belongs in it is whatever the team needs its members' sessions to do — above all,
`gitdashy remember`, since a session that files nothing leaves every draft at one observation.

**The dashboard shows it and asks before any session reads it**, and asks again if the wording changes;
until then it stays out of the mirror and nothing else changes. The reason is that this file is
imperative text handed to an agent that holds tools, where facts and a brief are evidence a reader
weighs, and whoever can push to the team's repo writes it. This gate covers `agents.md` only:
`project.md` and the team's `general.md` come out of the same repo and reach the same mirror ungated,
and the brief reaches review prompts too. The Knowledge row says when a team's `agents.md` is being
withheld, whether because nobody has read it yet or because somebody said no; `gitdashy teams
--agents-again` forgets that answer so the next launch asks once more.

**A repo belongs to a team, and that decides everything the team knows about it.**
`gitdashy bind <owner/name>` — or `--owner neomedsys` for a whole org in one line — sets which brief its
reviews are told, which facts they read, and whether a fact about it may be shared. **A repo bound to
nothing is private:** your own memory and nothing else, so a side project is never told how somebody
else's team conducts reviews. An exact binding or `--forget` overrides an owner rule, so the one repo in
the org that is not the project can be left out. When a section holds more than one team, the list
separates them under a header for each.
Joining a team binds the repos already named in its shared review log — once, so nothing you had
yesterday disappears — and after that it is yours to change; `--forget` sticks. `gitdashy bind --list`
shows every binding, and the detail pane names the brief a review of the selected PR will get.

**What a team covers travels with it.** `gitdashy teams --team KEY --cover neomedsys` (an owner, or
`acme/api` for one repo) records the claim in the team's `team.json` and binds it here. Someone **joining**
the team gets what it declares seeded into their own bindings once, exactly as the log is — it shows in
`bind --list`, and a `--forget` still sticks. A claim added *after* they joined is not bound on their
machine on its own: it is listed by `gitdashy teams`, and taken with `bind --owner`. What a repo's reviews
read, and where its facts may be pooled, is not something a push to the team repo gets to decide for
someone else. `--uncover` withdraws a claim, and rows it already seeded stay each person's to change.
A claim two joined teams both make is left alone rather than guessed at. `gitdashy teams` lists what each
declares. `gitdashy setup` writes the brief of the team the repo you run it in is bound to, and says so —
yours when it is bound to none.

Selection is **declared**, deliberately. The alternative was to infer it from the shared review log, the
way memory visibility is decided. That is wrong for a brief: a log entry is a side effect of reviewing
one PR, nothing ever removes one, two teams can both name a repo with no way to prefer either, and
nothing on screen says which brief you are about to get. Before this, every review of every repo was
given *both* briefs concatenated — two statements of what the work is for, in one prompt.

That split keeps two things apart: `project.md` is what the team is doing, and a corpus's `USER.md` is
who *you* are. Nobody should have to restate the project in their own file.

### Changes to a team's memory are pull requests

A team's repo is what every teammate's reviews and sessions read, so nothing in the app writes to it by hand.
Removing a fact from a team's file (`x` in inspect, or forgetting a shared fact nobody else backs) and changing
a team's brief or `agents.md` each push a `gitdashy/propose-…` branch and open a pull request on the team's
repo (or, when the team's repo is not on GitHub, push the branch and say so). A person with rights on that repo
approves it. **gitdashy never reviews a pull request on a joined team's repo**: auto skips it, `r`, `R` and `p`
are refused, and the row says "human review only". That is decided by the repo, not the branch name, so a pull
request opened outside the app is covered too.

**A team's repo holds its memory and nothing else.** Because no pull request on it is reviewed by a model, a
team kept inside a code repo would lose review of all that code; joining one says so. **A team with no
remote** is this machine's alone, so a change to its brief, `agents.md` or a fact is written and committed
directly: there is nobody to approve it. Editing memory text by hand through the app or `POST /api/memory`
is gone; reviews and `gitdashy remember` add facts, and the app only removes them.

### Where knowledge lives

The Knowledge group in the rail says where memory is actually read and written right now: `Memory` is the
solo directory when you are on your own and the team's when you are in a team, `Team` is the repo or `off`, and
`Store` appears only once the checkout sits somewhere other than its default.

`knowledge panel` in the same group (or `K`) opens the panel on its **stats** tab, a chart of how fast that memory grows. Nothing in a fact records when it was
learned, so the chart reads it from git: each joined team's history, and your memory's history up to the day
`~/.prs_learning.jsonl` began. From then on that file carries your side, one line per draft or fact gained,
holding its time, repo and how it came — never its text. A fact from the history shows as **earlier**, because git says when
a line appeared but not why. Team facts are credited to their git author and arrivals to the pool they landed in,
so one person can appear under two names.

`L` also takes a **git repo** — `owner/name`, a path, or a `git@`/`https://` URL. gitdashy clones it and
makes it your memory directory, moving the facts already there into it (and refusing, rather than choosing,
if a file exists on both sides). From then on your memory is a checkout that gitdashy pushes, so your facts
and drafts follow you between machines without ever passing through the team.

`L` and `C` point the memory directory and the team store — `~/.prs_teams`, which holds every joined team — somewhere else. There is no config file — the old
location becomes a symlink to the new one and whatever was there moves across, so the setting survives a restart
the same way team mode does, by being a fact about the filesystem. Nothing is overwritten: if both sides hold a
file of the same name, the move stops and says so. `PRS_MEMORY` and `PRS_TEAMS` still win when they are set, and
the keys say so rather than pretending to work. A target inside a git repo that does not ignore it asks first,
since memory is usually not yours alone to commit.

Auto mode (`a` or `--auto`) does the same thing unattended for every review request that appears
*after* you turn it on. `--auto` also reviews what is already listed; `a` asks whether to include
the ones on screen or leave them as the baseline. Auto only reviews PRs opened by the repo's owner,
an org member or a collaborator: a stranger's fork PR is a diff written to steer a model that may
post under your name, so auto leaves it for you to review with `r`.

Whether a finished review posts is a separate setting. Running a review is the expensive part;
posting it is the part you cannot take back. The rail's **Agent** group lists every owner on the board,
and a switch inside each one picks one of two modes:

- **One setting for every repo under the owner.** Reviews you start and reviews auto starts each post,
  as always, or hold — and that applies to all of the owner's repos. Shut, the row says so and says what
  they are: `all repos: you hold | auto posts`.
- **Each repo set on its own.** Shut, the row says `per repo · 3`; open it for each repo's two settings.

Turning the switch on can make a repo hold that posted, never the reverse: a kind of review is held for
the whole owner if the owner or any repo under it held it, and every repo rule under that owner comes
off, since one left behind would beat the owner. Turning it off changes nothing: each repo on the board
keeps the words it had, and the owner's rule stays as the fallback for a repo with no PR on the board. A held review
is written whole to `~/.prs_held`, the row says `waiting to post`, and `Y` reads it and either posts
it or drops it.

`Y` is also where you discuss it. A review run through the `claude` CLI is resumed in its own session,
so the agent still has everything it read and can use its tools again: ask why a finding is blocking,
or show it where it was wrong. Nothing said there is posted. When the conversation has changed its
mind, **revise the review** asks it to write the verdict again; the revision waits beside the original
and replaces it only if you accept it, and posting is refused while one is waiting, so the verdict you
read is still the verdict that goes up. Reviewing a held PR again (`r`) replaces the held review,
conversation included. It is a new review in a new session. A review that ran on `openrouter:` or `local:` cannot be
discussed yet: there is no session to go back to.

A pre-review is discussed the same way, from `p`. The conversation is kept beside it, in a
`.talk.json` next to its `.md` under `~/.prs_reviews`, never in the markdown itself, since that file is
read as it is. An accepted revision rewrites the markdown but keeps the file's time, so a push made
after the pre-review still shows as one. Running the pre-review again starts a new conversation.
No opening comment is posted for a held review either, so a PR never says it is being reviewed by
something that may never arrive.

The board spans whatever the token can see, so with nothing armed auto covers every repo on it.
`gitdashy auto owner/name` arms one, and from then on auto covers only what is armed; `--owner
OWNER` arms a whole org and a repo of its own still overrides it, so the one repo under that owner
you do not want reviewed can be left out with `gitdashy auto owner/repo --off`. A lone `--off` is
not a decision to arm the rest: while nothing is armed, auto still covers everything. `gitdashy
auto` reports where it applies, and what is set to wait. Widening the scope does not fire a batch, whether you widened it from the dashboard,
from the command line or by editing the file: the tick that notices a repo has come into scope
treats what it already has listed as seen, and only what arrives next starts.

### Agent sessions

What the reviews learn is worth having open in the editor too. `gitdashy sync-memory --into PATH`
copies the memory into a repo as a read-only mirror, `repo.md`, for an agent session working there to
read. It holds that repo's facts from every source bound to it — and, above them, the one brief a review
of that repo would get and the bound team's general facts, so a session and a review of one repo are
told the same things by the same team.
Your own general facts are not in it: they load live, everywhere, through `gitdashy install`. (`--general`
mirrors them too, for an agent that has not wired that route.)

```sh
cd ~/src/my-repo
gitdashy sync-memory --into .agent/team    # --repo defaults to this directory's origin
```

You normally never do this by hand — `install --full`'s hook does it in every repo you open. The command
exists for wiring an agent that is not Claude Code, and looks like this:

```sh
cd ~/src/my-repo
gitdashy init --into .agent/team --loader CLAUDE.local.md
```

which excludes the mirror from git (through `.git/info/exclude` — never the tracked `.gitignore`, which is
the team's), adds the import to whichever instruction file you name, writes the mirror, and registers the
path. The running dashboard then re-mirrors it on every refresh, so there are no hooks to install and
nothing on a session-start timeout budget.

**A mirror says how old it is.** Its header names, per team, when that team was last reached — and when
nothing is keeping it current, says so in the file the session is reading rather than in a hook message
that scrolls past:

```
> team org/review-team: last pulled 4 days ago — **this may be behind what the team has.** Nothing is
> refreshing it: start `gitdashy`, or run `gitdashy sync-memory --into` this directory.
```

A pull that fetched and then failed to rebase leaves a fresh timestamp over a checkout that did not
move, so a live error outranks the age and the line reads `— **the last sync did not land:** …`.

The session hook also starts one `sync-memory` in the background, detached, so the next read is current
on a machine where the dashboard is rarely open. A background sync does not pull while a dashboard is
running, while another sync holds the lock, or when every team was reached in the last few minutes. Six
repos opened in an editor is six session starts, and one fetch answers all of them. The dashboard's own
refresh takes the same lock, so a refresh and a background sync never rebase one checkout against each
other. A push that has to merge first still pulls outside it. Either way the report says why it did not
pull, rather than looking like a fetch that found nothing.

**Cross-repo facts take a different route, and a better one.** A *user-level* `CLAUDE.md` import follows a
symlink out of its own tree, where a project-level one refuses to — so one command wires it:

```sh
gitdashy install            # explains itself and asks; --dry-run to look, --uninstall to reverse
```

That symlinks your memory (and the team's) into the agent config directory and adds three imports, putting
every general fact into every session, everywhere, live — nothing to sync, nothing to expire, and it starts
carrying the team's the moment you have one. It is idempotent, it never replaces anything it did not
create, and `--uninstall` removes exactly what it wrote.

Which is why `sync-memory` mirrors the **repo** file only: general facts arriving by both routes would sit
in context twice. `--general` mirrors them in as well, for anyone not wiring the global route. And reviews
are untouched by either, because `--safe-mode` drops `CLAUDE.md` — memory reaches a review through the
prompt and never twice.

### Feeding memory from the other side

A coding session that works something out can file it where reviews file theirs:

```sh
gitdashy remember "the viewer owns mask state, the store only mirrors it"
gitdashy remember --general "PHI reaches the frontend; treat it as such"
```

It becomes a draft, not a fact — the same gate a review's claim passes. `--repo` defaults to the current
directory's origin. So a fact that a review proposed once and a session independently arrived at is
confirmed by their agreement, and neither surface can confirm itself, since drafts are never read back.

In a repo bound to a team that takes drafts, it goes where a review's would: your folder of the team's
draft pool, so a teammate's review can match it, and once confirmed it is the team's. `--private` keeps
it in your own drafts instead, and once confirmed it is yours alone:

```sh
gitdashy remember --private "I keep forgetting the migration flag"
```

A repo bound to no team keeps its name to itself; the fact becomes yours. `--general` there stays with that
repo too. Your own `general.md` is for how you work, and it is read in every session: it takes what you file
with no repo at all, and `--private --general` from anywhere. Re-run it whenever you
want a fresh copy — a session-start hook is a good home for it, with `--no-pull` so a slow network
cannot blow the hook's timeout. That mirrors whatever the last dashboard refresh pulled; the shipped hook
then starts a pulling one in the background, so the file is current by the next read even when no
dashboard has been open for days.

It is a copy, not a link: Claude Code confines instruction-file imports to the project tree, so `~`,
absolute and symlinked paths are all refused. The mirrors carry a header saying they are read-only,
naming their source and when they were taken — remove a fact with `n` / `g`, never by editing the mirror,
or the two disagree.

Memory is often team-private, so `sync-memory` refuses to write anywhere git would commit it. Ignore
the target path first (`.git/info/exclude` keeps the rule out of the tracked `.gitignore`).

## Following people

**+ follow** in the footer, or `F`, fuzzy-finds anyone who authored or reviewed a PR on the board (or takes
any GitHub username you type) and adds a pill for them beside the button. Click the pill for a pop-up with a
few bullets on what they have worked on over the last day, written from the PRs they opened or updated, and
the list of those PRs. `⟳` in the pop-up asks again; `×` on the pill unfollows. The rail's **view** group
also lists every team and org the board is showing; clicking one follows everyone it has a PR from.

Pills check on the board's refresh interval (not while the window is hidden). Each check is one GitHub
search; the model only runs when that person's PRs changed since the last story. When a check finds PRs
that are new or were pushed since the last one, the pill lights up and those PRs, and only those, pop up
above it.

A pill also marks itself when someone changes direction, and has its own mark, separate from the one for
new pushes. Every rewritten story is compared against the one before it, in the same
model call, and the bar is a different problem, even one taken up alongside the old work. Picking up an auth rewrite
next to the export job is a change of direction; moving from export pagination to export retries is more of
the same export work, and says nothing. Only the first kind marks the pill, and the pop-up opens with one
line saying what is new. Only opening the pop-up clears the mark: a mark raised while you
were away survives later rewrites until you have seen it.

Stories run on Haiku through the `claude` CLI, whatever model reviews use, unless reviews go through
`openrouter:` or `local:`, in which case the stories use that model too. They always run at low effort.
Following is meant for a handful of people: every pill is one search per refresh, so fifty (the cap)
on a short interval is a lot of GitHub traffic. Who you follow and the last story per person are kept in
`~/.prs_stories.json`, beside the settings file (`--demo` writes nothing).

## Versioning & self-update

The version lives in one place, `version` in `src-tauri/Cargo.toml`, and shows in the header badge and via
`gitdashy --version`. Releases are tagged `vX.Y.Z` and CI attaches one binary per OS to each.

Each refresh also lists the release tags on GitHub (`git ls-remote`, so no API rate limit). If a tag is
numerically newer than the running version, the header shows `↑ v2.1.0 · u`; pressing `u` confirms,
downloads that release's binary over the running one, and re-execs it with the same arguments. No
network: the badge just never appears.

## Other models

Reviews run through the `claude` CLI by default. Name a model `openrouter:x-ai/grok-4` or
`local:qwen3-coder` and it goes to that provider's OpenAI-compatible endpoint instead, as one chat
completion: those backends get no tool loop and are told so. Every backend, Claude included, gets the PR
and its diff pasted into the prompt (a diff over 200k characters is cut) — there is no `gh` to fetch it
with any more. Claude alone can read further: it is allowed exactly one command, `gitdashy api <path>`, a
read-only GET against the GitHub API that returns files decoded, which is how a deep review reaches the
code around the diff. That command is confined to the repo being reviewed — a path outside it is
refused, and a code search is narrowed to that repo (one naming another repo, user or org is refused
outright rather than narrowed). The confinement is set on the
review's own subprocess rather than written into its prompt, because the prompt is the part an untrusted
diff gets to influence.

A review of a PR opened by the repo's owner, an org member or a collaborator may also read the other
repos bound to the same team, so a change that depends on a shared library can be checked against it.
Both halves have to hold: `gitdashy bind` is where a person declares which repos belong together, and
GitHub's `author_association` is what says the diff was not written by a stranger. A fork PR from an
outsider gets the repo under review and nothing else. Code search stays on that repo either way.

That association means the author has standing in the repo being reviewed — not that they could read
the siblings themselves. A read-only collaborator, or an org member with no access to a private
sibling, still counts as trusted here, and the review body is published on their PR. So the set you
bind to a team is the set you are willing to have summarised there; keep the ones that are not out of
it, which is what `gitdashy bind --forget` is for. `--effort` carries over to OpenRouter as the
model's reasoning budget (`xhigh` and `max` collapse onto `high`, since OpenRouter stops there): leave
it at `low` unless a reasoning model is worth the wait. `gitdashy self-check` on another backend only
proves the endpoint answers.

```sh
PRS_MODELS=openrouter:x-ai/grok-4,local:qwen3-coder OPENROUTER_API_KEY=sk-... gitdashy
```

Then `m` cycles them like any other model.

## Environment

| var | default | what |
|-----|---------|------|
| `PRS_MODEL` | `opus` | model used for reviews; a bare name is the claude CLI, `openrouter:<model>` or `local:<model>` uses that OpenAI-compatible API instead |
| `PRS_MODELS` | (none) | comma separated extra names the `m` picker cycles, e.g. `openrouter:x-ai/grok-4,local:qwen3-coder` |
| `OPENROUTER_API_KEY` | (none) | key for `openrouter:` models |
| `PRS_OPENROUTER_URL` | `https://openrouter.ai/api/v1` | OpenRouter base url |
| `PRS_LOCAL_URL` | `http://localhost:1234/v1` | base url for `local:` models: LM Studio, Ollama (`:11434/v1`), llama.cpp |
| `PRS_LOCAL_KEY` | (none) | key for `local:` models, sent only when set |
| `PRS_DEBUG` | unset | `1` = same as `--debug` |
| `PRS_DEBUG_LOG` | `~/.prs_debug.log` | where `--debug` writes |
| `PRS_LOG` | `~/.prs_reviewed.jsonl` | review log path |
| `PRS_EFFORT` | `medium` | `--effort` passed to claude: low, medium, high, xhigh, max |
| `PRS_DEPTH` | `adaptive` | review depth: low (skim), medium, high (very in-depth), adaptive (judged from the diff size) |
| `PRS_TEAMS` | `~/.prs_teams` | one checkout per team, directory name = the team key; a directory with a `.git` in it is a joined team |
| `PRS_TEAM` | `~/.prs_team` | the pre-plural single checkout, moved into `PRS_TEAMS` on first launch |
| `PRS_MEMORY` | `~/.prs_memory` | memory directory: `general.md` + one file per repo |
| `PRS_INSTRUCTIONS` | (none) | text file appended to every review prompt; `--instructions` overrides |
| `PRS_SETTINGS` | `~/.prs_settings.json` | where runtime picks (model, depth, theme, notify…) are saved; env vars and flags still win. The file records the effective state, so a value set by a flag is kept once any setting changes |
| `PRS_NOTIFY` | `1` | desktop popup when a PR asks for your review; `0` turns it off, or toggle it in the Esc menu |
| `PRS_INLINE` | `0` | `1` = same as `--inline`: findings are also posted on the lines they name, for any repo with no rule of its own (`gitdashy inline`) |
| `PRS_THEME` | `pencil` | colour theme: pencil, dashy, dracula, gruvbox, nord; Esc menu cycles it |

## Layout

```
src-tauri/
  Cargo.toml      the crate; its build embeds dist/, so `pnpm build` must run first
  tauri.conf.json the window: splashscreen.html, then the dashboard
  src/
    main.rs         entry point
    cli.rs          argv, --help, the subcommands, opens the window or serves the page
    shell.rs        the Tauri window: splash, then the dashboard
    web.rs          the localhost API: one JSON route per thing the dashboard can do
    state.rs        background refresh loop, shared state
    config.rs       tunables, paths and env overrides
    github.rs       everything that talks to the GitHub API (no gh)
    llm.rs          which model answers a prompt: the claude CLI, or an OpenAI-compatible API
    review.rs       runs Claude headless, posts the verdict
    log.rs          ~/.prs_reviewed.jsonl store + detail view
    memory.rs       ~/.prs_memory store, drafts, pools, and the dream cleanup
    team.rs         git-backed sync of the log and memory
    bind.rs         which team a repo belongs to
    autorev.rs      which repos auto mode reviews, and whether a verdict posts
    held.rs         ~/.prs_held: finished reviews waiting for a keypress
    mirror.rs       read-only copies of the memory, for agent sessions
    install.rs      wiring: CLAUDE.md imports, hooks, the corpus, setup briefs
    update.rs       release check and self-update
    demo.rs         canned PRs and a fake reviewer
  ui/               splashscreen.html and notify.png (the shell before the dashboard)
hooks/            the Claude Code hooks `gitdashy install --full` registers
corpus/           the agent corpus a full install copies to ~/.agent-corpus
src/              the React app: main.tsx, App.tsx, screens.tsx, modals.tsx, components/
index.html        the Vite entry
vite.config.ts    dev server and the /api proxy to the local server
```

`--demo` flips `config.demo`, and the few functions that reach the network (fetch, detail, the model)
answer from `demo.rs` instead. Unit tests live beside the code in each module.

## Develop the UI

The React app and the Rust server run as two processes, so the frontend hot-reloads while the backend
keeps running. Vite proxies `/api` to the backend on port 7777 (see `vite.config.ts`). You need Node +
pnpm and Rust stable.

```sh
# terminal 1 — backend on 7777. --demo serves canned PRs and a fake reviewer: no GitHub token,
# no Claude, no network. Pin the token to "dev" for --demo only; a real run should generate one.
cd src-tauri && GITDASHY_GUI_TOKEN=dev cargo run -- --no-open --demo --port 7777

# terminal 2 — frontend with hot reload on 1420
pnpm install
pnpm dev
```

Open `http://localhost:1420/?token=dev`. The port matters.

- Use `1420`. Port 7777 serves the last `pnpm build` embedded in the binary, so your source changes
  only show up through Vite. Hard-refresh if the page looks stale.
- Stop any other `gitdashy` first: the installed binary defaults to 7777 and `cargo run` then fails
  with "Address already in use".
- Drop `--demo` to run against real GitHub (needs a token, see Environment), and omit
  `GITDASHY_GUI_TOKEN` to have one generated — it is printed in terminal 1.
- The installed binary embeds `dist/`; `pnpm build && cargo install --path src-tauri` updates it.

## Tests

```sh
pnpm install && pnpm build   # web.rs embeds dist/, so the frontend must exist before cargo
pnpm test                    # vitest, over the pure functions in src/board.ts
cd src-tauri && cargo test
```

`cargo test --manifest-path src-tauri/Cargo.toml` from the repo root works too, and runs the same
way: `.cargo/config.toml` sits at the root rather than beside the crate, because cargo reads it from
the working directory and never from `--manifest-path`.

`pnpm test` covers what the board derives from server data: which sections a bucket shows, how the
older runs of a reviewed PR fold, and how the two draft rules compose. Anything that needs a browser
is not tested — there is no DOM harness, and the layout is still checked by looking at it.

Created by Martin Soria Røvang.
