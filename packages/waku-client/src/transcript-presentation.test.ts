import { describe, expect, test } from 'bun:test';

import type { AgentTurn, ActivityItem } from './generated';
import {
  ACTIVITY_PAYLOAD_PREVIEW_LINES,
  activityPayloadPreview,
  turnFailureReason,
  turnSummaryLabel,
} from './transcript-presentation';

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

function command(overrides: Partial<ActivityItem> = {}): ActivityItem {
  return {
    id: 'activity-1',
    source_id: null,
    kind: 'command',
    title: 'Ran npm test',
    tool_name: null,
    mcp_server: null,
    detail: null,
    arguments: null,
    output: null,
    image_urls: [],
    failed: false,
    complete: true,
    file_changes: [],
    display_target: 'npm test',
    display_description: null,
    reasoning: null,
    ...overrides,
  } as ActivityItem
}

describe('activityPayloadPreview', () => {
  test('uses the line count the activity recorded', () => {
    // The count is taken once when the activity is built, so the preview must
    // not disagree with it: a renderer that recomputed would be walking a
    // 16k-character output on every frame.
    const output = Array.from({ length: 200 }, (_, index) => `line ${index + 1}`).join('\n')
    const preview = activityPayloadPreview(command({ output, output_line_count: 200 }))

    expect(preview?.fromTail).toBe(true)
    expect(preview?.hiddenLines).toBe(200 - ACTIVITY_PAYLOAD_PREVIEW_LINES)
    expect(preview?.text.endsWith('line 200')).toBe(true)
    expect(preview?.text.split('\n').length).toBe(ACTIVITY_PAYLOAD_PREVIEW_LINES)
  })

  test('falls back to counting for a record that predates the field', () => {
    const output = Array.from({ length: 20 }, (_, index) => `line ${index + 1}`).join('\n')
    const preview = activityPayloadPreview(command({ output }))

    expect(preview?.hiddenLines).toBe(20 - ACTIVITY_PAYLOAD_PREVIEW_LINES)
  })

  test('a non-command preview takes the head and counts the rest', () => {
    const output = Array.from({ length: 20 }, (_, index) => `hit ${index + 1}`).join('\n')
    const preview = activityPayloadPreview(
      command({ kind: 'search', output, output_line_count: 20 }),
    )

    expect(preview?.fromTail).toBe(false)
    expect(preview?.text.startsWith('hit 1')).toBe(true)
    expect(preview?.hiddenLines).toBe(20 - ACTIVITY_PAYLOAD_PREVIEW_LINES)
  })

  test('blank output gets no preview at all', () => {
    // A command that printed nothing is not the same as one that printed blank
    // lines, and an empty box would say otherwise.
    expect(activityPayloadPreview(command({ output: '   \n  ' }))).toBeNull()
    expect(activityPayloadPreview(command({ output: null }))).toBeNull()
  })
})

describe('activity exit status', () => {
  test('arrives as a number, not as text inside the output', () => {
    // The drivers used to append a translated "exit code N" sentence to the
    // output, which left a row unable to state its verdict without matching
    // that sentence.
    const failed = command({ output: 'boom', exit_code: 1 })
    expect(failed.exit_code).toBe(1)
    expect(failed.output).toBe('boom')

    expect(command({ exit_code: 0 }).exit_code).toBe(0)
    // Absent from the wire entirely when there is no status: the serializer
    // omits the key rather than writing null.
    expect(command().exit_code).toBeUndefined()
  })
});
