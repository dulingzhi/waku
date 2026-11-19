import { describe, expect, mock, test } from 'bun:test';
import { createElement, type ReactNode } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

/**
 * The diff body is the only place a change is ever read on a phone, so it has
 * two properties worth holding down: every changed line reaches the screen,
 * and nothing it renders scrolls sideways.
 *
 * Rendered rather than inspected because the previous form — an inline strip
 * inside a card that scrolled horizontally — looked correct in a diff of its
 * own and was unusable under a finger. What has to be true is that the reader
 * never has to drag.
 *
 * `react-native` is replaced with DOM equivalents. Its sources are Flow-typed
 * and Bun cannot parse them; the substitutes keep the structure this file
 * checks (a row per line, wrapping code text) and nothing else.
 */

const View = ({ children }: { children?: ReactNode }) => <div>{children}</div>;
const Text = ({ children, selectable }: { children?: ReactNode; selectable?: boolean }) => (
  <span {...(selectable ? { 'data-selectable': 'true' } : {})}>{children}</span>
);
// The substitute has to record `horizontal`, because that prop is the whole
// difference between a diff a phone can read and one it cannot — and it is a
// prop, so a plain pass-through View would hide the regression completely.
const ScrollView = ({
  children,
  horizontal,
}: {
  children?: ReactNode;
  horizontal?: boolean;
}) => <div {...(horizontal ? { 'data-horizontal': 'true' } : {})}>{children}</div>;

mock.module('react-native', () => ({
  ScrollView,
  StyleSheet: { create: (styles: unknown) => styles, hairlineWidth: 1 },
  Text,
  View,
}));

mock.module('@/constants/theme', () => ({
  MonoFont: 'monospace',
  Radius: { small: 6 },
}));

mock.module('@/hooks/use-theme', () => ({
  useTheme: () => ({
    border: '#ddd',
    danger: '#c00',
    dangerSoft: '#fee',
    overlay: '#f4f4f4',
    success: '#080',
    successSoft: '#efe',
    textSecondary: '#333',
    textTertiary: '#888',
  }),
}));

mock.module('@/lib/i18n-context', () => ({
  useI18n: () => ({
    t: (key: string, values?: { count?: number }) =>
      values?.count === undefined ? key : `${key}:${values.count}`,
  }),
}));

const { DiffView } = await import('./diff-view');

const DIFF = [
  '@@ -1,3 +1,3 @@',
  ' unchanged',
  '-old line',
  '+new line',
].join('\n');

function render(diff: string): string {
  return renderToStaticMarkup(createElement(DiffView, { diff }));
}

describe('the diff a phone reads', () => {
  test('renders every changed line with its numbers', () => {
    const markup = render(DIFF);
    expect(markup).toContain('old line');
    expect(markup).toContain('new line');
    expect(markup).toContain('unchanged');
    // Line numbers come from the diff, not from a counter in the renderer.
    expect(markup).toContain('>1<');
    expect(markup).toContain('>2<');
  });

  test('marks additions and deletions, not just colours them', () => {
    // Colour alone is not a signal: a reader who cannot separate red from
    // green would see two identical rows. The marker is what makes the row
    // say which side of the change it is on.
    const markup = render(DIFF);
    expect(markup).toContain('>+<');
    expect(markup).toContain('>−<');
  });

  test('never asks the reader to drag sideways', () => {
    // The failure this guards: a horizontal ScrollView around the code column.
    // A row of a diff is wider than a phone, so that form can only be read by
    // scrolling in one axis while the transcript scrolls in the other.
    expect(render(DIFF)).not.toContain('data-horizontal');
  });

  test('lets a long line wrap instead of running off the screen', () => {
    const body = 'x'.repeat(400);
    const markup = render(`@@ -1 +1 @@\n+${body}`);
    // The code is one text node that fills the remaining width, so the
    // platform wraps it. Splitting it into per-line rows would be the other
    // way to fit a wide line, and would lose the file's own line structure.
    expect(markup).toContain(body);
    expect(markup.match(/x{400}/g)?.length).toBe(1);
  });

  test('collapses unmodified runs around a change', () => {
    const body = Array.from({ length: 20 }, (_, index) => {
      const line = index + 1;
      if (line === 8) return '-old eight\n+new eight';
      return ` line ${line}`;
    }).join('\n');
    const markup = render(`@@ -1,20 +1,20 @@\n${body}`);
    // Three context lines either side, and the gap says how many are hidden.
    expect(markup).toContain('diff.unmodified_lines:4');
    expect(markup).toContain('diff.unmodified_lines:9');
    // The middle of a long unchanged run is not rendered at all.
    expect(markup).not.toContain('line 15');
  });

  test('says when the file was too large to show whole', () => {
    const body = Array.from({ length: 500 }, (_, index) => `+line ${index}`).join('\n');
    expect(render(`@@\n${body}`)).toContain('diff.truncated');
  });

  test('renders nothing for an empty diff', () => {
    expect(render('')).toBe('');
  });
});
