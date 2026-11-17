import { describe, expect, test } from 'bun:test';

import type { AgentTurn } from './generated';
import { turnFailureReason, turnSummaryLabel } from './transcript-presentation';

/**
 * A turn's failure reason is the sentence under its heading.
 *
 * The two answer different questions and must stay separate: the label is always
 * present, the reason is only there when the provider gave one. A failure with
 * no reason is a real state — Claude finishes a failed turn with
 * `summary: None` — and the label still reports the failure, so a reader is never
 * told a turn succeeded.
 */

function turn(overrides: Partial<AgentTurn> = {}): AgentTurn {
  return {
    id: 'turn-1',
    turn_count: 1,
    status: 'completed',
    provider_turn_started: true,
    started_at: 1_000,
    completed_at: 1_060,
    checkpoint: null,
    ...overrides,
  } as AgentTurn;
}

describe('turnFailureReason', () => {
  test('returns the reason a failed turn recorded', () => {
    expect(
      turnFailureReason(
        turn({ status: 'failed', failure_reason: 'provider exited before a response' }),
      ),
    ).toBe('provider exited before a response');
  });

  test('returns null for a failed turn with no reason', () => {
    // Not an error state: the label still says the turn failed.
    expect(turnFailureReason(turn({ status: 'failed', failure_reason: null }))).toBeNull();
    expect(turnFailureReason(turn({ status: 'failed' }))).toBeNull();
  });

  test('returns null for a turn that did not fail', () => {
    // A reason on a successful turn would put a red sentence under work that
    // worked, which is worse than showing nothing.
    expect(
      turnFailureReason(turn({ status: 'completed', failure_reason: 'stale' })),
    ).toBeNull();
    expect(
      turnFailureReason(turn({ status: 'interrupted', failure_reason: 'stale' })),
    ).toBeNull();
  });

  test('treats whitespace as no reason', () => {
    // Empty text would render an empty line, which reads as a rendering fault
    // rather than as "no reason given".
    for (const blank of ['', ' ', '\n', '   \n\t ']) {
      expect(turnFailureReason(turn({ status: 'failed', failure_reason: blank }))).toBeNull();
    }
  });

  test('keeps the label a heading even when a reason exists', () => {
    // The reason must not be folded into the label: it is prose of unknown
    // length and would push the stats off the line.
    const failed = turn({ status: 'failed', failure_reason: 'a long explanation' });
    expect(turnSummaryLabel(failed)).toBe('This turn failed');
    expect(turnSummaryLabel(failed)).not.toContain('a long explanation');
  });
});
