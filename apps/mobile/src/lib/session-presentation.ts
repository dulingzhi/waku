import type {
  AgentSession,
  AgentTurn,
  Checkpoint,
  Message,
  Project,
  ProviderKind,
  RuntimeMode,
  TranscriptBlock,
} from '@waku/client';
import {
  turnActivityCount,
  turnAnswerStart,
  turnChangedFileCount,
  turnSummaryLabel,
} from '@waku/client/transcript-presentation';

import type { MarkdownBlock } from '../md/parse';
import { TranscriptMarkdownCache } from '../md/transcript-cache';

export type SessionGroupId = 'today' | 'yesterday' | 'week' | 'month' | 'year' | 'more';

export interface SessionListItem {
  session: AgentSession;
  projectName: string;
  timestamp: number;
}

export interface SessionGroup {
  id: SessionGroupId;
  title: string;
  data: SessionListItem[];
}

/**
 * One row = one markdown top-level block / user bubble / tool group / chip,
 * never one assistant message. A streamed
 * token re-renders exactly one row (the live tail block), and the list only
 * re-lays-out what changed. `topGap` is resolved at build time because it
 * depends on the previous row. `turnId` lets the list tell the running
 * turn's rows — the ones that grow in place while streaming — from settled
 * history.
 */
export type TranscriptRow =
  | { kind: 'user'; key: string; turnId: string | null; message: Message; topGap: number }
  | { kind: 'system'; key: string; turnId: string | null; message: Message; topGap: number }
  | {
      kind: 'md';
      key: string;
      turnId: string | null;
      messageId: string;
      blockIx: number;
      source: string;
      node: MarkdownBlock['node'];
      /** True only for the last block of the streaming message: it renders
       * the mended display tail with the veil, at display cadence. */
      live: boolean;
      /** True for every block of a message that is still streaming — freshly
       * minted settled rows fade in on mount while this holds. */
      streaming: boolean;
      /** True for the message's final block: the one that carries the footer
       * timestamp and the per-response actions. */
      last: boolean;
      footerTimestamp: number | null;
      topGap: number;
    }
  | {
      kind: 'activities';
      key: string;
      turnId: string | null;
      block: TranscriptBlock;
      /** Position in `session.transcript_blocks`: the sheet's locator hint. */
      blockIndex: number;
      live: boolean;
      topGap: number;
    }
  | {
      kind: 'collapsed';
      key: string;
      turnId: string | null;
      turn: AgentTurn;
      label: string;
      steps: number;
      files: number;
      topGap: number;
    }
  | { kind: 'changed'; key: string; turnId: string | null; checkpoint: Checkpoint; topGap: number };

/** Spacing tokens: first row, message boundaries, tool boundaries,
 * sibling markdown blocks of one message. */
const GAP_FIRST = 26;
const GAP_TURN = 16;
const GAP_GROUP = 12;
const GAP_BLOCK = 12;

const GROUP_LABEL_KEYS: Record<SessionGroupId, string> = {
  today: 'sidebar.today',
  yesterday: 'sidebar.yesterday',
  week: 'sidebar.this_week',
  month: 'sidebar.this_month',
  year: 'sidebar.this_year',
  more: 'sidebar.more',
};

const GROUPS: Array<{ id: SessionGroupId; title: string }> = [
  { id: 'today', title: 'Today' },
  { id: 'yesterday', title: 'Yesterday' },
  { id: 'week', title: 'This Week' },
  { id: 'month', title: 'This Month' },
  { id: 'year', title: 'This Year' },
  { id: 'more', title: 'More' },
];

/** Translation is optional so these builders stay usable without a provider;
 * the English titles above are the fallback. */
export type Translator = (key: string, params?: Record<string, string | number>) => string;

function groupTitle(id: SessionGroupId, t?: Translator): string {
  if (t) return t(GROUP_LABEL_KEYS[id]);
  return GROUPS.find((group) => group.id === id)?.title ?? id;
}

export function displaySessionTitle(session: AgentSession, fallback?: string): string {
  if (session.title !== 'New task' && session.title.trim()) return session.title.trim();
  return session.auto_title?.trim() || fallback || 'New Task';
}

export function sessionHasStarted(session: AgentSession): boolean {
  return Boolean(
    session.turns.length || session.messages.length || session.provider_cursor || session.last_reply_at,
  );
}

export function sessionTimestamp(session: AgentSession): number {
  // Match desktop: a submitted/replied turn promotes the task, while metadata
  // edits such as rename leave it in its existing date group.
  return session.last_reply_at ?? session.created_at;
}

export function groupSessions(
  projects: Project[],
  sessions: AgentSession[],
  now = new Date(),
  t?: Translator,
): SessionGroup[] {
  const projectNames = new Map(projects.map((project) => [project.id, project.name]));
  const unknownProject = t ? t('sidebar.unknown_project') : 'Unknown project';
  const grouped = new Map<SessionGroupId, SessionListItem[]>();
  for (const session of sessions.filter(sessionHasStarted).sort((a, b) => (
    sessionTimestamp(b) - sessionTimestamp(a)
  ))) {
    const id = sessionDateGroup(sessionTimestamp(session), now);
    const items = grouped.get(id) ?? [];
    items.push({
      session,
      projectName: projectNames.get(session.project_id) || unknownProject,
      timestamp: sessionTimestamp(session),
    });
    grouped.set(id, items);
  }
  return GROUPS.flatMap((group) => {
    const data = grouped.get(group.id);
    return data?.length ? [{ ...group, title: groupTitle(group.id, t), data }] : [];
  });
}

export function sessionDateGroup(timestamp: number, now = new Date()): SessionGroupId {
  const start = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
  const day = new Date(timestamp * 1_000);
  const dayStart = new Date(day.getFullYear(), day.getMonth(), day.getDate()).getTime();
  if (dayStart >= start) return 'today';

  const yesterday = new Date(now.getFullYear(), now.getMonth(), now.getDate() - 1).getTime();
  if (dayStart === yesterday) return 'yesterday';

  const weekStart = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  weekStart.setDate(weekStart.getDate() - ((weekStart.getDay() + 6) % 7));
  if (dayStart >= weekStart.getTime()) return 'week';
  if (day.getFullYear() === now.getFullYear() && day.getMonth() === now.getMonth()) {
    return 'month';
  }
  if (day.getFullYear() === now.getFullYear()) return 'year';
  return 'more';
}

export function relativeSessionTime(
  timestamp: number,
  now = Date.now(),
  t?: Translator,
): string {
  const elapsed = Math.max(0, Math.floor(now / 1_000) - timestamp);
  if (t) {
    if (elapsed < 60) return t('sidebar.just_now');
    if (elapsed < 3_600) return t('sidebar.minutes_ago', { count: Math.floor(elapsed / 60) });
    if (elapsed < 86_400) return t('sidebar.hours_ago', { count: Math.floor(elapsed / 3_600) });
    return t('sidebar.days_ago', { count: Math.floor(elapsed / 86_400) });
  }
  if (elapsed < 60) return 'just now';
  if (elapsed < 3_600) return `${Math.floor(elapsed / 60)}m`;
  if (elapsed < 86_400) return `${Math.floor(elapsed / 3_600)}h`;
  return `${Math.floor(elapsed / 86_400)}d`;
}

export function providerLabel(provider: ProviderKind): string {
  const labels: Record<ProviderKind, string> = {
    amp: 'Amp',
    claude: 'Claude',
    codex: 'Codex',
    cursor: 'Cursor',
    deepSeek: 'DeepSeek',
    fx: 'Fx',
    openCode: 'OpenCode',
    grok: 'Grok',
    kimi: 'Kimi',
    ohMyPi: 'Oh My Pi',
    pi: 'Pi',
  };
  return labels[provider];
}

/**
 * Providers whose daemon can roll a native conversation back to a turn, and
 * separately fork one. Mirrors `ProviderKind::supports_conversation_rollback`
 * and `supports_conversation_fork` in `crates/waku-protocol`: Kimi and Fx are
 * absent from both because neither can reproduce Waku's turn-count
 * semantics. The two capabilities currently accept the same providers, but
 * they are distinct in the protocol, so they keep distinct predicates.
 */
const PROVIDERS_WITH_ROLLBACK: ReadonlySet<ProviderKind> = new Set<ProviderKind>([
  'amp', 'claude', 'codex', 'cursor', 'deepSeek', 'openCode', 'grok', 'ohMyPi', 'pi',
]);

export function providerSupportsRollback(provider: ProviderKind): boolean {
  return PROVIDERS_WITH_ROLLBACK.has(provider);
}

export function providerSupportsFork(provider: ProviderKind): boolean {
  return PROVIDERS_WITH_ROLLBACK.has(provider);
}

/**
 * The turn count a fork of this response would carry, or null when the
 * response cannot be forked.
 *
 * The daemon's `validate_response_fork` accepts a turn count whose turn the
 * provider actually started, on a session that is not mid-turn and whose
 * provider cursor belongs to the same provider. It does not require a
 * checkpoint ref: a fork that cannot copy the refs still succeeds and reports
 * `checkpointWarning`, so gating on checkpoints here would hide a working
 * action.
 */
export function responseForkTurnCount(
  session: AgentSession,
  message: Message,
): number | null {
  if (
    message.role !== 'assistant'
    || message.streaming
    || (session.status !== 'idle' && session.status !== 'failed')
    || !session.provider_cursor
    || session.provider_cursor.provider !== session.provider
    || !providerSupportsFork(session.provider)
  ) {
    return null;
  }
  const turn = message.turn_id
    ? session.turns.find((candidate) => candidate.id === message.turn_id)
    : undefined;
  if (!turn?.provider_turn_started) return null;
  return turn.turn_count;
}

/**
 * The turn count a rewind from this user message would pass to the daemon, or
 * null when the message cannot open a rewind.
 *
 * The daemon reads that value as the turn being rewound from and keeps
 * `turn_count - 1` turns, so this returns the message's own turn count while
 * gating on whether the *retained* count still has a checkpoint ref — the
 * restore needs that ref to put the worktree back.
 *
 * Only the prompt that opened a turn is a rewind boundary: a steer joins the
 * running turn as another user message, and checkpoints are per turn, so
 * rewinding from a steer would resubmit the steer text in place of the prompt
 * that actually opened the turn.
 */
export function userMessageRewindTurnCount(
  session: AgentSession,
  message: Message,
  availableRetainedTurnCounts: ReadonlySet<number>,
): number | null {
  if (
    message.role !== 'user'
    || (session.status !== 'idle' && session.status !== 'failed')
    || !providerSupportsRollback(session.provider)
  ) {
    return null;
  }
  const turn = message.turn_id
    ? session.turns.find((candidate) => candidate.id === message.turn_id)
    : undefined;
  if (!turn) return null;
  // Turn messages are contiguous, so checking the immediately preceding
  // message is enough to tell the opening prompt from a later steer.
  const index = session.messages.findIndex((candidate) => candidate.id === message.id);
  if (index < 0) return null;
  const previous = session.messages[index - 1];
  if (previous?.turn_id === turn.id) return null;
  const retainedTurnCount = Math.max(0, turn.turn_count - 1);
  if (!availableRetainedTurnCounts.has(retainedTurnCount)) return null;
  const rollbackTurns = session.turns
    .slice(retainedTurnCount)
    .filter((candidate) => candidate.provider_turn_started)
    .length;
  if (rollbackTurns > 0 && !session.provider_cursor) return null;
  return turn.turn_count;
}

export function runtimeModeLabel(mode: RuntimeMode, t?: Translator): string {
  if (t) {
    const keys: Record<RuntimeMode, string> = {
      ask: 'mode.supervised',
      autoAcceptEdits: 'mode.auto_accept_edits',
      auto: 'mode.auto',
      fullAccess: 'mode.full_access',
    };
    return t(keys[mode]);
  }
  const labels: Record<RuntimeMode, string> = {
    ask: 'Ask first',
    autoAcceptEdits: 'Accept edits',
    auto: 'Auto',
    fullAccess: 'Full access',
  };
  return labels[mode];
}

export function contextPercent(session: AgentSession): number | null {
  const usage = session.context_usage;
  if (!usage || !usage.window) return null;
  return Math.max(0, Math.min(100, Math.round((usage.tokens / usage.window) * 100)));
}

/**
 * Locator for one tool group, as handed to the activity sheet. Every stream
 * commit deep-clones the session, so the sheet keeps this rather than the
 * block and re-resolves it against the freshest session on each render. The
 * index is a hint checked against the anchor; a block that moved (a rewind
 * dropped an earlier turn) is found again by its anchor.
 */
export interface ActivityGroupTarget {
  blockIndex: number;
  turnId: string | null;
  afterMessage: number;
}

export function findActivityBlock(
  session: AgentSession,
  target: ActivityGroupTarget,
): TranscriptBlock | null {
  const matches = (block: TranscriptBlock) =>
    block.turn_id === target.turnId && block.after_message === target.afterMessage;
  const hinted = session.transcript_blocks[target.blockIndex];
  if (hinted && matches(hinted)) return hinted;
  return session.transcript_blocks.find(matches) ?? null;
}

/**
 * Message-granular pipeline rows — the cheap, parse-free skeleton of the
 * transcript. Assistant messages expand into `md` block rows only for the
 * slice that is actually rendered (see `expandTranscriptRows`), so opening a
 * five-thousand-message session costs one linear pass, not a markdown parse
 * of every answer.
 */
export type TranscriptPipelineRow =
  | { kind: 'message'; key: string; turnId: string | null; message: Message; footerTimestamp: number | null }
  | { kind: 'activities'; key: string; turnId: string | null; block: TranscriptBlock; blockIndex: number; live: boolean }
  | { kind: 'collapsed'; key: string; turnId: string | null; turn: AgentTurn; label: string; steps: number; files: number }
  | { kind: 'changed'; key: string; turnId: string | null; checkpoint: Checkpoint };

interface TaggedRow {
  row: TranscriptPipelineRow;
  turnId: string | null;
  foldable: boolean;
  answerText: boolean;
}

/**
 * Interleaves messages with activity blocks. Nothing is hidden by default:
 * every thought, tool row and text part a turn produced stays in the pipeline,
 * in the order it happened.
 *
 * A turn in `collapsedTurns` is the exception, and only because the reader
 * asked for it: all of its rows go behind one summary row.
 *
 * Linear in messages + blocks + turns: a commit lands at up to ~8 Hz and the
 * largest real sessions carry thousands of messages.
 */
export function buildTranscriptPipeline(
  session: AgentSession,
  collapsedTurns: ReadonlySet<string> = new Set(),
  /**
   * Turns whose rows must not be mounted, because the list is already holding
   * the render budget's worth of them. They become one placeholder each.
   *
   * Not a fold: the budget only limits how much is mounted at once on a phone
   * with no virtualized list, and a turn leaves the set as soon as the reader
   * scrolls back to it.
   */
  boundedTurns: ReadonlySet<string> = new Set(),
): TranscriptPipelineRow[] {
  const runningTurnId = session.turns.find((turn) => turn.status === 'running')?.id ?? null;
  const latestBlock = session.transcript_blocks.at(-1);

  const blocksByAnchor = new Map<number, Array<{ block: TranscriptBlock; index: number }>>();
  session.transcript_blocks.forEach((block, index) => {
    const bucket = blocksByAnchor.get(block.after_message);
    if (bucket) bucket.push({ block, index });
    else blocksByAnchor.set(block.after_message, [{ block, index }]);
  });

  const tagged: TaggedRow[] = [];
  const foldableByTurn = new Map<string, TaggedRow[]>();
  const lastRowByTurn = new Map<string, TaggedRow>();
  const tag = (item: TaggedRow) => {
    tagged.push(item);
    if (!item.turnId) return;
    lastRowByTurn.set(item.turnId, item);
    if (!item.foldable) return;
    const bucket = foldableByTurn.get(item.turnId);
    if (bucket) bucket.push(item);
    else foldableByTurn.set(item.turnId, [item]);
  };
  for (let messageIndex = 0; messageIndex <= session.messages.length; messageIndex += 1) {
    for (const { block, index } of blocksByAnchor.get(messageIndex) ?? []) {
      tag({
        row: {
          kind: 'activities',
          key: `activity:${index}:${block.turn_id ?? 'none'}:${block.after_message}`,
          turnId: block.turn_id,
          block,
          blockIndex: index,
          live: Boolean(
            runningTurnId && block.turn_id === runningTurnId && block === latestBlock &&
              block.after_message === session.messages.length,
          ),
        },
        turnId: block.turn_id,
        foldable: true,
        answerText: false,
      });
    }
    const message = session.messages[messageIndex];
    if (message) {
      tag({
        row: {
          kind: 'message',
          key: `message:${message.id}`,
          turnId: message.turn_id,
          message,
          footerTimestamp: null,
        },
        turnId: message.turn_id,
        foldable: message.role === 'assistant',
        answerText: message.role === 'assistant' && Boolean(message.content.trim()),
      });
    }
  }

  // Per collapsed turn: which rows it hides and where its summary anchors.
  // Nothing folds by default — a turn appears here only because the reader
  // collapsed it.
  const hidden = new Set<TaggedRow>();
  const anchors = new Map<TaggedRow, AgentTurn>();
  if (collapsedTurns.size) {
    for (const turn of session.turns) {
      if (!collapsedTurns.has(turn.id)) continue;
      const turnRows = foldableByTurn.get(turn.id);
      if (!turnRows?.length) continue;
      anchors.set(turnRows[0]!, turn);
      for (const item of turnRows) hidden.add(item);
    }
  }

  const turnsById = new Map(session.turns.map((turn) => [turn.id, turn]));
  const rows: TranscriptPipelineRow[] = [];
  const answerRowByTurn = new Map<string, Extract<TranscriptPipelineRow, { kind: 'message' }>>();
  const placeholderEmitted = new Set<string>();
  for (const item of tagged) {
    const anchorTurn = anchors.get(item);
    if (anchorTurn) {
      rows.push({
        kind: 'collapsed',
        key: `collapsed:${anchorTurn.id}`,
        turnId: anchorTurn.id,
        turn: anchorTurn,
        label: turnSummaryLabel(anchorTurn),
        steps: turnActivityCount(session, anchorTurn.id),
        files: turnChangedFileCount(session, anchorTurn.id),
      });
    }
    // A turn outside the render budget becomes one placeholder row the reader
    // can open. This is a limit on what is mounted, not a fold: the turn is
    // shown in full as soon as it enters the budget, and the placeholder says
    // how much it is standing in for.
    if (item.turnId && boundedTurns.has(item.turnId)) {
      if (!placeholderEmitted.has(item.turnId)) {
        placeholderEmitted.add(item.turnId);
        const turn = turnsById.get(item.turnId);
        if (turn) {
          rows.push({
            kind: 'collapsed',
            key: `placeholder:${turn.id}`,
            turnId: turn.id,
            turn,
            label: turnSummaryLabel(turn),
            steps: turnActivityCount(session, turn.id),
            files: turnChangedFileCount(session, turn.id),
          });
        }
      }
      continue;
    }
    if (!hidden.has(item)) {
      rows.push(item.row);
      if (item.answerText && item.row.kind === 'message' && item.turnId) {
        answerRowByTurn.set(item.turnId, item.row);
      }
    }
    if (item.turnId && lastRowByTurn.get(item.turnId) === item) {
      const turn = turnsById.get(item.turnId);
      if (turn && turn.status !== 'running' && turn.checkpoint?.files.length) {
        rows.push({ kind: 'changed', key: `changed:${turn.id}`, turnId: turn.id, checkpoint: turn.checkpoint });
      }
    }
  }
  for (const [turnId, row] of answerRowByTurn) {
    const turn = turnsById.get(turnId);
    if (turn?.completed_at && !row.message.streaming) row.footerTimestamp = turn.completed_at;
  }
  return rows;
}

/**
 * Turns kept in full on a phone before older ones become placeholders.
 *
 * Measured against the mounted window, not the session: twenty turns of
 * five-activity work is about the most that can stay mounted and scroll at
 * native speed without a virtualized list.
 */
export const MOBILE_FULL_TURN_BUDGET = 20;

/**
 * Turns the render budget cannot afford to mount in full, oldest first.
 *
 * A phone has no virtualized transcript list: everything inside the mounted
 * window stays mounted. With work visible by default a long session is far
 * more rows than before, so a session past `maxTurns` keeps its most recent
 * turns in full and stands each older one in for a single tappable placeholder.
 *
 * The reader's own actions always win: a turn they collapsed or explicitly
 * opened is never budgeted away, and neither is the turn that is running.
 */
export function budgetedTurns(
  session: AgentSession,
  options: { maxTurns: number; keep: ReadonlySet<string> } = {
    maxTurns: MOBILE_FULL_TURN_BUDGET,
    keep: new Set(),
  },
): Set<string> {
  const running = session.turns.find((turn) => turn.status === 'running')?.id;
  const eligible = session.turns.filter((turn) => (
    turn.id !== running && !options.keep.has(turn.id)
  ));
  const excess = eligible.length - options.maxTurns;
  if (excess <= 0) return new Set();
  return new Set(eligible.slice(0, excess).map((turn) => turn.id));
}

/** Full expansion — every pipeline row into block rows. Tests and small
 * transcripts; the list expands only its rendered tail. */
export function buildTranscriptRows(
  session: AgentSession,
  collapsedTurns: ReadonlySet<string> = new Set(),
  md: TranscriptMarkdownCache = new TranscriptMarkdownCache(),
  boundedTurns: ReadonlySet<string> = new Set(),
): TranscriptRow[] {
  return expandTranscriptRows(
    buildTranscriptPipeline(session, collapsedTurns, boundedTurns),
    md,
    0,
  );
}

/**
 * Expand pipeline rows from index `start` to the end into render rows, and
 * resolve leading gaps. The gap of the first expanded row is measured against
 * the nearest earlier row so a windowed tail lays out exactly as the full
 * transcript would.
 */
export function expandTranscriptRows(
  pipeline: readonly TranscriptPipelineRow[],
  md: TranscriptMarkdownCache,
  start = 0,
): TranscriptRow[] {
  const from = Math.max(0, Math.min(start, pipeline.length));
  const out: TranscriptRow[] = [];
  const liveIds = new Set<string>();
  for (let ix = from; ix < pipeline.length; ix += 1) {
    expandPipelineRow(pipeline[ix]!, md, liveIds, out);
  }

  // The nearest earlier row sets the first gap (an assistant message with
  // no blocks yields no row, so keep looking back).
  let previous: TranscriptRow | undefined;
  for (let ix = from - 1; ix >= 0 && !previous; ix -= 1) {
    const expanded: TranscriptRow[] = [];
    expandPipelineRow(pipeline[ix]!, md, liveIds, expanded);
    previous = expanded[expanded.length - 1];
  }
  md.prune(liveIds);

  for (const row of out) {
    row.topGap = topGap(row, previous);
    previous = row;
  }
  return out;
}

function expandPipelineRow(
  row: TranscriptPipelineRow,
  md: TranscriptMarkdownCache,
  liveIds: Set<string>,
  out: TranscriptRow[],
) {
  if (row.kind !== 'message') {
    out.push({ ...row, topGap: 0 });
    return;
  }
  const message = row.message;
  if (message.role === 'user') {
    out.push({ kind: 'user', key: `user:${message.id}`, turnId: row.turnId, message, topGap: 0 });
    return;
  }
  if (message.role === 'system') {
    out.push({ kind: 'system', key: `system:${message.id}`, turnId: row.turnId, message, topGap: 0 });
    return;
  }
  liveIds.add(message.id);
  const streaming = message.streaming === true;
  const blocks = md.blocksFor(message.id, message.content, streaming);
  for (let ix = 0; ix < blocks.length; ix += 1) {
    const block = blocks[ix]!;
    const last = ix === blocks.length - 1;
    out.push({
      kind: 'md',
      key: `md:${message.id}.${ix}`,
      turnId: row.turnId,
      messageId: message.id,
      blockIx: ix,
      source: block.source,
      node: block.node,
      live: streaming && last,
      streaming,
      last,
      footerTimestamp: last ? row.footerTimestamp : null,
      topGap: 0,
    });
  }
}

function topGap(row: TranscriptRow, previous: TranscriptRow | undefined): number {
  if (!previous) return GAP_FIRST;
  if (row.kind === 'md' && previous.kind === 'md' && previous.messageId === row.messageId) {
    return GAP_BLOCK;
  }
  // Tool stacks, folds, and change cards are denser than prose — one group
  // step on both boundaries, matching the desktop.
  if (row.kind !== 'user' && row.kind !== 'system' && row.kind !== 'md') return GAP_GROUP;
  if (previous.kind !== 'user' && previous.kind !== 'system' && previous.kind !== 'md') {
    return GAP_GROUP;
  }
  return GAP_TURN;
}

/**
 * Reuse the previous render's row objects wherever nothing about a row
 * changed, so memoized row views bail out and a stream commit re-renders only
 * the live tail. Keys are unique per transcript, so the merge is one map
 * lookup per row.
 */
export function stabilizeTranscriptRows(
  previous: readonly TranscriptRow[],
  next: readonly TranscriptRow[],
): TranscriptRow[] {
  if (!previous.length) return [...next];
  const byKey = new Map<string, TranscriptRow>();
  for (const row of previous) byKey.set(row.key, row);
  let changed = previous.length !== next.length;
  const out = next.map((row, index) => {
    const before = byKey.get(row.key);
    if (before && shallowEqualRow(before, row)) {
      if (previous[index] !== before) changed = true;
      return before;
    }
    changed = true;
    return row;
  });
  return changed ? out : (previous as TranscriptRow[]);
}

/** Field-wise equality. Markdown rows compare by `source`, not node
 * identity: the incremental parser re-creates the last two block nodes on
 * every append, and equal source parses to an equivalent tree. */
function shallowEqualRow(a: TranscriptRow, b: TranscriptRow): boolean {
  if (a === b) return true;
  const left = a as unknown as Record<string, unknown>;
  const right = b as unknown as Record<string, unknown>;
  const keys = Object.keys(left);
  if (keys.length !== Object.keys(right).length) return false;
  for (const key of keys) {
    if (key === 'node' && a.kind === 'md') continue;
    if (left[key] !== right[key]) return false;
  }
  return true;
}
