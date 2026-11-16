import { describe, expect, mock, test } from 'bun:test';
import { createElement, type ReactNode } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

/**
 * The authentication gate is the only place a user can catch a relay that ran
 * one handshake per side. Its two properties are that it appears whenever a
 * connection produced a string, and that it offers no way past the comparison.
 *
 * Rendered rather than inspected: a component that returns `null` and one that
 * renders a modal look identical in a diff, and only one of them protects
 * anything.
 *
 * Every `react-native` import is replaced with a plain DOM equivalent. The
 * library ships Flow-typed sources that Bun cannot parse, so a test that
 * imported the real one fails before reaching an assertion. The substitutes
 * preserve what this file checks — that a blocking modal renders, that it
 * carries the digits, and that it has exactly two controls — and nothing else.
 */

const View = ({ children }: { children?: ReactNode }) => <div>{children}</div>;
const Text = ({ children }: { children?: ReactNode }) => <span>{children}</span>;
const Pressable = ({
  children,
  onPress,
}: {
  children?: ReactNode;
  onPress?: () => void;
}) => (
  <button onClick={onPress} type="button">
    {children}
  </button>
);
const Modal = ({
  children,
  visible,
}: {
  children?: ReactNode;
  visible?: boolean;
}) => (visible ? <div role="dialog">{children}</div> : null);

mock.module('react-native', () => ({
  Modal,
  Pressable,
  StyleSheet: { create: (styles: unknown) => styles },
  Text,
  View,
}));

mock.module('@/components/glass-surface', () => ({
  GlassSurface: View,
}));

mock.module('@/constants/theme', () => ({
  Radius: { large: 12 },
  Spacing: { four: 16, three: 12, two: 8 },
}));

mock.module('@/hooks/use-theme', () => ({
  useTheme: () => ({
    text: '#000',
    textSecondary: '#444',
    surface: '#fff',
    inset: '#eee',
  }),
}));

mock.module('@/lib/i18n-context', () => ({
  useI18n: () => ({ t: (key: string) => key }),
}));

// A switch the tests flip, rather than re-mocking between cases: `mock.module`
// replaces a module for the whole process, and re-registering it per test would
// hand later files a different shape than this one registered.
let daemonState: {
  authenticationString: string | null;
  phase: 'connected' | 'connecting' | 'disconnected';
  disconnect: () => void;
} = {
  authenticationString: null,
  phase: 'disconnected',
  disconnect: () => {},
};

mock.module('@/lib/daemon-context', () => ({
  useDaemon: () => daemonState,
}));

const { SasGate } = await import('./sas-gate');

function render(): string {
  return renderToStaticMarkup(createElement(SasGate));
}

describe('the authentication gate', () => {
  test('renders the string while a connection is unconfirmed', () => {
    daemonState = {
      authenticationString: '3074 1852 9630',
      phase: 'connected',
      disconnect: () => {},
    };
    const markup = render();
    expect(markup).toContain('3074 1852 9630');
    expect(markup).toContain('pairing.sas_title');
    expect(markup).toContain('pairing.sas_body');
  });

  test('has exactly the two ways forward', () => {
    daemonState = {
      authenticationString: '3074 1852 9630',
      phase: 'connected',
      disconnect: () => {},
    };
    const markup = render();
    expect(markup.match(/<button/g)?.length).toBe(2);
    expect(markup).toContain('pairing.sas_confirmed');
    expect(markup).toContain('pairing.sas_mismatch');
  });

  test('offers no way to dismiss it', () => {
    // The failure this screen exists to prevent: a user waves it through
    // without comparing, and the relay's two handshakes go unnoticed.
    daemonState = {
      authenticationString: '3074 1852 9630',
      phase: 'connected',
      disconnect: () => {},
    };
    const markup = render();
    for (const forbidden of ['sas_skip', 'sas_close', 'sas_later', 'sas_dismiss']) {
      expect(markup).not.toContain(forbidden);
    }
    // The backdrop is a plain view, so tapping it is not a path out either.
    expect(markup).not.toContain('aria-label="Close"');
  });

  test('renders nothing when the connection produced no string', () => {
    // Nothing to compare, so a modal would be noise. This also keeps the gate
    // from covering the app on a disconnected screen.
    daemonState = {
      authenticationString: null,
      phase: 'disconnected',
      disconnect: () => {},
    };
    expect(render()).toBe('');
  });
});
