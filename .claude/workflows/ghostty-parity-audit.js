export const meta = {
  name: 'ghostty-parity-audit',
  description: 'Diff geist against upstream Ghostty (config keys, actions, app features, C API), verify each gap against src/, and write a phased parity plan',
  whenToUse: 'Periodically, or after bumping the vendored Ghostty commit, to refresh GAP.md and plan the next parity work. args: { date: "YYYY-MM-DD", ref?: "pinned" | "main" | <sha>, since?: <sha of last audit>, previous?: <last plan path>, out?: <plan path>, updateGap?: bool }',
  phases: [
    { title: 'Baseline', detail: 'locate the Ghostty source at the requested ref and the last audit point' },
    { title: 'Diff', detail: 'one agent per upstream surface: extract, then classify against src/' },
    { title: 'Verify', detail: 'adversarially re-check every OPEN claim and GAP.md staleness' },
    { title: 'Plan', detail: 'synthesize a phased plan (and optionally update GAP.md)' },
  ],
}

const A = args || {}
const DATE = A.date || 'undated'
const REF = A.ref || 'pinned'
const OUT = A.out || `docs/parity-plan-${DATE}.md`
const UPDATE_GAP = A.updateGap === true
const PREVIOUS = A.previous || '' // path of the last parity plan, e.g. docs/parity-plan-2026-09-26.md
// Resume note: the cache replays only the unchanged *prefix* of agent() calls. The GAP.md scan is the
// second call, so editing its prompt re-runs every diff/verify agent too. Edit the plan prompt freely.

// ---------- schemas ----------
const BASELINE = {
  type: 'object',
  properties: {
    ghosttySrc: { type: 'string', description: 'absolute path to a Ghostty checkout at the audited ref' },
    ghosttyCommit: { type: 'string' },
    pinnedCommit: { type: 'string', description: 'GHOSTTY_COMMIT from libghostty-vt-sys/build.rs' },
    sinceCommit: { type: 'string', description: 'upstream commit of the previous audit (from args or GAP.md), or empty' },
    bindingsPath: { type: 'string' },
    notes: { type: 'string' },
  },
  required: ['ghosttySrc', 'ghosttyCommit', 'pinnedCommit', 'sinceCommit', 'bindingsPath', 'notes'],
}

const ITEMS = {
  type: 'object',
  properties: {
    upstreamCount: { type: 'integer', description: 'total items in this upstream surface' },
    items: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          name: { type: 'string' },
          status: { type: 'string', enum: ['DONE', 'OPEN', 'PARTIAL', 'BLOCKED', 'NA'] },
          evidence: { type: 'string', description: 'geist file:line, upstream file:line, or why N/A/blocked' },
          effort: { type: 'string', enum: ['S', 'M', 'L', 'XL', '-'] },
          files: { type: 'string', description: 'geist files a fix would touch' },
          gapMd: { type: 'string', description: 'what GAP.md currently says about it (line), or "absent"' },
        },
        required: ['name', 'status', 'evidence', 'effort', 'files', 'gapMd'],
      },
    },
  },
  required: ['upstreamCount', 'items'],
}

const VERDICTS = {
  type: 'object',
  properties: {
    verdicts: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          name: { type: 'string' },
          confirmed: { type: 'boolean', description: 'true = the claimed status holds after trying to refute it' },
          correctedStatus: { type: 'string', enum: ['DONE', 'OPEN', 'PARTIAL', 'BLOCKED', 'NA'] },
          evidence: { type: 'string' },
        },
        required: ['name', 'confirmed', 'correctedStatus', 'evidence'],
      },
    },
  },
  required: ['verdicts'],
}

const STALE = {
  type: 'object',
  properties: {
    stale: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          line: { type: 'integer' },
          claim: { type: 'string' },
          reality: { type: 'string' },
          evidence: { type: 'string' },
        },
        required: ['line', 'claim', 'reality', 'evidence'],
      },
    },
    stillOpen: {
      type: 'array',
      description: 'GAP.md items (often geist-internal follow-ups no upstream surface lists) that src/ confirms are still open',
      items: {
        type: 'object',
        properties: {
          line: { type: 'integer' },
          item: { type: 'string' },
          evidence: { type: 'string' },
          effort: { type: 'string', enum: ['S', 'M', 'L', 'XL'] },
          files: { type: 'string' },
        },
        required: ['line', 'item', 'evidence', 'effort', 'files'],
      },
    },
  },
  required: ['stale', 'stillOpen'],
}

// ---------- Baseline ----------
phase('Baseline')
const base = await agent(
  `Locate the upstream Ghostty source to audit geist against. Requested ref: "${REF}".
- "pinned": read GHOSTTY_COMMIT in vendor/libghostty-rs/crates/libghostty-vt-sys/build.rs and find a
  target/*/build/libghostty-vt-sys-*/out/ghostty-src checkout at that commit (several may exist; check
  each with git rev-parse HEAD, or a version marker if it is not a git checkout).
- "main" or a sha: shallow-clone/fetch ghostty-org/ghostty at that ref into the session scratchpad
  (never into the repo) and use that.
sinceCommit: ${A.since ? `use "${A.since}"` : 'read the most recent "re-audited" note at the top of GAP.md for the upstream commit it audited; empty if none'}.
bindingsPath: the vendored libghostty-vt-sys bindings.rs. Read-only apart from the scratchpad clone.`,
  { label: 'baseline', schema: BASELINE, effort: 'low' },
)
if (!base) return { error: 'baseline failed' }
log(`Auditing against Ghostty ${base.ghosttyCommit} (pinned ${base.pinnedCommit}; since ${base.sinceCommit || 'n/a'})`)

const CTX = `Upstream Ghostty source: ${base.ghosttySrc} @ ${base.ghosttyCommit}. geist pins ${base.pinnedCommit}.
geist is a Windows port: macOS-/GTK-/Linux-only features are NA unless a Windows analogue exists
(GAP.md records analogues such as winchrome.rs, taskbar.rs, jumplist.rs — treat those as DONE).
Classify each item: DONE (implemented in geist src/ — cite file:line), PARTIAL, OPEN, BLOCKED (needs a
C API export absent from ${base.bindingsPath}, or an external dependency — say which), NA.
GAP.md is known to be stale — never trust its status; check src/. Record what GAP.md says in gapMd.`

// ---------- Diff: one agent per upstream surface, each verified as soon as it lands ----------
const SURFACES = [
  { key: 'config', prompt: `Extract every public config key from src/config/Config.zig (skip _-prefixed fields). For each, check geist's src/config.rs SETTERS table and special cases (e.g. theme). Report only non-DONE items in detail, but set upstreamCount to the full key count and include DONE items with brief evidence so counts can be reconciled.` },
  { key: 'actions', prompt: `Extract every member of the keybind Action union in src/input/Binding.zig. For each, check geist's src/command.rs (Action::name/from_name) and where it is executed. Include DONE items with brief evidence; set upstreamCount to the full union size.` },
  { key: 'app', prompt: `Survey user-facing app-level features that are neither config keys nor actions: macos/Sources (windows, tabs, splits, menus, notifications, accessibility, IME, drag-and-drop, update, quick terminal, inspector, etc.)${base.sinceCommit ? ` plus every user-facing commit in git log ${base.sinceCommit}..${base.ghosttyCommit}` : ''}. Map each to geist; list only features with a sensible Windows shape, marking the rest NA.` },
  { key: 'protocol', prompt: `Survey VT/protocol features (OSC/CSI/DCS/APC families, kitty graphics/keyboard/clipboard/notifications, modes) in src/terminal and the C API in include/ghostty/vt*.h. Compare with geist's engine use (src/engine/, side-scanners like osc_color.rs, osc_notify.rs, osc133.rs). For every item GAP.md lists as BLOCKED on the C API, check whether ${base.bindingsPath} or the upstream headers at this ref now export what it needs — an unblocked item is OPEN.` },
]

phase('Diff')
// GAP.md staleness is independent of the surface diffs, so start it first and await it after them.
const staleP = agent(
  `Scan GAP.md for every row/bullet marked ⬜ or ◐, or phrased "not done", "still open", "deferred", "missing",
"not implemented", "limitation", "divergence … not yet", "needs a human". For each, check src/ and sort it:
- stale: actually DONE now — GAP.md line + src file:line evidence;
- stillOpen: genuinely still open AND actionable in geist (skip pure "needs eyeballing" and items blocked on
  the C API or Windows) — include geist-internal follow-ups (e.g. config setters that don't report bad
  values, per-window HWND gaps, packaging/LICENSE, font/sprite leftovers), since no upstream surface lists them.`,
  { label: 'gap-staleness', phase: 'Verify', schema: STALE },
)
const surfaces = await pipeline(
  SURFACES,
  s => agent(`${CTX}\n\n${s.prompt}`, { label: `diff:${s.key}`, phase: 'Diff', schema: ITEMS }),
  (res, s) => {
    if (!res) return null
    const claims = res.items.filter(i => i.status !== 'DONE' && i.status !== 'NA')
    if (!claims.length) return { key: s.key, upstreamCount: res.upstreamCount, items: res.items, verdicts: [] }
    return agent(
      `${CTX}\n\nAnother agent claims these ${s.key} items are not done in geist. Try hard to REFUTE each:
search src/ (and tests/, vendor/ patches) for an implementation under any name; for BLOCKED, check whether
the C API really lacks it. Default confirmed=true only if you searched and found nothing.\n\n` +
        JSON.stringify(claims, null, 1),
      { label: `verify:${s.key}`, phase: 'Verify', schema: VERDICTS },
    ).then(v => ({ key: s.key, upstreamCount: res.upstreamCount, items: res.items, verdicts: v ? v.verdicts : [] }))
  },
)

const stale = await staleP

// ---------- merge (plain code) ----------
const rows = []
for (const s of surfaces.filter(Boolean)) {
  const byName = new Map(s.verdicts.map(v => [v.name, v]))
  for (const i of s.items) {
    const v = byName.get(i.name)
    const status = v && !v.confirmed ? v.correctedStatus : i.status
    rows.push({ surface: s.key, ...i, status, evidence: v && !v.confirmed ? v.evidence : i.evidence })
  }
}
const dropped = SURFACES.length - surfaces.filter(Boolean).length
if (dropped) log(`WARNING: ${dropped} surface(s) failed and are missing from the plan`)
const counts = {}
for (const s of surfaces.filter(Boolean)) {
  const done = rows.filter(r => r.surface === s.key && (r.status === 'DONE')).length
  counts[s.key] = `${done} done of ${s.upstreamCount}`
}
const open = rows.filter(r => r.status === 'OPEN' || r.status === 'PARTIAL')
const blocked = rows.filter(r => r.status === 'BLOCKED')
log(`${open.length} open/partial, ${blocked.length} blocked, ${(stale && stale.stale.length) || 0} stale GAP.md claims, ${(stale && stale.stillOpen.length) || 0} still-open GAP.md items`)

// ---------- Plan ----------
phase('Plan')
const plan = await agent(
  `Write a phased parity plan to ${OUT} (overwrite). Audit date ${DATE}; upstream ${base.ghosttyCommit}; pinned ${base.pinnedCommit}.
Structure: Context (scoreboard: ${JSON.stringify(counts)}); Phase 0 = GAP.md truth pass (the stale list below);
then phases of OPEN/PARTIAL items grouped by shared mechanism and ordered by value/effort (S before XL, items
that unlock others first); a Blocked list with exactly what would unblock each; a Human-only list; a
Verification section that follows CLAUDE.md (cargo test, clippy -D warnings, cargo build before manual
checks, conpty_passthrough probe first for protocol work, human visual confirmation for render/*).
Name files and existing helpers to reuse for each item. Keep it scannable.
${PREVIOUS ? `Runs are not deterministic, so reconcile against the previous plan ${PREVIOUS} (read it): every item in it must either
appear in the new plan or be listed under a final "Dropped since ${PREVIOUS}" section with src/ evidence that it is DONE or
a reason it is wrong. Nothing may silently disappear. If ${PREVIOUS} is the same path as ${OUT}, read it BEFORE overwriting.` : ''}
${UPDATE_GAP ? `Then update GAP.md: fix every stale claim below, refresh the scoreboard and the "re-audited" date/commit at the top, and replace §E "Order of attack" with the plan's phases. Do not touch ledger prose beyond status words and the listed lines.` : 'Do NOT edit GAP.md.'}
Return a 10-line summary: path written, scoreboard, counts per phase, top 5 next items.

STALE GAP.md CLAIMS:\n${JSON.stringify(stale ? stale.stale : [], null, 1)}

OPEN/PARTIAL:\n${JSON.stringify(open, null, 1)}

STILL-OPEN GAP.md ITEMS (geist-internal; place every one in a phase unless it duplicates an OPEN row above — say which):\n${JSON.stringify(stale ? stale.stillOpen : [], null, 1)}

BLOCKED:\n${JSON.stringify(blocked, null, 1)}`,
  { label: 'plan', phase: 'Plan' },
)

return { plan: OUT, upstream: base.ghosttyCommit, counts, open: open.length, blocked: blocked.length, stale: stale ? stale.stale.length : null, stillOpen: stale ? stale.stillOpen.length : null, summary: plan }
