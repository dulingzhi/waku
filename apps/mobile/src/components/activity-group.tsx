import type { TranscriptBlock } from '@waku/client';
import { activitiesForBlock } from '@waku/client/event-reducer';
import {
  activityActionLabel,
  activityPayloadPreview,
  activityRowDetail,
} from '@waku/client/transcript-presentation';
import { memo } from 'react';
import { Pressable, StyleSheet, Text, View } from 'react-native';

import { ACTIVITY_ICONS } from './activity-icons';
import { useActivitySheet } from './activity-sheet';
import { AppSymbol } from './app-symbol';
import { MonoFont } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useI18n } from '@/lib/i18n-context';

/**
 * Every activity row of a block, in the order it happened.
 *
 * A phone transcript used to show one summary line per block and push every
 * row into a sheet, which meant a tool's outcome was never visible without a
 * tap. Rows now render in place — the whole group is the unit that opens the
 * sheet, so the detail view still exists, it just is not the only way to see
 * what a tool did.
 *
 * No in-place disclosure: the sheet already handles full payloads, scroll and
 * diff, and the inverted anchored list must not absorb a layout change mid
 * stream.
 */
export const ActivityGroup = memo(function ActivityGroup({
  block,
  blockIndex,
  live,
}: {
  block: TranscriptBlock;
  blockIndex: number;
  live: boolean;
}) {
  const theme = useTheme();
  const { t } = useI18n();
  const openSheet = useActivitySheet();
  const activities = activitiesForBlock(block);
  if (!activities.length) return null;
  const target = {
    blockIndex,
    turnId: block.turn_id,
    afterMessage: block.after_message,
  };
  return (
    <View style={styles.group}>
      {activities.map((activity) => {
        const action = activityActionLabel(activity, t);
        const detail = activityRowDetail(activity, t);
        const preview = activity.reasoning ? null : activityPayloadPreview(activity);
        const failed = activity.failed;
        return (
          <Pressable
            accessibilityHint={t('transcript.open_activity')}
            accessibilityRole="button"
            hitSlop={{ top: 4, bottom: 4 }}
            key={activity.id}
            onPress={() => openSheet(target)}
            style={({ pressed }) => [
              styles.row,
              failed && { borderLeftColor: theme.danger, borderLeftWidth: 2 },
              pressed && { opacity: 0.6 },
            ]}>
            <View style={styles.line}>
              {failed ? (
                <AppSymbol
                  name={{ ios: 'exclamationmark.triangle', android: 'warning', web: 'warning' }}
                  size={12}
                  tintColor={theme.danger}
                />
              ) : (
                <AppSymbol name={ACTIVITY_ICONS[activity.kind]} size={12} tintColor={theme.textTertiary} />
              )}
              <Text
                numberOfLines={1}
                style={[styles.action, { color: failed ? theme.danger : theme.textSecondary }]}>
                {action}
              </Text>
              {detail.length > 0 && (
                <Text numberOfLines={1} style={[styles.detail, { color: theme.textSecondary }]}>
                  {detail}
                </Text>
              )}
              {failed && (
                <Text style={[styles.failed, { color: theme.danger }]}>
                  {t('transcript.row_failed')}
                </Text>
              )}
              {!activity.complete && !failed && (
                <View style={[styles.liveDot, { backgroundColor: theme.accent }]} />
              )}
            </View>
            {preview && (
              <View style={[styles.preview, { backgroundColor: theme.overlay }]}>
                <Text numberOfLines={8} style={[styles.previewText, { color: theme.textSecondary }]}>
                  {preview.text}
                </Text>
                {preview.hiddenLines > 0 && (
                  <Text style={[styles.more, { color: theme.textTertiary }]}>
                    {t('activity.more_lines', { count: preview.hiddenLines })}
                  </Text>
                )}
              </View>
            )}
          </Pressable>
        );
      })}
    </View>
  );
});

const styles = StyleSheet.create({
  group: {
    minWidth: 0,
  },
  row: {
    minHeight: 28,
    justifyContent: 'center',
    paddingVertical: 2,
  },
  line: {
    alignItems: 'center',
    flexDirection: 'row',
    gap: 8,
    minHeight: 24,
  },
  action: { flexShrink: 0, fontSize: 12.5, fontWeight: '600' },
  detail: { flexShrink: 1, fontSize: 12.5 },
  failed: { flexShrink: 0, fontSize: 11.5 },
  liveDot: { borderRadius: 3, height: 6, width: 6 },
  preview: {
    borderRadius: 6,
    marginBottom: 2,
    marginLeft: 20,
    paddingHorizontal: 10,
    paddingVertical: 6,
  },
  previewText: { fontFamily: MonoFont, fontSize: 10.5, lineHeight: 16 },
  more: { fontSize: 10, marginTop: 2 },
});
