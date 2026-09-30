import { Pinata } from './Pinata'
import { useNow } from '../usePoll'
import type { StateData } from '../types'
import type { Only, VisSection } from '../board'

type Props = {
  data: StateData | null
  /** What the filters left on screen — the counts beside the brand describe this, not the raw board. */
  secs: VisSection[]
  spinning: boolean
  onRefresh: () => void
  onAuto: () => void
  onMenu: () => void
  onUpdate: () => void
  onHelp: () => void
  onLogo: () => void
  view: 'board' | 'graph' | 'necronomicon'
  onView: (v: 'board' | 'graph' | 'necronomicon') => void
  /** The repo and author picks; empty is all. */
  only: Only
  /** False when a picker would open empty (no data yet, or nothing on the board). */
  canPick: (which: keyof Only) => boolean
  onOnly: (which: keyof Only) => void
  onClearOnly: (which: keyof Only) => void
}

/** "Ns" to the next refresh: the only ticking text, so only it re-renders every second, not its bar. */
export function Countdown({ at, interval }: { at: number; interval: number }) {
  const now = useNow(1000)
  return <>{Math.max(0, Math.round(interval - (now / 1000 - at)))}s</>
}

/** The 44px bar: brand, counts, the refresh state, and the actions the whole app can take. */
export function TopBar({ data: d, spinning, secs, onRefresh, onAuto, onMenu, onUpdate, onHelp, onLogo, view, onView, only, canPick, onOnly, onClearOnly }: Props) {
  const running = d?.running || 0
  // ponytail: both numbers come off the same list. Counting PRs after the filters and repos before
  // them read as "3 PRs · 12 repos", which is two answers to one question.
  const shown = secs.flatMap((x) => x.prs)
  const total = shown.length
  const repos = new Set(shown.map((p) => p.repo)).size
  return (
    <div className="top">
      <button className="logo" title="play the intro" aria-label="toggle the player" onClick={onLogo}>
        <img src="/head.png" alt="" />
      </button>
      <div style={{ display: 'flex', alignItems: 'baseline', gap: 8 }}>
        <span className="brand">gitdashy</span>
        <span className="mono" style={{ fontSize: 11, color: 'var(--dim2)' }}>
          v{d?.version || ''}
        </span>
      </div>
      <div className="vbar" />
      <div className="ctx">
        {/* ponytail: repos, not orgs. The board spans whatever the token can see, and "4 repos" is the
            number that tells you whether a queue looks short because it is, or because you are
            pointed at less than you thought. */}
        {d ? (
          <>
            <span style={{ color: 'var(--ink)', fontWeight: 500 }}>{total} PRs</span>
            <i className="dot" />
            <span>{repos} repo{repos === 1 ? '' : 's'}</span>
          </>
        ) : (
          <i className="sk" style={{ width: '8em' }} />
        )}
      </div>
      <div className="views" title="switch view (G)">
        <div className="vtabs">
          {(['board', 'graph', 'necronomicon'] as const).map((v) => (
            <button key={v} className={view === v ? 'on' : ''} onClick={() => onView(v)}>
              {v}
            </button>
          ))}
        </div>
        <kbd className="hint">G</kbd>
      </div>
      <div className="fgroup">
        {(['repos', 'authors'] as const).map((w) => {
          const n = only[w].length
          return (
            <button key={w} className="chip" aria-pressed={!!n} disabled={!canPick(w)} title={`only these ${w}`} onClick={() => onOnly(w)}>
              {w} <b>{n ? (n === 1 ? only[w][0].split('/').pop() : n) : 'all'}</b>
              {n ? (
                <span
                  aria-label={`clear ${w}`}
                  onClick={(e) => {
                    e.stopPropagation()
                    onClearOnly(w)
                  }}
                >
                  ✕
                </span>
              ) : null}
            </button>
          )
        })}
      </div>
      <div style={{ flex: 1 }} />
      {d?.update ? (
        <div className="pill up" onClick={onUpdate}>
          <kbd className="hint">u</kbd> ↑ update to v{d.update}
        </div>
      ) : null}
      {running ? (
        <div className="pill busy">
          <span>{`${running} agent${running > 1 ? 's' : ''} running`}</span>
        </div>
      ) : null}
      {d?.peers?.length ? (
        <div className="pill" title={d.peers.map((p) => `peer ${p.id}${p.auto ? ' · auto' : ''}`).join('\n')}>
          {d.peers.length} on network · {d.peers.filter((p) => p.auto).length} auto
        </div>
      ) : null}
      {d?.error ? (
        <div className="pill err" title={d.error}>
          ✗ refresh failed: {d.error.slice(0, 40)}
        </div>
      ) : null}
      <span className="ib" title="refresh now (f)" onClick={onRefresh}>
        {spinning ? <span className="spinner" /> : <span>⟳</span>}
        <kbd className="hint">f</kbd>
      </span>
      <div className={`toggle${d?.auto ? ' on' : ''}`} title="auto-run on new PRs (a)" onClick={onAuto}>
        <div className="track">
          <i />
        </div>
        <span>AUTO</span>
        <kbd className="hint">a</kbd>
      </div>
      <Pinata />
      <button className="ghost" title="keyboard shortcuts (?)" onClick={onHelp}>
        <kbd className="hint">?</kbd> shortcuts
      </button>
      <span className="ib" title="menu (esc)" onClick={onMenu}>
        ☰
        <kbd className="hint">esc</kbd>
      </span>
    </div>
  )
}
