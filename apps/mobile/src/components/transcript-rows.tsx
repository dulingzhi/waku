import type { AgentSession, Checkpoint, Message } from '@waku/client';
import { formatMessageTime } from '@waku/client/transcript-presentation';
import * as Clipboard from 'expo-clipboard';
import * as Haptics from 'expo-haptics';
import { memo, useEffect, useState } from 'react';
import { ActivityIndicator, Pressable, StyleSheet, Text, View } from 'react-native';

import { ActivityGroup } from '@/components/activity-group';
import { AppSymbol } from '@/components/app-symbol';
import { AttachmentTile } from '@/components/attachment-tile';
import {
  MdBlockLive,
  MdBlockSettled,
  MdRevealOnMount,
  type VeilRegistry,
} from '@/components/md-block-row';
import { useRowAnchor } from '@/components/transcript-anchor';
import { MonoFont, Radius } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useI18n } from '@/lib/i18n-context';
import type { TranscriptRow } from '@/lib/session-presentation';
import type { MarkdownStyles } from '@/md/render';
import type { TranscriptMarkdownCache } from '@/md/transcript-cache';

/**
 * One transcript row's content. Memoized on its props: the list hands it the
 * same row object whenever nothing about the row changed, so a stream commit
 * re-renders exactly the live tail block.
 */
export const TranscriptRowView = memo(function TranscriptRowView({
  row,
  md,
  veils,
  seeded,
  markdownStyles,
  onToggleFold,
  forkTurnCount,
  forking,
  rewindTurnCount,
  rewinding,
  onForkResponse,
  onRewindMessage,
}: {
  row: TranscriptRow;
  md: TranscriptMarkdownCache;
  veils: VeilRegistry;
  seeded: boolean;
  markdownStyles: MarkdownStyles;
  onToggleFold: (turnId: string) => void;
  /** Set only on the assistant reply that can be forked from. */
  forkTurnCount?: number | null;
  forking?: boolean;
  /** Set only on the user prompt that can open a rewind. */
  rewindTurnCount?: number | null;
  rewinding?: boolean;
  onForkResponse?: (turnCount: number) => void;
  onRewindMessage?: (turnCount: number, prompt: string) => void;
}) {
  const theme = useTheme();
  const { t } = useI18n();
  switch (row.kind) {
    case 'user':
      return (
        <UserBubble
          message={row.message}
          onRewind={rewindTurnCount != null && onRewindMessage
            ? () => onRewindMessage(rewindTurnCount, row.message.display_content ?? row.message.content)
            : undefined}
          rewinding={rewinding ?? false}
          rewindLabel={t(rewinding ? 'session.reverting_message' : 'session.revert_to_here')}
        />
      );
    case 'system':
      return (
        <View style={styles.systemFrame}>
          <Text style={[styles.systemMessage, { backgroundColor: theme.overlay, color: theme.textTertiary }]}>
            {row.message.display_content ?? row.message.content}
          </Text>
        </View>
      );
    case 'md':
      return (
        <>
          {row.live ? (
            <MdBlockLive
              md={md}
              messageId={row.messageId}
              node={row.node}
              rowKey={row.key}
              seeded={seeded}
              source={row.source}
              styles={markdownStyles}
              veils={veils}
            />
          ) : (
            <MdRevealOnMount enabled={row.streaming && !seeded}>
              <MdBlockSettled node={row.node} source={row.source} styles={markdownStyles} />
            </MdRevealOnMount>
          )}
          {row.footerTimestamp != null && (
            <Text style={[styles.messageFooter, { color: theme.textGhost }]}>
              {formatMessageTime(row.footerTimestamp)}
            </Text>
          )}
          {forkTurnCount != null && row.last && onForkResponse && (
            <View style={styles.assistantActions}>
              <Pressable
                accessibilityLabel={t(forking ? 'session.forking_task' : 'session.fork_task')}
                accessibilityRole="button"
                accessibilityState={{ disabled: forking }}
                disabled={forking}
                hitSlop={10}
                onPress={() => onForkResponse(forkTurnCount)}
                style={({ pressed }) => [styles.forkButton, { opacity: pressed || forking ? 0.5 : 1 }]}>
                {forking
                  ? <ActivityIndicator size="small" />
                  : (
                      <AppSymbol
                        name={{ ios: 'arrow.triangle.branch', android: 'fork_right', web: 'fork_right' }}
                        size={13}
                        tintColor={theme.textTertiary}
                      />
                    )}
                <Text style={[styles.forkLabel, { color: theme.textTertiary }]}>
                  {t(forking ? 'session.forking_task_title' : 'session.fork_task_title')}
                </Text>
              </Pressable>
            </View>
          )}
        </>
      );
    case 'activities':
      return <ActivityGroup block={row.block} blockIndex={row.blockIndex} live={row.live} />;
    case 'collapsed':
      return (
        <CollapsedTurnRow
          files={row.files}
          label={row.label}
          reason={row.reason}
          steps={row.steps}
          turnId={row.turn.id}
          failed={row.turn.status === 'failed'}
          onToggle={onToggleFold}
        />
      );
    case 'changed':
      return <ChangedFilesCard checkpoint={row.checkpoint} />;
  }
});

/**
 * One collapsed turn: its summary standing in for the whole turn, and the way
 * back open. This row exists only because the reader collapsed the turn, so it
 * carries more weight than a tool row — a filled surface and a border — and
 * says out loud what is behind it.
 */
function CollapsedTurnRow({
  label,
  reason,
  steps,
  files,
  failed,
  turnId,
  onToggle,
}: {
  label: string;
  reason: string | null;
  steps: number;
  files: number;
  failed: boolean;
  turnId: string;
  onToggle: (turnId: string) => void;
}) {
  const theme = useTheme();
  const { t } = useI18n();
  const keepTop = useRowAnchor();
  const stats = [
    ...(steps > 0 ? [t('transcript.step_count', { count: steps })] : []),
    ...(files > 0 ? [t('transcript.file_count', { count: files })] : []),
  ].join(' · ');
  return (
    <View style={styles.collapsedStack}>
      <Pressable
        accessibilityHint={t('transcript.expand_turn')}
        accessibilityLabel={label}
        accessibilityRole="button"
        accessibilityState={{ expanded: false }}
        hitSlop={{ top: 6, bottom: 6 }}
        onPress={() => keepTop(() => onToggle(turnId))}
        style={({ pressed }) => [
          styles.collapsedRow,
          {
            backgroundColor: theme.overlay,
            borderColor: failed ? theme.danger : theme.borderStrong,
            opacity: pressed ? 0.7 : 1,
          },
        ]}>
        <AppSymbol
          name={failed
            ? { ios: 'exclamationmark.triangle', android: 'warning', web: 'warning' }
            : { ios: 'chevron.right', android: 'chevron_right', web: 'chevron_right' }}
          size={12}
          tintColor={failed ? theme.danger : theme.textTertiary}
        />
        <Text
          numberOfLines={1}
          style={[styles.collapsedLabel, { color: failed ? theme.danger : theme.textSecondary }]}>
          {label}
        </Text>
        {stats.length > 0 && (
          <Text numberOfLines={1} style={[styles.collapsedStats, { color: theme.textTertiary }]}>
            {stats}
          </Text>
        )}
        <Text numberOfLines={1} style={[styles.collapsedHint, { color: theme.textGhost }]}>
          {t('transcript.expand_turn')}
        </Text>
      </Pressable>
      {reason ? (
        <Text style={[styles.collapsedReason, { color: theme.textSecondary }]}>{reason}</Text>
      ) : null}
    </View>
  );
}
const UserBubble = memo(
  UserBubbleInner,
  (previous, next) =>
    previous.message.id === next.message.id &&
    previous.message.content === next.message.content &&
    previous.message.display_content === next.message.display_content &&
    previous.message.attachments === next.message.attachments &&
    previous.rewinding === next.rewinding &&
    previous.rewindLabel === next.rewindLabel &&
    Boolean(previous.onRewind) === Boolean(next.onRewind),
);

function UserBubbleInner({
  message,
  onRewind,
  rewinding = false,
  rewindLabel,
}: {
  message: Message;
  /** Present only when this prompt opened a turn that can be rewound from;
   * absent hides the control entirely rather than disabling it. */
  onRewind?: () => void;
  rewinding?: boolean;
  rewindLabel?: string;
}) {
  const theme = useTheme();
  const { t } = useI18n();
  const content = message.display_content ?? message.content;

  async function copy() {
    await Clipboard.setStringAsync(message.content);
    await Haptics.notificationAsync(Haptics.NotificationFeedbackType.Success);
  }

  return (
    <View style={styles.userFrame}>
      {message.attachments?.length ? (
        <View style={styles.attachments}>
          {message.attachments.map((attachment, index) => (
            <AttachmentTile
              attachment={attachment}
              key={`${attachment.blob_reference ?? attachment.path}:${index}`}
            />
          ))}
        </View>
      ) : null}
      {content ? (
        <Pressable
          accessibilityHint={t('transcript.copy_hint')}
          delayLongPress={350}
          onLongPress={() => void copy()}
          style={[styles.userBubble, { backgroundColor: theme.raised }]}>
          <Text selectable style={[styles.userText, { color: theme.text }]}>{content}</Text>
        </Pressable>
      ) : null}
      {onRewind && (
        <Pressable
          accessibilityLabel={rewindLabel}
          accessibilityRole="button"
          accessibilityState={{ disabled: rewinding }}
          disabled={rewinding}
          hitSlop={10}
          onPress={onRewind}
          style={({ pressed }) => [styles.rewindButton, { opacity: pressed || rewinding ? 0.5 : 1 }]}>
          {rewinding
            ? <ActivityIndicator size="small" />
            : (
                <AppSymbol
                  name={{ ios: 'arrow.uturn.backward', android: 'undo', web: 'undo' }}
                  size={12}
                  tintColor={theme.textGhost}
                />
              )}
          <Text style={[styles.rewindLabel, { color: theme.textGhost }]}>{rewindLabel}</Text>
        </Pressable>
      )}
    </View>
  );
}

function ChangedFilesCard({ checkpoint }: { checkpoint: Checkpoint }) {
  const theme = useTheme();
  const { t } = useI18n();
  const keepTop = useRowAnchor();
  const [open, setOpen] = useState(false);
  return (
    <View style={[styles.changedCard, { backgroundColor: theme.surface, borderColor: theme.border }]}>
      <Pressable
        accessibilityRole="button"
        accessibilityState={{ expanded: open }}
        onPress={() => keepTop(() => setOpen((value) => !value))}
        style={({ pressed }) => [styles.changedHeader, { opacity: pressed ? 0.6 : 1 }]}>
        <AppSymbol
          name={{ ios: 'plusminus', android: 'difference', web: 'difference' }}
          size={13}
          tintColor={theme.textSecondary}
        />
        <Text style={[styles.changedTitle, { color: theme.textSecondary }]}>
          {checkpoint.files.length} file{checkpoint.files.length === 1 ? '' : 's'} changed
        </Text>
        <Text style={styles.changedStats}>
          <Text style={{ color: theme.success }}>+{checkpoint.additions}</Text>
          <Text style={{ color: theme.textGhost }}> </Text>
          <Text style={{ color: theme.danger }}>−{checkpoint.deletions}</Text>
        </Text>
        <AppSymbol
          name={open
            ? { ios: 'chevron.up', android: 'keyboard_arrow_up', web: 'keyboard_arrow_up' }
            : { ios: 'chevron.down', android: 'keyboard_arrow_down', web: 'keyboard_arrow_down' }}
          size={11}
          tintColor={theme.textGhost}
        />
      </Pressable>
      {open && (
        <View style={[styles.changedFiles, { borderTopColor: theme.border }]}>
          {checkpoint.files.map((file) => (
            <View key={file.path} style={styles.changedFileRow}>
              <Text numberOfLines={1} style={[styles.changedFilePath, { color: theme.text }]}>
                {file.path}
              </Text>
              <Text style={styles.changedStats}>
                <Text style={{ color: theme.success }}>+{file.additions}</Text>
                <Text style={{ color: theme.textGhost }}> </Text>
                <Text style={{ color: theme.danger }}>−{file.deletions}</Text>
              </Text>
            </View>
          ))}
        </View>
      )}
    </View>
  );
}

/** Working-indicator flavour words, rotated every 7s, seeded per chat. */
const FLAVOUR_WORDS = [
  'Thinking', 'Pondering', 'Scheming', 'Brewing', 'Weaving', 'Tinkering',
  'Musing', 'Composing', 'Sifting', 'Untangling', 'Distilling', 'Sketching',
  'Plotting', 'Riffing', 'Combobulating', 'Percolating', 'Marinating',
  'Noodling', 'Puzzling', 'Conjuring',
];

function flavourSeed(chatId: string): number {
  let hash = 0x811c9dc5;
  for (let ix = 0; ix < chatId.length; ix += 1) {
    hash ^= chatId.charCodeAt(ix);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash;
}

function formatElapsed(seconds: number): string {
  const clamped = Math.max(0, seconds);
  if (clamped < 60) return `${clamped}s`;
  return `${Math.floor(clamped / 60)}m ${clamped % 60}s`;
}

/** The live footer under the streaming tail: spinner, flavour word, elapsed. */
export function WorkingStrip({ session }: { session: AgentSession }) {
  const theme = useTheme();
  const { t } = useI18n();
  const startedAt = session.turns.at(-1)?.started_at ?? null;
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1_000));
  useEffect(() => {
    const timer = setInterval(() => setNow(Math.floor(Date.now() / 1_000)), 1_000);
    return () => clearInterval(timer);
  }, []);
  const elapsed = startedAt ? Math.max(0, now - startedAt) : 0;
  const word = session.status === 'waiting'
    ? t('transcript.waiting_for_you')
    : `${FLAVOUR_WORDS[(flavourSeed(session.id) + Math.floor(elapsed / 7)) % FLAVOUR_WORDS.length]}…`;
  return (
    <View accessibilityLiveRegion="polite" style={styles.workingStrip}>
      <ActivityIndicator color={theme.textTertiary} size="small" />
      <Text style={[styles.workingText, { color: theme.textTertiary }]}>{word}</Text>
      {startedAt != null && session.status !== 'waiting' && (
        <Text style={[styles.workingElapsed, { color: theme.textGhost }]}>
          {formatElapsed(elapsed)}
        </Text>
      )}
    </View>
  );
}

/** Sits above the oldest mounted row while more history exists; the window
 * extends ahead of the reader so this is rarely on screen for long. */
export function EarlierIndicator() {
  const theme = useTheme();
  const { t } = useI18n();
  return (
    <View accessibilityLabel={t('transcript.loading_earlier')} style={styles.earlier}>
      <ActivityIndicator color={theme.textGhost} size="small" />
    </View>
  );
}

export function SessionEmpty({
  loading,
  error,
  missing,
}: {
  loading: boolean;
  error: unknown;
  missing: boolean;
}) {
  const theme = useTheme();
  const { t } = useI18n();
  if (loading) return <ActivityIndicator color={theme.textTertiary} />;
  return (
    <View style={styles.empty}>
      <Text style={[styles.emptyTitle, { color: theme.text }]}>
        {missing ? t('errors.task_not_found') : error ? 'Couldn’t load this task' : t('transcript.empty')}
      </Text>
      {Boolean(error) && (
        <Text style={[styles.emptyBody, { color: theme.textSecondary }]}>
          {error instanceof Error ? error.message : String(error)}
        </Text>
      )}
    </View>
  );
}

const styles = StyleSheet.create({
  earlier: { alignItems: 'center', paddingVertical: 10 },
  userFrame: { alignItems: 'flex-end', gap: 6 },
  userBubble: {
    borderRadius: Radius.large,
    borderBottomRightRadius: 6,
    maxWidth: '88%',
    paddingHorizontal: 16,
    paddingVertical: 10,
  },
  userText: { fontSize: 14, lineHeight: 21 },
  rewindButton: {
    alignItems: 'center',
    flexDirection: 'row',
    gap: 5,
    paddingHorizontal: 4,
    paddingVertical: 2,
  },
  rewindLabel: { fontSize: 11 },
  assistantActions: { flexDirection: 'row', marginTop: 4 },
  forkButton: {
    alignItems: 'center',
    flexDirection: 'row',
    gap: 5,
    paddingHorizontal: 4,
    paddingVertical: 2,
  },
  forkLabel: { fontSize: 11 },
  messageFooter: { fontSize: 10.5, marginTop: 10 },
  systemFrame: { alignItems: 'center' },
  systemMessage: {
    borderRadius: Radius.pill,
    fontSize: 11.5,
    lineHeight: 16,
    overflow: 'hidden',
    paddingHorizontal: 11,
    paddingVertical: 5,
    textAlign: 'center',
  },
  attachments: { flexDirection: 'row', flexWrap: 'wrap', justifyContent: 'flex-end', gap: 8, maxWidth: '88%' },
  collapsedRow: {
    alignItems: 'center',
    borderRadius: Radius.medium,
    borderWidth: StyleSheet.hairlineWidth,
    flexDirection: 'row',
    gap: 8,
    minHeight: 34,
    paddingHorizontal: 10,
  },
  collapsedLabel: { flexShrink: 0, fontSize: 12.5, fontWeight: '600' },
  collapsedStats: { flexShrink: 1, fontSize: 12.5 },
  collapsedHint: { flexShrink: 0, fontSize: 11 },
  // The reason sits under the row rather than inside it: the row is a button
  // whose label must stay one line, and a real error message would push the
  // stats off it.
  collapsedStack: { gap: 4 },
  collapsedReason: { fontSize: 12, lineHeight: 16, paddingHorizontal: 10 },
  changedCard: {
    borderRadius: Radius.medium,
    borderWidth: StyleSheet.hairlineWidth,
    overflow: 'hidden',
  },
  changedHeader: {
    alignItems: 'center',
    flexDirection: 'row',
    gap: 8,
    minHeight: 40,
    paddingHorizontal: 11,
  },
  changedTitle: { flex: 1, fontSize: 12.5, fontWeight: '600' },
  changedStats: { fontSize: 11.5, fontVariant: ['tabular-nums'], fontWeight: '600' },
  changedFiles: {
    borderTopWidth: StyleSheet.hairlineWidth,
    gap: 6,
    paddingHorizontal: 11,
    paddingVertical: 9,
  },
  changedFileRow: { alignItems: 'center', flexDirection: 'row', gap: 8 },
  changedFilePath: { flex: 1, fontFamily: MonoFont, fontSize: 11 },
  workingStrip: {
    alignItems: 'center',
    flexDirection: 'row',
    gap: 8,
    paddingBottom: 8,
    paddingTop: 14,
  },
  workingText: { fontSize: 12.5, fontWeight: '500' },
  workingElapsed: { fontSize: 12, fontVariant: ['tabular-nums'] },
  empty: { alignItems: 'center', paddingHorizontal: 32 },
  emptyTitle: { fontSize: 17, fontWeight: '700', textAlign: 'center' },
  emptyBody: { fontSize: 13.5, lineHeight: 19, marginTop: 8, textAlign: 'center' },
});
