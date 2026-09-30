// The screens the curses keys opened, ported from gui.html. Each drives the modal store imperatively
// and mutates its modal in place, then repaint()s — the same shape as the vanilla openModal/paint pair.
import { api, copyText, errorText, post } from './api'
import type { Foot } from './modals'
import { busy, close, confirm, isOpen, notice, open, prompt, repaint, viewer } from './modals'
import type { Ask, Row, StateData } from './types'
import type { LEvent } from './learning'
import { LearningChart } from './components/LearningChart'
import { pageTo } from './board'

export type Ctx = {
  getData: () => StateData | null
  current: Row | null
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  call: (path: string, body?: unknown, okMsg?: string) => Promise<any>
  setting: (name: string, value: unknown) => Promise<void>
  flash: (msg: string) => void
  quit: () => Promise<void>
}

type Json = Record<string, unknown>

/** ‹ previous and next › for a screen that shows one item at a time.
 *
 *  ponytail: buttons as well as keys. These screens paged with j and k only, and the footer never said so, so
 *  with a mouse the first item was the only one there was. As footer entries the keys still work -- ModalHost
 *  fires an entry's key -- and they are left out when there is nothing to page to. No Esc entry either: Esc
 *  closes every screen. */
function pager(len: number, go: (step: number) => void): Foot[] {
  return len > 1
    ? [
        ['k', '‹ previous', () => go(-1)],
        ['j', 'next ›', () => go(1)],
      ]
    : []
}

/** What a proposal to a team's repo came back as: the pull request to approve, or why there is none. */
function proposed(ctx: Ctx, out: Json | null) {
  if (!out || out.error) return
  const url = String(out.url || '')
  if (!url) {
    notice(String(out.note || 'proposed'), 'proposed')
    return
  }
  notice(
    <>
      <div>A pull request is open on the team's repo. A person with rights on that repo approves it; gitdashy never reviews it.</div>
      <div className="link mono" style={{ marginTop: 8 }} title="click to copy" onClick={async () => ctx.flash(await copyText(url, 'the pull request'))}>
        {url}
      </div>
    </>,
    'pull request opened',
  )
}

/** A founding document's name as people say it: the brief, agents.md, or the about of one repo. */
const docName = (doc: string, repo = '') => (doc === 'agents' ? 'agents.md' : doc === 'about' ? `about ${repo}` : 'the brief')

/** Propose a change to a team's founding document (the brief, agents.md, or one repo's about) as a pull request.
 *
 *  ponytail: the only text anyone types into memory, and it never lands by itself. Every teammate's reviews
 *  and sessions read these, so the change waits for a person with rights on the team's repo.
 *  ponytail: what the document is FOR sits beside it, from the server, not in the file. The file is the team's
 *  to rewrite, and the one brief that was rewritten lost its template's guidance with its sections.
 *  The model can help: it asks what only a person can answer and offers a revision, which goes into the text
 *  only when you take it. */
export async function proposeDoc(ctx: Ctx, team: string, doc: string, repo = '') {
  const r = await api(`/api/memory?team=${encodeURIComponent(team)}&doc=${encodeURIComponent(doc)}&repo=${encodeURIComponent(repo)}`)
  if (!r.ok) {
    ctx.flash(`✗ ${await errorText(r)}`)
    return
  }
  const got = await r.json()
  const name = docName(doc, repo)
  const model = ctx.getData()?.model || 'the model'
  type Help = { questions: string[]; notes: string[]; text: string }
  let help: Help | null = null
  let asking = false
  let failed = ''
  let elapsed = 0
  let before: string | null = null
  const area = () => document.querySelector('#me') as HTMLTextAreaElement | null
  const use = () => {
    const t = area()
    if (!t || !help?.text) return
    before = t.value
    t.value = help.text
    refresh()
  }
  const undo = () => {
    const t = area()
    if (!t || before === null) return
    t.value = before
    before = null
    refresh()
  }
  const ask = async () => {
    if (asking) return
    const out = await ctx.call('/api/doc-help', { team, doc, repo, text: area()?.value || '' })
    if (!out) return
    asking = true
    failed = ''
    help = null
    refresh()
    const poll = async () => {
      if (!isOpen(m)) return
      // a poll that cannot be answered ends the wait with the reason, or the spinner would spin until Esc
      let j
      try {
        j = await (await api('/api/doc-help')).json()
      } catch (e) {
        asking = false
        failed = `lost the help: ${e instanceof Error ? e.message : String(e)}`
        refresh()
        return
      }
      if (j.running) {
        elapsed = j.elapsed || 0
        refresh()
        setTimeout(poll, 700)
        return
      }
      asking = false
      if (j.error) failed = String(j.error)
      else help = j.result as Help
      refresh()
    }
    void poll()
  }
  const propose = async (text: string) => {
    if (!(await confirm(`This opens a pull request on team ${team}'s repo. Nothing changes until a person with rights on that repo approves it.`, { yes: 'open pull request', no: 'keep editing' }))) return false
    const body = { op: 'propose', team, doc, repo, text }
    let out = await ctx.call('/api/memory', body)
    if (!out) return false // refused (no remote, a failed fetch): the editor stays open, so the text is not lost
    if (out.warn) {
      if (!(await confirm(`${out.warn}\n\nOpen the pull request anyway?`, { yes: 'open it anyway', no: 'keep editing' }))) return false
      out = await ctx.call('/api/memory', { ...body, anyway: true })
      if (!out) return false
    }
    proposed(ctx, out)
    return true
  }
  const list = (title: string, xs: string[]) =>
    xs.length ? (
      <>
        <div className="lab">{title}</div>
        <ul>
          {xs.map((x, k) => (
            <li key={k}>{x}</li>
          ))}
        </ul>
      </>
    ) : null
  const m = open({
    title: `propose a change · ${team} / ${name}`,
    sub: got.path,
    wide: true,
    dismiss: false,
    focus: '#me',
    body: () => (
      <div className="doced">
        <textarea id="me" defaultValue={got.text || got.draft || ''} />
        <aside>
          <pre className="guide">{got.guide}</pre>
          <div className="help">
            {asking ? (
              <div>
                <span className="spinner" /> {model} is reading your draft… <span className="mono dim">{elapsed}s</span>
              </div>
            ) : failed ? (
              <div className="warn">✗ {failed}</div>
            ) : help ? (
              <>
                {list(`${model} asks`, help.questions)}
                {list('notes', help.notes)}
                {help.text ? <div className="dim">Its revision is ready: take it into the text (F3), then edit it, or ignore it.</div> : null}
              </>
            ) : (
              <div className="dim">Stuck, or want a second look? {model} can ask what this needs and propose a revision (F2). Nothing goes into the text until you take it.</div>
            )}
          </div>
        </aside>
      </div>
    ),
    foot: [],
  })
  // the footer follows the help's state: what can be done now, and nothing that cannot
  const refresh = () => {
    m.foot = [
      ['^S', 'propose (pull request)', async () => { const t = area(); if (t && (await propose(t.value))) close(m) }, 'go'],
      ...(asking ? [] : ([['F2', help ? 'ask again' : 'help me write this', () => void ask()]] as Foot[])),
      ...(help?.text ? ([['F3', 'take its revision', use]] as Foot[]) : []),
      ...(before !== null ? ([['F4', 'undo that', undo]] as Foot[]) : []),
      ['Esc', 'discard', () => close(m)],
    ] as Foot[]
    repaint()
  }
  m.keys = { Escape: () => close(m), 'ctrl+s': () => m.foot![0][2](), F2: () => void ask(), F3: use, F4: undo }
  refresh()
}

export type KnowledgeTab = 'stats' | 'inspect' | 'drafts'
const TABS: [KnowledgeTab, string][] = [
  ['stats', 'K'],
  ['inspect', 'g'],
  ['drafts', 'W'],
]

/** One thing inspect can open: a facts file (repo, "" for general) or a team's founding document (doc). */
export type KFile = { team: string; repo?: string; doc?: string }
const fileKey = (f: KFile) => JSON.stringify([f.team, f.repo || '', f.doc || ''])
const fileName = (f: KFile) => (f.doc ? docName(f.doc, f.repo).replace(/^the /, '') : f.repo || 'general')

/** Everything the memory holds, in one panel: how fast it learns, what it knows file by file, what is waiting
 *  for a second sighting, what the team has of yours, and the dream as an action over it.
 *
 *  ponytail: these were five buttons in the rail, each its own screen. One panel with tabs keeps K / g / W / P
 *  as keys that open it on their tab, and switch tabs inside it. A tab reloads when it is shown, so a dream
 *  made over the panel is not acted on from a stale list.
 *  ponytail: inspect reads and removes, it does not write. Facts arrive through reviews and the two-sightings
 *  gate; a person only takes them out. Out of yours at once, out of a team's by pull request. */
export async function knowledgeScreen(ctx: Ctx, first: KnowledgeTab, pick: KFile = { team: '', repo: '' }) {
  let tab = first
  let events: LEvent[] = []
  let drafts: Json[] = []
  let promoteAt = 2
  let i = 0
  const failed: Partial<Record<KnowledgeTab, string>> = {}
  let files: KFile[] = []
  let file = pick
  let facts: string[] = []
  let backers: string[][] = []
  let doc = ''
  let where = ''
  const model = ctx.getData()?.model || 'the model'

  const load = async (t: KnowledgeTab) => {
    const url =
      t === 'stats'
        ? '/api/learning'
        : t === 'drafts'
          ? '/api/drafts'
          : `/api/memory?team=${encodeURIComponent(file.team)}&${file.doc ? `doc=${encodeURIComponent(file.doc)}&` : ''}repo=${encodeURIComponent(file.repo || '')}`
    const r = await api(url)
    if (!r.ok) {
      failed[t] = await errorText(r)
      return
    }
    delete failed[t]
    const got = await r.json()
    if (t === 'stats') events = got.events
    else if (t === 'drafts') {
      drafts = got.items
      promoteAt = got.promoteAt
    } else {
      facts = got.facts || []
      backers = got.backers || []
      doc = got.text || ''
      where = got.path || ''
    }
    if (t === tab) i = pageTo(i, 0, items().length)
  }
  const items = (): unknown[] => (tab === 'drafts' ? drafts : tab === 'inspect' && !file.doc ? facts : [])

  const show = async (t: KnowledgeTab) => {
    if (t !== tab) i = 0
    tab = t
    await load(t)
    refresh()
  }
  const open_ = async (f: KFile) => {
    file = f
    i = 0
    await show('inspect')
  }
  const go = (step: number) => {
    i = pageTo(i, step, items().length)
    refresh()
  }
  const act = async (path: string, body: Json, ok: string) => {
    const out = await ctx.call(path, body, ok)
    // a forget can leave the team's copy to remove: that is a pull request, and it says so
    if (out?.branch) proposed(ctx, out)
    else if (out?.error) notice(String(out.error))
    await load(tab)
    refresh()
  }
  const remove = async () => {
    const fact = facts[i]
    if (fact === undefined) return
    const theirs = !!file.team
    const ask = theirs
      ? `Propose removing this fact from team ${file.team}'s ${fileName(file)} memory? It opens a pull request on the team's repo; the fact stays until a person with rights on that repo approves it.`
      : `Remove this fact from your ${fileName(file)} memory? It is gone from your reviews at once.`
    if (!(await confirm(`${ask}\n\n${fact}`, { yes: theirs ? 'open pull request' : 'remove', no: 'keep it' }))) return
    const out = await ctx.call('/api/memory', { op: 'remove', team: file.team, repo: file.repo || 'general', fact }, theirs ? '' : 'removed')
    if (theirs) proposed(ctx, out)
    await load('inspect')
    refresh()
  }

  const factCard = (repo: unknown, team: string, mark: string, warn: boolean, fact: unknown) => (
    <>
      <div className="kv">
        <span>
          {String(repo || 'general')}
          {team}
        </span>
        <b className={`mark${warn ? ' warn' : ''}`}>{mark}</b>
      </div>
      <div className="fact">{String(fact)}</div>
    </>
  )

  const picker_ = () => {
    const all = files.some((f) => fileKey(f) === fileKey(file)) ? files : [file, ...files]
    return (
      <select
        value={fileKey(file)}
        aria-label="which memory to inspect"
        onChange={(e) => {
          const f = all.find((x) => fileKey(x) === e.target.value)
          if (f) void open_(f)
        }}
      >
        {[...new Set(all.map((f) => f.team))].map((team) => (
          <optgroup key={team} label={team ? `team ${team}` : 'your memory'}>
            {all
              .filter((f) => f.team === team)
              .map((f) => (
                <option key={fileKey(f)} value={fileKey(f)}>
                  {team ? `${team} / ` : ''}
                  {fileName(f)}
                  {f.doc ? ' (founding document)' : ''}
                </option>
              ))}
          </optgroup>
        ))}
      </select>
    )
  }

  const inspect = () => (
    <div className="inspect">
      <div className="lbar">
        {picker_()}
        <span className="sp" />
        <span className="dim mono">{where}</span>
      </div>
      <p className="knote">
        {file.doc
          ? `${file.doc === 'about' ? `What ${file.repo} is, as team ${file.team} wrote it: reviews of that repo read it right after the team's brief.` : file.doc === 'brief' ? `Team ${file.team}'s brief: why it builds what it builds, read by every review of every repo bound to it.` : `How agent sessions work in team ${file.team}'s repos; reviews never read it.`} A change is proposed as a pull request on the team's repo and approved by a person with rights on it; gitdashy never reviews those pull requests.`
          : file.team
            ? `Team ${file.team}'s facts, learned by its reviews. Removing one opens a pull request on the team's repo; it stays until a person with rights on that repo approves it.`
            : 'Your facts, learned by your reviews. Nothing is typed in here: a fact arrives when two reviews find it. Removing one takes it out of your memory at once.'}
      </p>
      {failed.inspect ? (
        <p className="empty">✗ {failed.inspect}</p>
      ) : file.doc ? (
        doc.trim() ? <pre className="doc">{doc}</pre> : <p className="empty">nothing written yet</p>
      ) : facts.length ? (
        <div className="facts">
          {facts.map((f, k) => (
            <div key={k} className={`opt${k === i ? ' on' : ''}`} onClick={() => { i = k; refresh() }}>
              <span className="tick">{k === i ? '›' : ''}</span>
              <span>{f}</span>
              {backers[k]?.length ? (
                <span className="dim" title={backers[k].join(', ')}>
                  {backers[k].length > 1 ? `★ ${backers[k].length} people found this` : `found by ${backers[k][0]}`}
                </span>
              ) : null}
            </div>
          ))}
        </div>
      ) : (
        <p className="empty">nothing learned here yet</p>
      )}
    </div>
  )

  const content = () => {
    if (tab === 'inspect') return inspect()
    if (failed[tab]) return <p className="empty">✗ {failed[tab]}</p>
    if (tab === 'stats') {
      return events.length ? <LearningChart events={events} /> : <p className="empty">nothing learned yet: the chart fills in as reviews propose and confirm facts</p>
    }
    // drafts, the one tab left
    const it = items()[i] as Json | undefined
    if (!it) return <p className="empty">nothing waiting — every observation so far is either a fact or gone</p>
    const left = promoteAt - (it.n as number)
    const count = `seen ${it.n}×` + (left > 0 ? ` · ${left} more to go` : ' · confirmed')
    const mark = it.kind === 'self' ? 'pre-review · one opinion' : it.kind === 'team' ? `in the team's drafts · ${count}` : count
    return factCard(it.repo, it.team ? ` · team ${it.team}` : '', mark, true, it.fact)
  }

  // every tab up front: the tabs show their counts, and the panel never opens on an empty tab that is still loading
  await Promise.all([
    ...TABS.map(([t]) => load(t)),
    api('/api/memory/files')
      .then((r) => (r.ok ? r.json() : { files: [] }))
      .then((j) => (files = j.files)),
  ])
  const m = open({
    title: 'knowledge',
    wide: true,
    body: () => (
      <div className="kpanel">
        <div className="lbar">
          <div className="seg" role="tablist" aria-label="knowledge">
            {TABS.map(([t, key]) => (
              <button key={t} role="tab" aria-pressed={tab === t} onClick={() => void show(t)}>
                {t}
                {t === 'drafts' && drafts.length ? ` ${drafts.length}` : ''} <kbd className="hint">{key}</kbd>
              </button>
            ))}
          </div>
          <span className="sp" />
          <div className="tags">
            <button className="tag" title="asks before it starts" onClick={() => void dreamScreen(ctx)}>
              dream: tidy memory <kbd className="hint">Z</kbd>
            </button>
          </div>
        </div>
        {(
          <p className="knote">
            <b>Dream</b> has {model} read every memory file, yours and your teams', and propose a tidier version of yours:
            overlapping facts merged, duplicates removed, stale ones dropped. You see each file's before and
            after and nothing is written until you accept. Team files are only read, so yours do not end up
            repeating theirs; a dream never rewrites them.
          </p>
        )}
        {content()}
      </div>
    ),
    foot: [],
  })

  const refresh = () => {
    const list = items()
    const it = list[i] as Json | undefined
    m.sub = tab === 'stats' ? (events.length ? `${events.length} events` : '') : tab === 'inspect' && file.doc ? '' : `${list.length ? i + 1 : 0}/${list.length}`
    const foot: Foot[] = [...pager(list.length, go)]
    if (tab === 'inspect' && file.doc) foot.push(['e', doc.trim() ? 'propose a change (pull request)' : 'write it (pull request)', () => void proposeDoc(ctx, file.team, file.doc!, file.repo || ''), 'go'])
    if (tab === 'inspect' && !file.doc && facts[i] !== undefined) foot.push(['x', file.team ? 'propose removing it (pull request)' : 'remove it', () => void remove(), 'warn'])
    // a draft in a team's pool: dropping it is yours to do, but accepting it into the team's knowledge by hand is
    // a pull request; only a second independent sighting moves it there by itself
    if (tab === 'drafts' && it && it.kind === 'team') {
      const pooled = { repo: it.repo, fact: it.fact, team: it.team, pooled: true }
      foot.push(
        ['t', 'propose as a team fact (pull request)', () => void act('/api/drafts', { op: 'promote', ...pooled }, ''), 'go'],
        ['x', 'drop', () => void act('/api/drafts', { op: 'drop', ...pooled }, 'dropped'), 'warn'],
      )
    } else if (tab === 'drafts' && it) {
      foot.push(
        ['t', 'make it a fact', () => void act('/api/drafts', { op: 'promote', repo: it.repo, fact: it.fact }, 'accepted'), 'go'],
        ['x', 'drop', () => void act('/api/drafts', { op: 'drop', repo: it.repo, fact: it.fact }, 'dropped'), 'warn'],
      )
    }
    if (tab === 'drafts' && list.length > 1) foot.push(['s', 'scan for repeats', () => overlapScreen(ctx, () => void show('drafts'))])
    m.foot = foot
    repaint()
  }
  const step = (by: number) => void show(TABS[pageTo(TABS.findIndex(([t]) => t === tab), by, TABS.length)][0])
  m.keys = {
    K: () => void show('stats'),
    g: () => void show('inspect'),
    W: () => void show('drafts'),
    '[': () => step(-1),
    ']': () => step(1),
    Z: () => void dreamScreen(ctx),
    Escape: () => close(m),
    q: () => close(m),
  }
  refresh()
}

export async function overlapScreen(ctx: Ctx, onDone?: () => void) {
  void ctx
  await post('/api/overlaps', { op: 'start' })
  const model = ctx.getData()?.model || 'the model'
  let pairs: Json[] | null = null
  let i = 0
  const m = open({
    title: 'same fact?',
    dismiss: false,
    body: () => {
      if (pairs === null)
        return (
          <div>
            <span className="spinner" /> {model} is reading the candidates…
          </div>
        )
      if (!pairs.length) return <div>no two drafts look like one fact — nothing to fold</div>
      const p = pairs[i]
      return (
        <>
          <div className="kv">
            <span>{String(p.repo || 'general')}</span>
            <b className={`mark${p.promotes ? '' : ' warn'}`}>
              {String(p.says)} · folds to {String(p.would)}×{p.promotes ? ' · becomes a fact' : ''}
            </b>
          </div>
          <div className="lab" style={{ marginTop: 8 }}>A</div>
          <div className="fact">{String(p.a)}</div>
          <div className="lab">B</div>
          <div className="fact">{String(p.b)}</div>
        </>
      )
    },
    foot: [],
  })
  const done = () => {
    close(m)
    onDone?.()
  }
  const fold = async (keepA: boolean) => {
    const p = pairs![i]
    await ctx.call('/api/overlaps', { op: 'merge', repo: p.repo, keep: keepA ? p.a : p.b, drop: keepA ? p.b : p.a }, 'folded')
    next()
  }
  const next = () => {
    if (!pairs) return
    i += 1
    if (i >= pairs.length) done()
    else repaint()
  }
  m.keys = { Escape: done, q: done }
  const poll = async () => {
    if (!isOpen(m)) return
    const j = await (await api('/api/overlaps')).json()
    if (j.running) {
      repaint()
      setTimeout(poll, 1000)
      return
    }
    if (j.error) {
      close(m)
      notice(`scan failed: ${j.error}`)
      return
    }
    pairs = j.result || []
    m.sub = pairs!.length ? `1/${pairs!.length}` : ''
    if (pairs!.length) {
      m.foot = [
        ['y', 'one fact, keep A', () => fold(true), 'go'],
        ['b', 'keep B', () => fold(false)],
        ['n', 'different', next],
        ['Esc', 'stop', done],
      ] as Foot[]
      m.keys = { ...m.keys, y: () => fold(true), b: () => fold(false), n: next }
    } else {
      m.foot = [['Esc', 'close', done]] as Foot[]
    }
    repaint()
  }
  poll()
}

export async function teamsScreen(ctx: Ctx, p: Row | null) {
  const load = async () => (await api('/api/teams')).json()
  let got = await load()
  const m = open({
    title: got.teams.length ? `teams · ${got.teams.length} joined` : 'teams · none yet',
    body: () =>
      got.teams.length ? (
        got.teams.map((t: Json, i: number) => (
          <div key={String(t.key)} className="opt" onClick={() => pick(i)}>
            <kbd>{i + 1}</kbd>
            <span>{String(t.name)}</span>
            <em>
              {t.arrived ? `${t.arrived} new · ` : ''}
              {String(t.remote || 'no remote yet')}
            </em>
          </div>
        ))
      ) : (
        <div>
          a team is a git repo of shared memory.
          <br />
          start one here, or join one that exists.
        </div>
      ),
    foot: [],
  })
  const pick = async (i: number) => {
    const t = got.teams[i]
    if (t) {
      await teamScreen(ctx, String(t.key), p)
      await reload()
    }
  }
  const refresh = () => {
    m.title = got.teams.length ? `teams · ${got.teams.length} joined` : 'teams · none yet'
    m.foot = [
      ['n', 'start one', newTeam],
      ['a', 'join one', joinTeam],
      ['Esc', 'close', () => close(m)],
    ] as Foot[]
    m.keys = { n: newTeam, a: joinTeam, Escape: () => close(m), q: () => close(m) }
    for (let i = 1; i <= 8; i++) m.keys![String(i)] = () => pick(i - 1)
    repaint()
  }
  const reload = async () => {
    got = await load()
    refresh()
  }
  async function newTeam() {
    const name = await prompt('Name the new team (this is what your repos get bound to):')
    if (!name) return
    const desc = await prompt('One line: what is this team for? (shared with everyone who joins)')
    const owner = p ? p.repo.split('/')[0] : ''
    const cover = owner && (await confirm(`${name.slice(0, 24)} covers ${owner}/*, for everyone who joins?`))
    const out = await busy('starting team', `creating ${name}…`, () =>
      ctx.call('/api/teams', { op: 'new', name, desc, owner: cover ? owner : '' }, `started ${name}`),
    )
    if (out) await teamScreen(ctx, out.key, p)
    await reload()
    await askConsents(ctx)
  }
  async function joinTeam() {
    const repo = await prompt('Existing team (a git URL, owner/name on GitHub, or a path to a bare repo):')
    if (!repo) return
    const out = await busy('joining team', `cloning ${repo}…`, () => ctx.call('/api/teams', { op: 'join', repo }, 'joined'))
    if (out?.warning) await notice(out.warning)
    if (out?.key) await teamScreen(ctx, out.key, p)
    await reload()
    await askConsents(ctx)
  }
  refresh()
}

export function teamScreen(ctx: Ctx, key: string, p: Row | null): Promise<void> {
  return new Promise((res) => {
    const load = async () => (await api('/api/teams')).json().then((g) => g.teams.find((t: Json) => t.key === key))
    void (async () => {
      let t = await load()
      if (!t) return res()
      for (const target of (t.undecided as string[]) || []) {
        const yes = await confirm(`${t.name.slice(0, 24)} now covers ${target} — use it here too?`)
        await ctx.call('/api/teams', { op: 'claim', key, target, yes })
      }
      t = await load()
      const m = open({
        title: `${t.name}  (${key})`,
        body: () => (
          <>
            <div className="kv"><span>what it is for</span><b>{String(t.description || 'nothing yet')}</b></div>
            <div className="kv"><span>checkout</span><b>{String(t.checkout)}</b></div>
            <div className="kv"><span>git remote</span><b>{String(t.remote || 'none yet')}</b></div>
            <div className="kv"><span>used for</span><b>{String(t.used || 'nothing yet')}</b></div>
          </>
        ),
        foot: [],
      })
      const done = () => {
        close(m)
        res()
      }
      const reload = async () => {
        t = await load()
        if (!t) return done()
        repaint()
      }
      const verbs: Record<string, () => Promise<void>> = {
        // the brief is a founding document: a change to it is a pull request, never a push
        e: () => proposeDoc(ctx, key, 'brief'),
        d: async () => {
          const desc = await prompt(`One line: what is ${t.name} for?  [now: ${String(t.description).slice(0, 40) || 'nothing yet'}]`)
          if (desc) {
            await ctx.call('/api/teams', { op: 'describe', key, desc }, 'described')
            await reload()
          }
        },
        c: async () => {
          const url = await prompt(`Git URL for ${key} (an EMPTY repo you can push to — one with history is a team to join):`)
          if (url) {
            const out = await ctx.call('/api/teams', { op: 'connect', key, url })
            if (out) await notice(`${key} now pushes to ${out.remote}`)
            await reload()
          }
        },
        o: async () => {
          const dflt = p ? p.repo.split('/')[0] : ''
          const said = await prompt('Cover which owner?' + (dflt ? ` [${dflt}]` : ' (e.g. neomedsys):'), dflt)
          const owner = said || dflt
          if (!owner) return
          if (!(await confirm(`${t.name.slice(0, 24)} covers ${owner}/*, for everyone who joins?`))) return
          await ctx.call('/api/teams', { op: 'cover', key, owner }, `covers ${owner}/*`)
          await reload()
        },
        x: async () => {
          const what = t.linked ? 'removes only the link, your checkout is kept' : 'DELETES its files'
          if (!(await confirm(`leave ${key}? it ${what} — ${t.checkout}`, { yes: 'leave', no: 'stay' }))) return
          if (await ctx.call('/api/teams', { op: 'leave', key }, `left ${key}`)) done()
        },
      }
      m.foot = [
        ['e', 'propose a brief change', verbs.e],
        ['d', 'describe', verbs.d],
        ['c', 'remote', verbs.c],
        ['o', 'cover', verbs.o],
        ['x', 'leave', verbs.x, 'warn'],
        ['Esc', 'back', done],
      ] as Foot[]
      m.keys = { ...verbs, Escape: done, q: done }
      repaint()
    })()
  })
}

export async function dreamScreen(ctx: Ctx) {
  if (!(await confirm('Dream: Claude tidies every memory file (merge, dedupe, drop stale). Nothing is written until you accept. Start?'))) return
  await post('/api/dream', { op: 'start' })
  const model = ctx.getData()?.model || 'the model'
  const SKY = '˖ ⋆ ✧ ✦ ☾ · ° ˚ z Z'.split(' ')
  let seed = 1
  const rnd = () => (seed = (seed * 16807) % 2147483647) / 2147483647
  const sky = () => Array.from({ length: 4 }, () => Array.from({ length: 44 }, () => (rnd() < 0.18 ? SKY[Math.floor(rnd() * SKY.length)] : ' ')).join('')).join('\n')
  let res: Json | null = null
  let elapsedS = 0
  const m = open({
    title: 'dreaming',
    dismiss: false,
    body: () =>
      res ? (
        <>
          <pre>{String(res.summary)}</pre>
          <div className="sep" />
          {(res.files as Json[]).map((f, i) => (
            <div key={i} className={`diffrow${f.deleted ? ' gone' : ''}`}>
              <span>{String(f.name)}</span>
              <span>
                {String(f.before)} → {f.deleted ? 'DELETED' : String(f.after)}
              </span>
            </div>
          ))}
          {/* the dream reads the team's files and rewrites none of them: say how many, or one simply
              missing from the list reads as a file it never looked at. */}
          {Number(res.theirs) > 0 && (
            <div className="diffrow">
              <span>
                {String(res.theirs)} team file{Number(res.theirs) === 1 ? '' : 's'} read, none changed
              </span>
              <span />
            </div>
          )}
        </>
      ) : (
        <>
          <div className="sky">{sky()}</div>
          <div style={{ marginTop: 10 }}>
            <span className="spinner" /> {model} is tidying the memories…{' '}
            <span className="mono" style={{ color: 'var(--dim2)' }}>{elapsedS}s</span>
          </div>
        </>
      ),
    foot: [['Esc', 'wake up without changes', () => { close(m); post('/api/dream', { op: 'discard' }) }]] as Foot[],
  })
  const stop = () => {
    close(m)
    post('/api/dream', { op: 'discard' })
  }
  m.keys = { Escape: stop }
  const poll = async () => {
    if (!isOpen(m)) return
    const j = await (await api('/api/dream')).json()
    if (j.running) {
      elapsedS = j.elapsed
      repaint()
      setTimeout(poll, 500)
      return
    }
    if (j.error) {
      close(m)
      notice(`dream failed: ${j.error}`)
      return
    }
    res = j.result
    const gone = (res!.files as Json[]).filter((f) => f.deleted)
    m.title = 'dream over'
    const accept = async () => {
      if (gone.length && !(await confirm(`DELETE ${gone.map((f) => f.name).join(', ')} — ${res!.lost} fact${res!.lost === 1 ? '' : 's'} lost. Sure?`, { yes: 'delete', no: 'keep' }))) return
      const out = await ctx.call('/api/dream', { op: 'apply' }, 'memory rewritten')
      if (out?.error) await notice(out.error)
      close(m)
    }
    // nothing of yours to change: "accept and rewrite memory" offered a rewrite that cannot happen,
    // and the keypress would still take a backup and a commit for it.
    const nothing = (res!.files as Json[]).length === 0
    m.foot = [
      ...(nothing ? [] : [['y', gone.length ? `accept — DELETES ${gone.length} file${gone.length === 1 ? '' : 's'}` : 'accept and rewrite memory', accept, gone.length ? 'warn' : 'go']]),
      ['v', 'view full', () => viewer('the dream', String(res!.detail))],
      ['n', nothing ? 'close — nothing of yours to change' : 'discard', stop],
    ] as Foot[]
    m.keys = { ...Object.fromEntries(m.foot.map((f) => [f[0], f[2]])), Escape: stop }
    repaint()
  }
  poll()
}

export async function updateScreen(ctx: Ctx) {
  const v = ctx.getData()?.update
  if (!v) return
  if (!(await confirm(`v${ctx.getData()?.version}  →  v${v}. Installs the release tag and restarts gitdashy. Update now?`, { yes: 'update now', no: 'later' }))) return
  const n = ctx.getData()?.running || 0
  if (n && !(await confirm(`${n} agent${n === 1 ? '' : 's'} running. The restart kills ${n === 1 ? 'it' : 'them'}. Update anyway?`, { yes: 'update anyway', no: 'later' }))) return
  // ponytail: the server reports no progress, so the spinner runs until a failure notice arrives,
  // the process re-execs under a desktop window, or the cap runs out (a --browser page survives the
  // re-exec and would otherwise sit behind it forever). Add a real bar when the server can report one.
  await busy('update', `downloading v${v}…`, async () => {
    const seen = (ctx.getData()?.notices || []).length
    if (!(await ctx.call('/api/update', {}))) return
    for (let i = 0; i < 240; i++) {
      await new Promise((r) => setTimeout(r, 500))
      if ((ctx.getData()?.notices || []).length > seen) return
    }
  })
}

/** The release notes under the header picture: once after an update, and again from the menu. */
export function whatsNew(text: string, version = '') {
  return viewer("what's new", text, `v${version}`, <img className="news" src="/whats-new.webp" alt="" />)
}

export function escMenu(ctx: Ctx) {
  let idx = 0
  // the fourth slot is the board key that does the same thing, where there is one
  const items = (): [string, string, () => void | Promise<void>, string?][] => {
    const s = ctx.getData()?.settings || {}
    return [
      ['Theme', s.theme || 'pencil', () => void cycleTheme(ctx)],
      ['Notify', s.notify ? 'on' : 'off', () => void ctx.setting('notify', !s.notify)],
      ['LAN', s.lan ? 'on' : 'off', () => void ctx.setting('lan', !s.lan)],
      ['Refresh', '', async () => { await ctx.call('/api/refresh', {}, 'refreshing…'); close(m) }, 'f'],
      ["What's new", '', async () => {
        const r = await api('/api/changelog')
        if (!r.ok) return ctx.flash(`✗ ${await errorText(r)}`)
        close(m)
        whatsNew((await r.json()).text, ctx.getData()?.version)
      }],
      ['Debug', '', () => { close(m); void debugScreen(ctx) }],
      ['Quit', '', () => void ctx.quit(), 'q'],
    ]
  }
  const m = open({
    title: 'gitdashy',
    body: () =>
      items().map(([l, v, , key], i) => (
        <div key={l} className={`opt${i === idx ? ' on' : ''}`} onClick={() => pick(i)}>
          <span className="tick">{i === idx ? '▸' : ''}</span>
          <span>{l}</span>
          {key ? <kbd className="hint">{key}</kbd> : null}
          <em>{v}</em>
        </div>
      )),
    foot: [['⏎', 'pick', () => pick(idx), 'go'], ['Esc', 'close', () => close(m)], ['q', 'quit', () => void ctx.quit()]] as Foot[],
  })
  const pick = (i: number) => {
    idx = i
    void items()[i][2]()
  }
  const n = () => items().length
  m.keys = {
    j: () => { idx = (idx + 1) % n(); repaint() },
    k: () => { idx = (idx - 1 + n()) % n(); repaint() },
    Enter: () => pick(idx),
    Escape: () => close(m),
    q: () => void ctx.quit(),
  }
}

/** The diagnostic bundle from /api/debug, as pretty JSON you can copy into a bug report. */
export async function debugScreen(ctx: Ctx) {
  const r = await api('/api/debug')
  if (!r.ok) {
    ctx.flash(`✗ ${await errorText(r)}`)
    return
  }
  const text = JSON.stringify(await r.json(), null, 2)
  const copy = async () => ctx.flash(await copyText(text, 'the debug data'))
  const m = viewer('debug', text, 'holds repo names, pr urls and config paths — read it before pasting it in public')
  m.keys!.y = copy
  m.foot!.unshift(['y', 'copy', copy, 'go'])
  repaint()
}

async function cycleTheme(ctx: Ctx) {
  const all = ctx.getData()?.options.theme || []
  const cur = ctx.getData()?.settings.theme
  await ctx.setting('theme', all[(all.indexOf(cur || '') + 1) % all.length])
}

export async function setPath(ctx: Ctx, which: 'L' | 'C') {
  const k = ctx.getData()?.knowledge
  const cur = which === 'L' ? k?.memory : k?.store
  const what = which === 'L' ? 'Memory' : 'Store'
  const path = await prompt(`${what} directory${which === 'L' ? ', or a git repo to clone' : ''} [${cur}]:`)
  if (!path) return
  let r = await post('/api/path', { which, path })
  const out = r.ok ? await r.json() : null
  if (!r.ok) {
    ctx.flash(`✗ ${await errorText(r)}`)
    return
  }
  if (out.confirm) {
    if (!(await confirm(out.confirm))) return
    r = await post('/api/path', { which, path, force: true })
    if (!r.ok) {
      ctx.flash(`✗ ${await errorText(r)}`)
      return
    }
  }
  ctx.flash(`${what} is now ${path}`)
}

export async function askConsents(ctx: Ctx, asks?: Ask[]) {
  let list = asks
  if (!list) {
    try {
      list = ((await (await api('/api/asks')).json()).asks as Ask[]) || []
    } catch {
      return // server gone; the next poll retries
    }
  }
  if (!list.length) return
  const a = list[0]
  if (a.kind === 'publishing') {
    const m = open({
      title: `${a.name}  (${a.key})`,
      dismiss: false,
      body: () => (
        <>
          <div>from now on this team receives, for the repos bound to it:</div>
          <div style={{ margin: '8px 0 8px 12px' }}>
            · facts of yours, as they are confirmed
            <br />· what your reviews proposed, unconfirmed
          </div>
          <div className="mono" style={{ color: 'var(--amber)' }}>{a.waiting ? a.waiting + ' waiting to go' : 'nothing waiting yet'}</div>
        </>
      ),
      foot: [['y', 'yes', () => answer(true), 'go'], ['n', 'not this team', () => answer(false)]] as Foot[],
    })
    const answer = async (yes: boolean) => {
      close(m)
      await ctx.call('/api/consent', { kind: 'publishing', key: a.key, yes })
      askConsents(ctx)
    }
    m.keys = { y: () => answer(true), n: () => answer(false) }
  } else {
    const m = open({
      title: `${a.name}  (${a.key})  ·  what it tells your sessions to do`,
      dismiss: false,
      wide: true,
      body: () => (
        <>
          <div>this team's agents.md reaches every session in its repos. it is written by whoever can push to the team's repo.</div>
          <pre style={{ margin: '10px 0', padding: 10, background: 'var(--bg4)', borderRadius: 8, maxHeight: '40vh', overflow: 'auto' }}>{a.text}</pre>
          <div className="mono" style={{ fontSize: 11, color: 'var(--dim2)' }}>{a.path}</div>
          <div style={{ marginTop: 8 }}>reviews never see it. nothing else on this machine changes.</div>
        </>
      ),
      foot: [['y', 'let sessions read it', () => answer(true), 'go'], ['n', 'keep it out', () => answer(false)]] as Foot[],
    })
    const answer = async (yes: boolean) => {
      close(m)
      await ctx.call('/api/consent', { kind: 'agents', key: a.key, yes, text: a.text })
      askConsents(ctx)
    }
    m.keys = { y: () => answer(true), n: () => answer(false) }
  }
}
