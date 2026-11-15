import { useMemo } from 'react';
import { StyleSheet, View } from 'react-native';

import { MdBlockSettled, useMarkdownStyles } from '@/components/md-block-row';

import { parseBlocks } from './parse';

/**
 * A whole Markdown document rendered as settled blocks — a skill's SKILL.md,
 * not a transcript message.
 *
 * Deliberately separate from the transcript path: that one keys its parse
 * cache by message id, mends a streaming tail and veils appended text, none of
 * which a static document has. This renders each top-level block exactly once
 * with `MdBlockSettled`, which touches no veil and no row anchor.
 *
 * Parsing is memoized on the source text, so re-rendering the pane (a parent
 * state change, a theme swap) re-parses nothing.
 */
export function MarkdownDocument({ source }: { source: string }) {
  const styles = useMarkdownStyles();
  const blocks = useMemo(() => parseBlocks(source), [source]);
  return (
    <View style={styles_.document}>
      {blocks.map((block) => (
        <View key={`${block.start}:${block.end}`} style={styles_.block}>
          <MdBlockSettled node={block.node} source={block.source} styles={styles} />
        </View>
      ))}
    </View>
  );
}

const styles_ = StyleSheet.create({
  // A document needs its own vertical rhythm: transcript spacing lives on the
  // rows (`topGap`), which a document does not have.
  document: { gap: 10 },
  block: { flexShrink: 0 },
});
