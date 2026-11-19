import { useMemo } from 'react';
import { StyleSheet, Text, View } from 'react-native';

import { MonoFont, Radius } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useI18n } from '@/lib/i18n-context';
import { parseUnifiedDiff, type DiffLine } from '@/lib/diff-presentation';

/**
 * A diff as a phone can actually read it.
 *
 * There used to be a second layout here: an inline strip inside a card that
 * scrolled sideways. On a phone that form is unusable — a code line is wider
 * than the screen, so reading one means dragging sideways, and the drag fights
 * the vertical scroll of the transcript it sits in. Every diff now renders in
 * this one numbered form, which wraps instead: the reader scrolls one axis,
 * the way they scroll everything else, and a wide line is never off-screen.
 *
 * Context runs are collapsed to three lines either side of a change, because a
 * phone screen holds about twenty rows and a full file would bury the edit.
 */
export function DiffView({ diff }: { diff: string }) {
  const theme = useTheme();
  const { t } = useI18n();
  const { lines, truncated } = useMemo(
    () => parseUnifiedDiff(diff, { contextLines: 3, hidePositionedHunks: true }),
    [diff],
  );
  if (!lines.length) return null;
  return (
    <View style={styles.lines}>
      {lines.map((line, index) => (
        <DiffRow key={index} line={line} />
      ))}
      {truncated ? (
        <Text style={[styles.truncated, { color: theme.textTertiary, borderColor: theme.border }]}>
          {t('diff.truncated')}
        </Text>
      ) : null}
    </View>
  );
}

function DiffRow({ line }: { line: DiffLine }) {
  const theme = useTheme();
  const { t } = useI18n();
  if (line.kind === 'gap' || line.kind === 'hunk') {
    const hidden = line.hiddenLines ?? 0;
    const label =
      line.kind === 'gap'
        ? t('diff.unmodified_lines', { count: hidden })
        : line.text;
    return (
      <View style={[styles.metaRow, { backgroundColor: theme.overlay }]}>
        <Text selectable style={[styles.meta, { color: theme.textTertiary }]}>
          {label}
        </Text>
      </View>
    );
  }

  const changed = line.kind === 'add' || line.kind === 'remove';
  const semanticColor = line.kind === 'add' ? theme.success : theme.danger;
  const background =
    line.kind === 'add'
      ? theme.successSoft
      : line.kind === 'remove'
        ? theme.dangerSoft
        : 'transparent';
  const shownLine = line.newLine ?? line.oldLine;
  return (
    <View
      style={[
        styles.row,
        {
          backgroundColor: background,
          borderLeftColor: changed ? semanticColor : 'transparent',
        },
      ]}>
      <View
        style={[
          styles.gutter,
          { backgroundColor: changed ? background : 'transparent', borderColor: theme.border },
        ]}>
        <Text
          style={[
            styles.lineNumber,
            { color: changed ? semanticColor : theme.textTertiary },
          ]}>
          {shownLine ?? ''}
        </Text>
      </View>
      {/* The marker is not decoration: without it an addition and a deletion
          differ only in colour, which a reader who cannot separate those hues
          sees as two identical rows. */}
      <Text style={[styles.marker, { color: changed ? semanticColor : theme.textGhost }]}>
        {line.kind === 'add' ? '+' : line.kind === 'remove' ? '−' : ' '}
      </Text>
      {/* Wrapping, not sideways scrolling: this column fills the remaining
          width, so a long line continues on the next row. */}
      <Text selectable style={[styles.code, { color: theme.textSecondary }]}>
        {line.text || ' '}
      </Text>
    </View>
  );
}

const styles = StyleSheet.create({
  lines: { minWidth: '100%', overflow: 'hidden' },
  row: {
    alignItems: 'stretch',
    borderLeftWidth: 2,
    flexDirection: 'row',
    minHeight: 20,
  },
  gutter: {
    alignItems: 'flex-end',
    borderRightWidth: StyleSheet.hairlineWidth,
    justifyContent: 'flex-start',
    paddingRight: 8,
    paddingTop: 2,
    width: 42,
  },
  lineNumber: {
    fontFamily: MonoFont,
    fontSize: 10.5,
    lineHeight: 17,
  },
  code: {
    flex: 1,
    flexShrink: 1,
    fontFamily: MonoFont,
    fontSize: 11.5,
    lineHeight: 17,
    paddingHorizontal: 10,
    paddingVertical: 2,
  },
  marker: {
    fontFamily: MonoFont,
    fontSize: 11,
    lineHeight: 17,
    paddingTop: 2,
    width: 14,
  },
  metaRow: { justifyContent: 'center', minHeight: 22, paddingHorizontal: 10, paddingVertical: 2 },
  meta: { fontFamily: MonoFont, fontSize: 10.5, lineHeight: 16, paddingHorizontal: 2 },
  truncated: {
    borderTopWidth: StyleSheet.hairlineWidth,
    fontSize: 10.5,
    paddingHorizontal: 12,
    paddingVertical: 8,
  },
});
