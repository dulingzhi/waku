import { describe, expect, mock, test } from 'bun:test';
import type { ReactNode } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

/**
 * The authentication dialog is the only place a user can catch a relay that ran
 * one handshake per side. Its two properties are that it renders whenever a
 * connection produced a string, and that nothing in it offers a way past the
 * comparison — a dialog that can be dismissed leaves the check unperformed
 * while looking implemented.
 *
 * Rendered rather than inspected: a component that returns `null` and one that
 * renders a modal read the same in a diff, and only one of them protects
 * anything. `renderToStaticMarkup` is enough because the component holds no
 * state and needs no DOM.
 *
 * The real i18n hook is used rather than a stub. Bun's module mocks are
 * process-wide, and stubbing `@/lib/i18n` broke every later file in the run
 * that expects `translate` to exist — the hook works under `renderToStaticMarkup`
 * because its `useSyncExternalStore` falls back to the server snapshot.
 */

// Only the two presentational dependencies are stubbed: a button that forwards
// its children, and an icon that renders nothing. Both are invisible to what
// this file asserts.
mock.module('@/components/ui/button', () => ({
  Button: ({
    children,
    onClick,
  }: {
    children?: ReactNode;
    onClick?: () => void;
  }) => (
    <button onClick={onClick} type="button">
      {children}
    </button>
  ),
}));

mock.module('@/components/waku-icon', () => ({
  WakuIcon: () => null,
}));

const { SasDialog } = await import('./sas-dialog');

function render(digits = '3074 1852 9630'): string {
  return renderToStaticMarkup(
    <SasDialog digits={digits} onConfirm={() => {}} onMismatch={() => {}} />,
  );
}

describe('the authentication dialog', () => {
  test('shows the string the user must compare', () => {
    const markup = render();
    // The digits are the whole point: a dialog that asks "do they match"
    // without showing what they are leaves nothing to compare.
    expect(markup).toContain('3074 1852 9630');
  });

  test('is modal and labelled', () => {
    const markup = render();
    expect(markup).toContain('aria-modal="true"');
    expect(markup).toContain('role="dialog"');
    expect(markup).toContain('aria-labelledby="sas-title"');
  });

  test('offers exactly the two ways forward', () => {
    const markup = render();
    // Resolved through the real translations, so these are the words a user
    // actually reads rather than message keys.
    expect(markup).toContain('button');
    expect(markup.match(/<button/g)?.length).toBe(2);
  });

  test('offers no way to dismiss it', () => {
    // This is the failure the screen exists to prevent: a user waves it through
    // without comparing, and the relay's two handshakes go unnoticed. Nothing
    // in the markup may look like a skip, close, or later control.
    const markup = render();
    for (const forbidden of ['sas_skip', 'sas_close', 'sas_later', 'sas_dismiss']) {
      expect(markup).not.toContain(forbidden);
    }
    expect(markup).not.toContain('aria-label="Close"');
    // No form, so Enter cannot submit past it either.
    expect(markup).not.toContain('<form');
  });
});
