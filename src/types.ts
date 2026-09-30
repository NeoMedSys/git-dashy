// The JSON shapes web.rs emits. Hand-written against payload(), detail() and code().

/** web.rs pre_json: null, or the pre-review's timestamp and whether the PR moved since. */
export type Pre = { at: number; moved: boolean } | null

/** One row of the board, as payload() builds it. The client adds uid/section/older via visible(). */
export type Pr = {
  url: string
  number: number
  title: string
  repo: string
  author: string
  updatedAt: string
  isDraft: boolean
  /** lines added/deleted from the board query; null when GitHub did not say. */
  add?: number | null
  del?: number | null
  status: string
  prev: string
  checks: string
  reviewers: string
  /** this session's review status string, "" when none is running or finished here. Not the log:
   *  a review from a past session leaves this empty. See `reviewed`. */
  review: string
  /** The review log has an entry for this PR, so the pane will have an AI REVIEW section. */
  reviewed?: boolean
  busy: boolean
  since?: number
  team: string
  summary: string
  reviewAt: string
  /** the newest review's tag: feature, fix, security...; "" when no review tagged it. */
  kind: string
  breaking: boolean
  /** Its newest review found schema changes or database risks. */
  db?: boolean
  /** Each scorer's grade from its newest review; empty when none was on. */
  scores: Score[]
  pre: Pre
  /** A finished review nobody has posted yet. */
  waiting?: boolean
  /** A team's memory repo: approved by a person with rights on it, never reviewed by a model. */
  humanOnly?: boolean
}

/** One scorer's grade: `name` is also its sprite. `blocks` means it forbids merging. */
export type Score = { name: string; score: number; grade: 'A' | 'B' | 'C' | 'D'; note: string; blocks: boolean }

export type Section = { name: string; prs: Pr[]; error: string }

/** A PR with the client-side fields visible()/group() add. */
export type Row = Pr & { uid: string; section: string; older: Row[] }

export type Team = { key: string; name: string; arrived: number }

export type Options = {
  model: string[]
  depth: string[]
  effort: string[]
  voice: string[]
  hunter: string[]
  subs: string[]
  window: (number | null)[]
  interval: number[]
  theme: string[]
  /** "team:<key>" and "org:<owner>" the TEAM section can search. */
  scopes: string[]
}

export type Knowledge = {
  /** The Friday report's background job, and the newest report on disk by date ("2026-09-15"). */
  report?: { job: { running: boolean; elapsed?: number; error?: string }; latest: string | null }
  memory: string
  store: string
  teams: Team[]
  teamError: string
  notes: string[]
  /** Team answers still holding something back. A row you can press, unlike a note. */
  waiting?: Waiting[]
}

export type Waiting = { kind: string; key: string; what: string }

/** One message in a discussion of a held review. */
export type Turn = { who: 'you' | 'agent' | 'error'; text: string; at: number }

/** The conversation about a saved review: the same shape for a held review and a pre-review. */
export type Talk = {
  /** What the person who ran it typed for it; private, never posted. */
  instructions: string
  thread: Turn[]
  /** A revision waiting for a yes or a no. The review stays what it was until it is accepted. */
  proposed: { verdict: string; summary: string; body: string } | null
  /** "" when it can be discussed, else why not. */
  cannotDiscuss: string
  /** The agent is answering, or something else runs on the row: nothing changes until it lands. */
  busy: boolean
}

/** What happens to a finished review: 'post' or 'hold'. */
export type PostWord = 'post' | 'hold'
/** Every rule on the machine, in the order web.rs builds them. */
export type PostingRule = {
  target: string
  manual: PostWord
  auto: PostWord
  /** Where the word came from: 'repo'/'owner' when this target sets it, 'owner' on a repo row that
   *  inherits it, '' when nothing is set and the word is the default. */
  manualVia: '' | 'repo' | 'owner'
  autoVia: '' | 'repo' | 'owner'
  /** Whether this target's findings are posted on the lines they name. */
  inline: boolean
  /** Same three states as the others, except that '' means the --inline switch decided, not a constant. */
  inlineVia: '' | 'repo' | 'owner'
  /** Owner rows only: switched to each repo on its own. Its rule, if any, is then only the fallback for a
   *  repo that has none, not a setting that decides for all of them. */
  perRepo?: boolean
}

export type Ask = { kind: string; key: string; name: string; waiting?: string; text?: string; path?: string }

/** config::snapshot, only the keys the UI reads. */
/** A panel's layout: each list names sections. A setting, since the GUI's localStorage starts empty every launch. */
export type Layout = { order?: string[]; off?: string[]; shut?: string[]; out?: string[] }

export type Settings = {
  window?: number | null
  drafts?: boolean
  /** Post each finding on the line it names, for every repo with no rule of its own. */
  inline?: boolean
  hinted?: boolean
  /** Show the key hint on every button and settings row. */
  keyhints?: boolean
  /** the PR pane's layout */
  pane?: Layout
  /** the left sidebar's layout */
  side?: Layout
  subs?: string
  theme?: string
  notify?: boolean
  model?: string
  depth?: string
  effort?: string
  voice?: string[]
  hunter?: string[]
  spells?: string[]
  scopes?: string[]
  read?: Record<string, string>
  /** url -> the updatedAt it was hidden at; a PR that moves past it shows again. */
  hidden?: Record<string, string>
  interval?: number
  [key: string]: unknown
}

export type StateData = {
  version: string
  /** Your GitHub login, "" until the first fetch. */
  me?: string
  sections: Section[]
  fetchedAt: number | null
  interval: number
  fetching: boolean
  /** Ticks finished, landed or failed, since the server started. See post_refresh. */
  ticks: number
  error: string
  auto: boolean
  /** Other gitdashys on the LAN, by a random per-launch id. */
  peers?: { id: string; auto: boolean }[]
  /** How many of `peers` run auto. */
  peersAuto?: number
  pending: number
  model: string
  running: number
  update: string
  settings: Settings
  options: Options
  knowledge: Knowledge
  asks: Ask[]
  notices: string[]
  postingRules?: PostingRule[]
  /** Which repo holds each repo's database, owners (`acme/*`) first. `db` "" is a deliberate none. */
  dbRules?: { target: string; db: string }[]
  /** Release notes since the last version run, "" once dismissed. */
  changelog: string
}

export type Finding = { kind: string; text: string; loc?: string }

export type Check = { name: string; state: string }

export type Review = {
  verdict: string
  summary: string
  model: string
  tag: string
  at: string
  findings: Finding[]
  text: string
  /** What the PR does to the database, when its repo has a DB repo. Model output: every field may be missing. */
  db?: DbImpact | null
}

export type DbImpact = {
  tables?: { name?: string; change?: string; refs?: string[]; columns?: { name?: string; change?: string; note?: string; key?: string; ref?: string }[] }[]
  risks?: { kind?: string; loc?: string; text?: string }[]
}

/** detail(): the side pane's frame for one PR. */
export type Detail = {
  url: string
  pending: boolean
  branch: string
  add?: number
  del?: number
  files?: number
  checks: Check[]
  brief: { whose: string; empty: boolean }
  pre: Pre
  /** every spell cast on this PR and what it found, kept on this machine */
  spells: { name: string; text: string; at: number; quotes: boolean }[]
  review: Review | null
}

/** code_rows(): the diff as a flat list. */
export type CodeRow =
  | { kind: 'file'; path: string; add: number; dele: number }
  | { kind: 'hunk'; header: string }
  | { kind: 'line'; n?: number; sign: string; text: string; del?: unknown; mark: string }
  | { kind: 'note'; mark: string; text: string }
  | { kind: 'orphan'; mark: string; text: string; loc: string; why: string }
  | { kind: 'gap' }

export type Code = { url: string; pending: boolean; rows: CodeRow[]; empty?: string }

export type Drafts = { promoteAt: number; items: { repo: string | null; n: number; fact: string; kind: string; team: string }[] }
