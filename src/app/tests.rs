use super::composer::{
    ComposerSubmitAction, attachment_upload_from_path, composer_submit_action,
    dropped_file_mention, merged_submission, next_picker_highlight, visible_branch_entries,
};
use super::runtime::{merge_remote_session_catalog, session_has_active_provider_turn};
use super::settings::visible_settings_pages;
use super::{
    ESCAPE_STOP_CONFIRMATION_TIMEOUT, EscapeStopConfirmation, EscapeStopPress, EscapeStopTarget,
    NAVIGATION_RAIL_TICK_HEIGHT, NAVIGATION_RAIL_TURN_HEIGHT, PendingUserInput, SessionNavigation,
    StreamDeltaKind, TranscriptRowKind::*, active_navigation_turn_index,
    append_text_delta_to_session, assistant_response_footer, assistant_response_footer_index,
    assistant_response_footer_time, compact_driver_error, disclosure_leading_space, fenced_code,
    fitted_file_tree_width, fitted_panel_widths, folded_transcript_row_kinds,
    format_worked_duration, format_working_elapsed, maintain_transcript_anchor, message_opens_turn,
    message_starts_followup_turn, navigation_preview_snippet, navigation_rail_fade_visibility,
    navigation_rail_height, navigation_rail_scale, paused_toast_duration, pop_stream_batch,
    push_transcript_activity, response_footer_message_index, response_row_turn_id,
    session_accepts_turn_output, session_is_reapable, should_refresh_branch_after_activity,
    should_show_navigation_rail, should_show_scroll_to_bottom, stream_delta_kind,
    stream_delta_text, task_id_from_notification_tag, task_notification_tag,
    transcript_anchor_end_space, transcript_navigation_turns, transcript_rests_at_tail,
    transcript_row_kinds, transcript_row_splice, transcript_rows_fingerprint,
    widened_panel_width_for_file_editor, widened_panel_width_for_review,
};
use crate::git_branch::BranchEntry;
use crate::model::{
    ActivityItem, ActivityKind, AgentSession, Checkpoint, CheckpointFile, CheckpointStatus,
    DriverEvent, Message, MessageRole, ProviderKind, ReasoningBlock, RuntimeEventCursor,
    SessionStatus, TranscriptBlock, TurnStatus, UserInputOption, UserInputQuestion,
};

#[test]
fn structured_user_input_preserves_question_order_and_custom_answer_precedence() {
    let questions = vec![
        UserInputQuestion {
            id: "environment".into(),
            header: "Environment".into(),
            question: "Where should this deploy?".into(),
            options: vec![UserInputOption {
                label: "Preview".into(),
                description: None,
            }],
            multi_select: false,
        },
        UserInputQuestion {
            id: "notes".into(),
            header: "Notes".into(),
            question: "Anything else?".into(),
            options: Vec::new(),
            multi_select: false,
        },
    ];
    let mut pending = PendingUserInput::new("request-1".into(), questions);
    pending
        .selections
        .insert("environment".into(), vec!["Preview".into()]);
    pending
        .selections
        .insert("notes".into(), vec!["stale choice".into()]);
    pending
        .custom_answers
        .insert("notes".into(), "Use the EU region".into());

    let answers = pending.answers();
    assert_eq!(answers[0].question_id, "environment");
    assert_eq!(answers[0].answers, ["Preview"]);
    assert_eq!(answers[1].question_id, "notes");
    assert_eq!(answers[1].answers, ["Use the EU region"]);
}
use gpui::{ListAlignment, ListState, Pixels, px};
use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
};
use uuid::Uuid;

fn attach_changed_files(session: &mut AgentSession, files: Vec<CheckpointFile>) {
    let turn = session.turns.last_mut().expect("the test has a turn");
    turn.checkpoint = Some(Checkpoint {
        turn_count: turn.turn_count,
        git_ref: format!("refs/waku/test-turn-{}", turn.turn_count),
        status: CheckpointStatus::Ready,
        files,
        additions: 0,
        deletions: 0,
        created_at: 1,
    });
    turn.checkpoint
        .as_mut()
        .expect("checkpoint was just attached")
        .refresh_totals();
}

#[test]
fn remote_task_catalog_adds_web_tasks_without_replacing_hydrated_detail() {
    let project_id = Uuid::new_v4();
    let mut local = AgentSession::new(project_id, ProviderKind::Codex);
    local.title = "Local title".into();
    local
        .messages
        .push(Message::new(MessageRole::User, "keep this transcript"));
    let local_id = local.id;

    let mut local_projection = local.list_projection();
    local_projection.title = "Renamed elsewhere".into();
    local_projection.status = SessionStatus::Waiting;
    local_projection.updated_at += 10;

    let mut web_task = AgentSession::new(project_id, ProviderKind::Claude).list_projection();
    web_task.title = "Created in Web".into();
    let web_task_id = web_task.id;

    let mut catalog = vec![local];
    let removed =
        merge_remote_session_catalog(&mut catalog, vec![local_projection, web_task], |_| false);

    assert!(removed.is_empty());
    assert_eq!(catalog.len(), 2);
    let merged_local = catalog
        .iter()
        .find(|session| session.id == local_id)
        .unwrap();
    assert_eq!(merged_local.title, "Renamed elsewhere");
    assert_eq!(merged_local.status, SessionStatus::Waiting);
    assert_eq!(merged_local.messages.len(), 1);
    assert_eq!(merged_local.messages[0].content, "keep this transcript");
    assert!(catalog.iter().any(|session| session.id == web_task_id));
}

#[test]
fn composer_only_offers_stop_after_submission_preparation() {
    assert_eq!(
        composer_submit_action(Some(SessionStatus::Idle), false),
        ComposerSubmitAction::Send
    );
    assert_eq!(
        composer_submit_action(Some(SessionStatus::Connecting), true),
        ComposerSubmitAction::Preparing
    );
    assert_eq!(
        composer_submit_action(Some(SessionStatus::Connecting), false),
        ComposerSubmitAction::Stop
    );
    assert_eq!(
        composer_submit_action(Some(SessionStatus::Working), false),
        ComposerSubmitAction::Stop
    );
    assert_eq!(
        composer_submit_action(Some(SessionStatus::Failed), false),
        ComposerSubmitAction::Send
    );
}

#[test]
fn connecting_status_does_not_hide_a_started_provider_turn_from_steering() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    session.begin_turn("inspect the project");
    session.mark_active_turn_provider_started();
    session.status = SessionStatus::Connecting;

    assert!(session_has_active_provider_turn(&session));
}

#[test]
fn foreground_output_recovers_a_missed_provider_turn_start_for_steering() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    session.begin_turn("inspect the project");
    session.status = SessionStatus::Connecting;

    assert!(!session_has_active_provider_turn(&session));
    assert!(session_accepts_turn_output(&mut session));
    assert_eq!(session.status, SessionStatus::Working);
    assert!(session_has_active_provider_turn(&session));
}

#[test]
fn completed_mutating_activities_refresh_git_status() {
    assert!(should_refresh_branch_after_activity(
        ActivityKind::FileChange,
        true
    ));
    assert!(should_refresh_branch_after_activity(
        ActivityKind::Command,
        true
    ));
    assert!(!should_refresh_branch_after_activity(
        ActivityKind::FileChange,
        false
    ));
    assert!(!should_refresh_branch_after_activity(
        ActivityKind::FileRead,
        true
    ));
}

#[test]
fn escape_stop_requires_a_matching_second_press_and_expires() {
    let target = EscapeStopTarget {
        session_id: Uuid::new_v4(),
        turn_id: Some(Uuid::new_v4()),
    };
    let other_turn = EscapeStopTarget {
        session_id: target.session_id,
        turn_id: Some(Uuid::new_v4()),
    };
    let mut confirmation = EscapeStopConfirmation::default();
    let now = Instant::now();

    let first_arm = match confirmation.press(target, now) {
        EscapeStopPress::Arm(arm) => arm,
        EscapeStopPress::Stop => panic!("the first press must arm Stop"),
    };
    assert!(confirmation.is_armed_for(target, now + Duration::from_secs(2)));
    assert_eq!(
        confirmation.press(target, now + Duration::from_secs(2)),
        EscapeStopPress::Stop
    );
    assert!(!confirmation.is_armed_for(target, now + Duration::from_secs(2)));

    assert_eq!(ESCAPE_STOP_CONFIRMATION_TIMEOUT, Duration::from_secs(3));
    let second_arm = match confirmation.press(target, now) {
        EscapeStopPress::Arm(arm) => arm,
        EscapeStopPress::Stop => panic!("an unarmed confirmation must arm Stop"),
    };
    assert!(!confirmation.is_armed_for(target, now + Duration::from_secs(3)));
    let replacement_arm = match confirmation.press(other_turn, now + Duration::from_secs(3)) {
        EscapeStopPress::Arm(arm) => arm,
        EscapeStopPress::Stop => panic!("an expired or different target must arm Stop again"),
    };
    assert!(!confirmation.expire(first_arm));
    assert!(!confirmation.expire(second_arm));
    assert!(confirmation.expire(replacement_arm));
}

#[test]
fn toast_pause_preserves_time_with_a_readable_minimum() {
    assert_eq!(
        paused_toast_duration(Duration::from_secs(10), Duration::from_secs(3)),
        Duration::from_secs(7)
    );
    assert_eq!(
        paused_toast_duration(Duration::from_secs(1), Duration::from_secs(5)),
        Duration::from_millis(800)
    );
}

#[test]
fn dropped_files_mention_project_relative_paths() {
    let root = std::path::Path::new("/work/repo");
    assert_eq!(
        dropped_file_mention(
            Some(root),
            std::path::Path::new("/work/repo/src/main.rs"),
            false
        ),
        "src/main.rs"
    );
    assert_eq!(
        dropped_file_mention(Some(root), std::path::Path::new("/tmp/shot.png"), false),
        "/tmp/shot.png"
    );
    assert_eq!(
        dropped_file_mention(Some(root), std::path::Path::new("/work/repo/src"), true),
        "src/"
    );
    // The project root itself relativizes to nothing; keep it absolute.
    assert_eq!(
        dropped_file_mention(Some(root), std::path::Path::new("/work/repo"), true),
        "/work/repo/"
    );
    assert_eq!(
        dropped_file_mention(None, std::path::Path::new("/tmp/no project.png"), false),
        "/tmp/no project.png"
    );
}

#[test]
fn submissions_append_attachment_mentions_after_the_prompt() {
    let mentions = vec!["src/a.rs".to_owned(), "shot.png".to_owned()];
    assert_eq!(
        merged_submission("fix this", &mentions).as_deref(),
        Some("fix this @src/a.rs @shot.png")
    );
    // Attachments alone are a valid submission; blank text contributes
    // nothing but whitespace-trimming.
    assert_eq!(
        merged_submission("  ", &mentions).as_deref(),
        Some("@src/a.rs @shot.png")
    );
    assert_eq!(merged_submission(" plain ", &[]).as_deref(), Some("plain"));
    assert_eq!(merged_submission("   ", &[]), None);
}

#[test]
fn branch_picker_pins_selection_and_filters_by_name() {
    let branches = vec![
        BranchEntry {
            name: "topic/zebra".into(),
            checked_out_elsewhere: false,
        },
        BranchEntry {
            name: "main".into(),
            checked_out_elsewhere: false,
        },
        BranchEntry {
            name: "topic/apple".into(),
            checked_out_elsewhere: true,
        },
    ];
    assert_eq!(
        visible_branch_entries(&branches, "main", "")
            .iter()
            .map(|branch| branch.name.as_str())
            .collect::<Vec<_>>(),
        vec!["main", "topic/apple", "topic/zebra"]
    );
    assert_eq!(
        visible_branch_entries(&branches, "main", "TOPIC APPLE")
            .iter()
            .map(|branch| branch.name.as_str())
            .collect::<Vec<_>>(),
        vec!["topic/apple"]
    );
}

#[test]
fn driver_errors_are_bounded_before_rendering() {
    let error = (0..20)
        .map(|line| format!("provider diagnostic line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let compact = compact_driver_error(&error);

    assert_eq!(compact.lines().count(), 7);
    assert!(compact.ends_with('…'));
    assert!(!compact.contains("provider diagnostic line 6"));

    let long = compact_driver_error(&"x".repeat(2_000));
    assert_eq!(long.chars().count(), 800);
    assert!(long.ends_with('…'));
}

#[test]
fn session_navigation_tracks_back_forward_and_new_branches() {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let third = Uuid::new_v4();
    let branch = Uuid::new_v4();
    let mut navigation = SessionNavigation::default();

    navigation.visit(Some(first), second);
    navigation.visit(Some(second), third);
    assert_eq!(navigation.go_back(third), Some(second));
    assert_eq!(navigation.go_back(second), Some(first));
    assert_eq!(navigation.go_forward(first), Some(second));

    navigation.visit(Some(second), branch);
    assert_eq!(navigation.go_forward(branch), None);
    assert_eq!(navigation.go_back(branch), Some(second));
}

#[test]
fn session_navigation_prunes_deleted_tasks() {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let third = Uuid::new_v4();
    let mut navigation = SessionNavigation::default();

    navigation.visit(Some(first), second);
    navigation.visit(Some(second), third);
    assert_eq!(navigation.go_back(third), Some(second));

    navigation.remove(first);
    navigation.remove(third);
    assert_eq!(navigation.go_back(second), None);
    assert_eq!(navigation.go_forward(second), None);
}

#[test]
fn task_notification_tags_route_to_the_corresponding_task() {
    let session_id = Uuid::new_v4();
    let tag = task_notification_tag(session_id);

    assert_eq!(task_id_from_notification_tag(&tag), Some(session_id));
    assert_eq!(task_id_from_notification_tag("waku-task:not-a-uuid"), None);
    assert_eq!(task_id_from_notification_tag(&session_id.to_string()), None);
}

#[test]
fn conversation_navigation_rail_visibility_uses_all_three_gates() {
    assert!(should_show_navigation_rail(true, 2, 872.0));
    assert!(!should_show_navigation_rail(false, 2, 872.0));
    assert!(!should_show_navigation_rail(true, 1, 872.0));
    assert!(!should_show_navigation_rail(true, 2, 871.0));
}

#[test]
fn conversation_navigation_rail_height_caps_at_eighty_percent() {
    assert_eq!(navigation_rail_height(10, 600.0), 120.0);
    // Every turn keeps its full 12px scroll position; only the viewport is
    // capped, so the remaining turns are reached by scrolling the rail.
    assert_eq!(navigation_rail_height(100, 600.0), 480.0);
    assert!(navigation_rail_height(100, 600.0) <= 600.0 * 0.80);
    assert_eq!(
        NAVIGATION_RAIL_TURN_HEIGHT - NAVIGATION_RAIL_TICK_HEIGHT,
        10.0
    );
}

#[test]
fn conversation_navigation_rail_fades_point_toward_hidden_turns() {
    assert_eq!(
        navigation_rail_fade_visibility(px(0.0), px(120.0)),
        (false, true)
    );
    assert_eq!(
        navigation_rail_fade_visibility(px(-40.0), px(120.0)),
        (true, true)
    );
    assert_eq!(
        navigation_rail_fade_visibility(px(-120.0), px(120.0)),
        (true, false)
    );
    assert_eq!(
        navigation_rail_fade_visibility(px(0.0), px(0.0)),
        (false, false)
    );
}

#[test]
fn conversation_navigation_tick_scale_follows_hover_falloff() {
    assert_eq!(navigation_rail_scale(0, None), 0.25);
    assert_eq!(navigation_rail_scale(4, Some(4)), 1.0);
    assert_eq!(navigation_rail_scale(3, Some(4)), 0.68);
    assert_eq!(navigation_rail_scale(2, Some(4)), 0.44);
    assert_eq!(navigation_rail_scale(1, Some(4)), 0.25);
}

#[test]
fn conversation_navigation_active_turn_follows_the_scroll_top_and_tail() {
    let turn_rows = [0, 4, 9];
    assert_eq!(active_navigation_turn_index(&turn_rows, 0, false), Some(0));
    assert_eq!(active_navigation_turn_index(&turn_rows, 3, false), Some(0));
    assert_eq!(active_navigation_turn_index(&turn_rows, 4, false), Some(1));
    assert_eq!(active_navigation_turn_index(&turn_rows, 8, false), Some(1));
    assert_eq!(active_navigation_turn_index(&turn_rows, 4, true), Some(2));
    assert_eq!(active_navigation_turn_index(&[], 0, false), None);
}

#[test]
fn conversation_navigation_preview_uses_each_prompt_and_latest_response() {
    let project_id = Uuid::new_v4();
    let mut session = AgentSession::new(project_id, ProviderKind::Codex);
    session.begin_turn("  First\n\nprompt  ");
    session.push_message(MessageRole::Assistant, "Interim update");
    session.push_message(MessageRole::Assistant, "Final answer");
    session.finish_active_turn(TurnStatus::Completed);
    session.begin_turn("Second prompt");
    let rows = [Message(0), Message(1), Message(2), Message(3)];

    let turns = transcript_navigation_turns(&session, &rows);
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].message_index, 0);
    assert_eq!(turns[0].row_index, 0);
    assert_eq!(turns[0].prompt, "First prompt");
    assert_eq!(turns[0].response, "Final answer");
    assert_eq!(turns[1].row_index, 3);
    assert!(turns[1].response.is_empty());
    assert_eq!(
        navigation_preview_snippet("one   two\nthree", 20),
        "one two three"
    );
}

#[test]
fn conversation_navigation_preview_does_not_change_during_a_running_turn() {
    let project_id = Uuid::new_v4();
    let mut session = AgentSession::new(project_id, ProviderKind::Codex);
    let session_id = session.id;
    session.begin_turn("Streaming prompt");
    append_text_delta_to_session(
        std::slice::from_mut(&mut session),
        session_id,
        false,
        "Partial".to_owned(),
    );
    let rows = [Message(0), Message(1)];
    let before = transcript_navigation_turns(&session, &rows);

    append_text_delta_to_session(
        std::slice::from_mut(&mut session),
        session_id,
        true,
        " response".to_owned(),
    );
    let during = transcript_navigation_turns(&session, &rows);

    assert_eq!(before, during);
    assert_eq!(during[0].response, "");

    session.finish_active_turn(TurnStatus::Completed);
    let completed = transcript_navigation_turns(&session, &rows);
    assert_eq!(completed[0].response, "Partial response");
}

#[test]
fn panel_widths_preserve_main_content_when_the_window_narrows() {
    let (sidebar, right_panel) = fitted_panel_widths(980.0, true, true, 420.0, 720.0);

    assert_eq!(sidebar, 340.0);
    assert_eq!(right_panel, 280.0);
    assert_eq!(980.0 - sidebar - right_panel, 360.0);
}

#[test]
fn hidden_panels_do_not_consume_layout_width() {
    let (sidebar, right_panel) = fitted_panel_widths(980.0, false, true, 420.0, 720.0);

    assert_eq!(sidebar, 0.0);
    assert_eq!(right_panel, 620.0);
}

#[test]
fn file_tree_width_preserves_a_usable_editor() {
    assert_eq!(fitted_file_tree_width(460.0, 184.0), 184.0);
    assert_eq!(fitted_file_tree_width(460.0, 400.0), 320.0);
    assert_eq!(fitted_file_tree_width(280.0, 184.0), 140.0);
    assert_eq!(fitted_file_tree_width(280.0, f32::NAN), 140.0);
}

#[test]
fn first_file_editor_opening_reserves_500_pixels() {
    assert_eq!(widened_panel_width_for_file_editor(460.0, 184.0), 684.0);
    assert_eq!(widened_panel_width_for_file_editor(720.0, 184.0), 720.0);
    assert_eq!(widened_panel_width_for_file_editor(460.0, 360.0), 860.0);
}

#[test]
fn first_review_opening_reserves_diff_and_tree_space() {
    assert_eq!(widened_panel_width_for_review(460.0), 820.0);
    assert_eq!(widened_panel_width_for_review(920.0), 920.0);
}

#[test]
fn anchor_end_space_keeps_a_short_new_turn_at_the_viewport_top() {
    assert_eq!(
        transcript_anchor_end_space(gpui::px(700.0), gpui::px(180.0)),
        gpui::px(520.0)
    );
    assert_eq!(
        transcript_anchor_end_space(gpui::px(700.0), gpui::px(900.0)),
        gpui::px(0.0)
    );
}

#[test]
fn scroll_to_bottom_only_appears_while_the_tail_is_below_the_viewport() {
    let viewport_bottom = px(700.0);

    assert_eq!(
        should_show_scroll_to_bottom(false, false, true, viewport_bottom, None, Pixels::ZERO),
        Some(false)
    );
    assert_eq!(
        should_show_scroll_to_bottom(
            true,
            true,
            true,
            viewport_bottom,
            Some(px(900.0)),
            Pixels::ZERO,
        ),
        Some(false)
    );
    // Disclosure pinning keeps `is_scrolled` true and a splice can leave the
    // tail temporarily unmeasured, but a collapsed transcript that fits the
    // viewport has nowhere to scroll back to.
    assert_eq!(
        should_show_scroll_to_bottom(true, false, false, viewport_bottom, None, Pixels::ZERO),
        Some(false)
    );
    assert_eq!(
        should_show_scroll_to_bottom(
            true,
            false,
            true,
            viewport_bottom,
            Some(px(701.0)),
            Pixels::ZERO,
        ),
        Some(true)
    );
    assert_eq!(
        should_show_scroll_to_bottom(
            true,
            false,
            true,
            viewport_bottom,
            Some(px(500.0)),
            px(200.0),
        ),
        Some(false)
    );
    // A stream commit remeasures the tail rows, so the frame after each one has
    // no bounds to read. Answering "show" there strobes the button against the
    // measured frames between commits; the caller holds its last answer instead.
    assert_eq!(
        should_show_scroll_to_bottom(true, false, true, viewport_bottom, None, Pixels::ZERO),
        None
    );
}

#[test]
fn scrolling_back_onto_the_tail_is_told_apart_from_an_unmeasured_tail() {
    let viewport_bottom = px(700.0);

    // Landed on the tail: the reply's last row ends at the viewport bottom,
    // with or without the anchor's reserved end space below it.
    assert_eq!(
        transcript_rests_at_tail(viewport_bottom, Some(px(700.0)), Pixels::ZERO),
        Some(true)
    );
    assert_eq!(
        transcript_rests_at_tail(viewport_bottom, Some(px(500.0)), px(200.0)),
        Some(true)
    );
    // Stopped short of it, so the stream must not reclaim the view.
    assert_eq!(
        transcript_rests_at_tail(viewport_bottom, Some(px(701.0)), Pixels::ZERO),
        Some(false)
    );
    // Unmeasured this frame: unknown, not "stopped short" — concluding the
    // latter would drop the re-engage whenever a scroll settles on a commit.
    assert_eq!(
        transcript_rests_at_tail(viewport_bottom, None, Pixels::ZERO),
        None
    );
}

#[test]
fn pending_expansion_reasserts_the_user_message_anchor() {
    let rows = gpui::ListState::new(3, gpui::ListAlignment::Bottom, gpui::px(0.0));
    rows.scroll_to(gpui::ListOffset {
        item_ix: 0,
        offset_in_item: gpui::px(42.0),
    });

    assert!(maintain_transcript_anchor(&rows, 0, true, gpui::px(320.0),));
    let anchored = rows.logical_scroll_top();
    assert_eq!(anchored.item_ix, 0);
    assert_eq!(anchored.offset_in_item, gpui::Pixels::ZERO);
    assert!(!maintain_transcript_anchor(
        &rows,
        0,
        true,
        gpui::Pixels::ZERO,
    ));
}

#[test]
fn settling_an_anchored_turn_splices_without_resetting_its_prompt() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    session.begin_turn("hi");
    session.push_message(MessageRole::Assistant, "Hello.");
    session.finish_active_turn(TurnStatus::Completed);

    let turn_id = session.begin_turn("give me a quick overview");
    session.status = SessionStatus::Working;
    session.transcript_blocks.push(TranscriptBlock {
        after_message: session.messages.len(),
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::from_reasoning(
            ReasoningBlock {
                content: "Inspecting the project".into(),
                started_at_ms: 1_000,
                finished_at_ms: 2_000,
            },
            true,
        )],
    });
    session.push_message(MessageRole::Assistant, "Here is the overview.");

    let running = folded_transcript_row_kinds(&session, &HashSet::new());
    let anchor_row = running
        .iter()
        .position(|kind| *kind == Message(2))
        .expect("the second prompt is visible");
    let rows = ListState::new(running.len(), ListAlignment::Top, px(2048.0));
    rows.scroll_to(gpui::ListOffset {
        item_ix: anchor_row,
        offset_in_item: Pixels::ZERO,
    });

    session.status = SessionStatus::Idle;
    session.finish_active_turn(TurnStatus::Completed);
    let settled = folded_transcript_row_kinds(&session, &HashSet::new());
    let (range, new_count) = transcript_row_splice(&running, &settled)
        .expect("settlement folds the live work and removes its working row");

    assert!(
        range.start > anchor_row,
        "only rows after the anchored prompt should be folded"
    );
    rows.splice(range, new_count);
    assert_eq!(rows.item_count(), settled.len());
    let retained_anchor = rows.logical_scroll_top();
    assert_eq!(
        retained_anchor.item_ix, anchor_row,
        "an exact settlement splice must retain the sent-row anchor"
    );
    assert_eq!(retained_anchor.offset_in_item, Pixels::ZERO);
}

#[test]
fn only_later_user_messages_start_followup_turns() {
    let messages = vec![
        Message::new(MessageRole::User, "first"),
        Message::new(MessageRole::Assistant, "answer"),
        Message::new(MessageRole::User, "follow-up"),
        Message::new(MessageRole::Assistant, "answer"),
    ];
    assert!(!message_starts_followup_turn(&messages, 0));
    assert!(!message_starts_followup_turn(&messages, 1));
    assert!(message_starts_followup_turn(&messages, 2));
    assert!(!message_starts_followup_turn(&messages, 3));
}

#[test]
fn only_the_turn_opening_prompt_is_a_rewind_boundary() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    session.begin_turn("first prompt");
    session.push_message(MessageRole::Assistant, "working on it");
    // A steer the provider folded into the live turn.
    session.push_user_message_with_presentation("actually, also this", None, Vec::new());
    session.push_message(MessageRole::Assistant, "answer");
    session.finish_active_turn(TurnStatus::Interrupted);
    session.begin_turn("second prompt");

    assert!(message_opens_turn(&session.messages, 0));
    assert!(!message_opens_turn(&session.messages, 1));
    assert!(
        !message_opens_turn(&session.messages, 2),
        "a steer shares the turn's checkpoint, so it cannot be rewound to on its own"
    );
    assert!(!message_opens_turn(&session.messages, 3));
    assert!(message_opens_turn(&session.messages, 4));
}

#[test]
fn fenced_code_collects_all_blocks_without_languages() {
    let markdown = "Before\n```rust\nfn main() {}\n```\nAfter\n```\ncargo test\n```";
    assert_eq!(
        fenced_code(markdown).as_deref(),
        Some("fn main() {}\n\ncargo test")
    );
    assert_eq!(fenced_code("No code here"), None);
}

#[test]
fn stream_batches_commit_full_adjacent_text_and_preserve_event_order() {
    let runtime_id = Uuid::new_v4();
    let epoch = Uuid::new_v4();
    let mut events = VecDeque::from([
        DriverEvent::TextDelta("first ".into()),
        DriverEvent::RuntimeEventCursorAdvanced(RuntimeEventCursor {
            runtime_id,
            epoch,
            sequence: 1,
        }),
        DriverEvent::TextDelta("line\nsecond line".into()),
        DriverEvent::RuntimeEventCursorAdvanced(RuntimeEventCursor {
            runtime_id,
            epoch,
            sequence: 2,
        }),
        DriverEvent::Activity {
            id: None,
            kind: ActivityKind::Tool,
            title: "Tool".into(),
            detail: None,
            complete: true,
        },
        DriverEvent::TextDelta("after tool".into()),
    ]);

    assert!(matches!(
        pop_stream_batch(&mut events, StreamDeltaKind::Text),
        Some(DriverEvent::TextDelta(text)) if text == "first line\nsecond line"
    ));
    assert!(matches!(
        events.pop_front(),
        Some(DriverEvent::RuntimeEventCursorAdvanced(cursor)) if cursor.sequence == 2
    ));
    assert!(matches!(events.front(), Some(DriverEvent::Activity { .. })));
    assert!(matches!(
        events.get(1),
        Some(DriverEvent::TextDelta(text)) if text == "after tool"
    ));
}

#[test]
fn stream_parts_keep_targeting_the_running_session_after_selection_changes() {
    let project_id = uuid::Uuid::new_v4();
    let mut running = AgentSession::new(project_id, ProviderKind::Codex);
    running.begin_turn("background task");
    running.status = SessionStatus::Working;
    let running_id = running.id;
    let visible = AgentSession::new(project_id, ProviderKind::Claude);
    let visible_id = visible.id;
    let mut sessions = vec![running, visible];

    append_text_delta_to_session(&mut sessions, running_id, false, "first".into());
    // Navigation changes only which task is rendered. The runtime keeps
    // emitting with its own task ID while another task is visible.
    let selected_session = visible_id;
    append_text_delta_to_session(&mut sessions, running_id, true, " second".into());

    assert_eq!(selected_session, visible_id);
    assert_eq!(sessions[0].messages[1].content, "first second");
    assert!(sessions[0].messages[1].streaming);
    assert!(sessions[1].messages.is_empty());
}

#[test]
fn reasoning_and_tools_share_one_ordered_activity_block() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    session.begin_turn("Build it");

    push_transcript_activity(
        &mut session,
        ActivityItem::from_reasoning(
            ReasoningBlock {
                content: "Inspecting the project".into(),
                started_at_ms: 1_000,
                finished_at_ms: 2_000,
            },
            true,
        ),
        false,
    );
    push_transcript_activity(
        &mut session,
        ActivityItem::new(None, ActivityKind::Command, "Ran tests", None, false),
        true,
    );

    assert_eq!(session.transcript_blocks.len(), 1);
    assert_eq!(session.transcript_blocks[0].activities.len(), 2);
    assert_eq!(
        session.transcript_blocks[0]
            .activities
            .iter()
            .map(|activity| activity.kind)
            .collect::<Vec<_>>(),
        [ActivityKind::Reasoning, ActivityKind::Command]
    );

    session.push_message(MessageRole::Assistant, "Interim update");
    push_transcript_activity(
        &mut session,
        ActivityItem::from_reasoning(
            ReasoningBlock {
                content: "Checking the result".into(),
                started_at_ms: 3_000,
                finished_at_ms: 4_000,
            },
            false,
        ),
        false,
    );
    assert_eq!(
        session.transcript_blocks.len(),
        2,
        "assistant text keeps later work at its own transcript position"
    );
}

#[test]
fn idle_reaping_releases_finished_sessions_but_never_a_running_turn() {
    let project_id = uuid::Uuid::new_v4();
    let fresh = Duration::from_secs(60);
    let stale = Duration::from_secs(60 * 60);

    let idle = AgentSession::new(project_id, ProviderKind::Codex);
    assert!(session_is_reapable(Some(&idle), stale, false));
    assert!(!session_is_reapable(Some(&idle), fresh, false));
    assert!(!session_is_reapable(Some(&idle), stale, true));

    let mut working = AgentSession::new(project_id, ProviderKind::Codex);
    working.begin_turn("a long tool call");
    working.status = SessionStatus::Working;
    assert!(!session_is_reapable(Some(&working), stale, false));

    // An approval can sit unanswered far longer than the idle window; its agent
    // is blocked on the user, not abandoned.
    let mut waiting = AgentSession::new(project_id, ProviderKind::Codex);
    waiting.begin_turn("needs approval");
    waiting.status = SessionStatus::Waiting;
    assert!(!session_is_reapable(Some(&waiting), stale, false));

    let mut failed = AgentSession::new(project_id, ProviderKind::Codex);
    failed.begin_turn("failed turn");
    failed.finish_active_turn(TurnStatus::Failed);
    failed.status = SessionStatus::Failed;
    assert!(session_is_reapable(Some(&failed), stale, false));

    // A runtime whose session is already gone is pure leak.
    assert!(session_is_reapable(None, stale, false));
}

#[test]
fn turn_blocks_keep_their_message_boundaries() {
    // user, assistant text, tool row, assistant text, reasoning row,
    // assistant text
    let rows = transcript_row_kinds(4, &[2, 3]);
    assert_eq!(
        rows,
        vec![
            Message(0),
            Message(1),
            TurnBlock(0),
            Message(2),
            TurnBlock(1),
            Message(3)
        ]
    );
}

#[test]
fn blocks_follow_the_latest_message_without_a_reply() {
    let rows = transcript_row_kinds(2, &[2]);
    assert_eq!(rows, vec![Message(0), Message(1), TurnBlock(0)]);
}

#[test]
fn plain_transcript_maps_one_to_one() {
    let rows = transcript_row_kinds(4, &[]);
    assert_eq!(rows, vec![Message(0), Message(1), Message(2), Message(3)]);
}

#[test]
fn multiple_blocks_at_one_boundary_preserve_event_order() {
    let rows = transcript_row_kinds(2, &[1, 1]);
    assert_eq!(
        rows,
        vec![Message(0), TurnBlock(0), TurnBlock(1), Message(1)]
    );
}

/// Row *kinds* and row *count* are derived from the same list, and
/// `transcript_row` looks a row up by index in the cached kinds. If the two
/// ever disagree — or the cache is left empty — every row silently falls back
/// to `Message(n)` and all reasoning and tool activity vanish from the
/// transcript. That is exactly the bug this guards.
/// Expanding a disclosure pins a short transcript to the bottom of its
/// viewport. Doing that needs the document's real height — treating an
/// unmeasured list as zero-height asks for a leading space of the entire
/// viewport, which pushes every row off screen and leaves the transcript blank
/// until the reader scrolls it back.
#[test]
fn a_disclosure_never_forces_a_scroll_it_cannot_measure() {
    // Unmeasured: no scroll at all.
    assert_eq!(disclosure_leading_space(px(718.0), None), None);

    // Short content sits at the bottom, with the remainder as leading space.
    assert_eq!(
        disclosure_leading_space(px(718.0), Some(px(200.0))),
        Some(px(518.0))
    );

    // Content taller than the viewport needs no leading space, and never a
    // negative one.
    assert_eq!(
        disclosure_leading_space(px(718.0), Some(px(5_000.0))),
        Some(Pixels::ZERO)
    );
}

/// Expanding a disclosure re-measures exactly one row by splicing it in place.
/// That splice went dead while the row-kind cache was empty, so this pins the
/// behaviour it depends on: replacing one row with one row must not disturb the
/// list's contents.
#[test]
fn splicing_one_row_in_place_preserves_the_list() {
    let list = ListState::new(6, ListAlignment::Bottom, px(2048.0));
    assert_eq!(list.item_count(), 6);

    list.splice(3..4, 1);
    assert_eq!(
        list.item_count(),
        6,
        "a 1-for-1 splice must keep the row count"
    );

    // The tail remeasure path splices a trailing window.
    list.splice(3..6, 3);
    assert_eq!(list.item_count(), 6);

    // A range at the very end is still valid.
    list.splice(5..6, 1);
    assert_eq!(list.item_count(), 6);
}

#[test]
fn row_kinds_and_row_count_describe_the_same_rows() {
    let project_id = Uuid::new_v4();
    let mut session = AgentSession::new(project_id, ProviderKind::Codex);
    let turn_id = session.begin_turn("Build it");
    session.transcript_blocks.push(TranscriptBlock {
        after_message: 1,
        turn_id: Some(turn_id),
        activities: vec![
            ActivityItem::from_reasoning(
                ReasoningBlock {
                    content: "Looking around".into(),
                    started_at_ms: 1_000,
                    finished_at_ms: 2_000,
                },
                true,
            ),
            ActivityItem::new(None, ActivityKind::Command, "Ran tests", None, true),
        ],
    });
    session.push_message(MessageRole::Assistant, "Done.");
    session.finish_active_turn(TurnStatus::Completed);

    let kinds = folded_transcript_row_kinds(&session, &HashSet::new());
    // The work the agent did must be reachable by index, not just counted.
    assert!(
        kinds.iter().any(|kind| matches!(kind, TurnBlock(_))),
        "reasoning and activity must survive into the rendered rows: {kinds:?}"
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| matches!(kind, TurnBlock(_)))
            .count(),
        1,
        "reasoning and tools share one activity cluster: {kinds:?}"
    );
    // Every index below the count resolves to a real row.
    for index in 0..kinds.len() {
        assert!(kinds.get(index).is_some());
    }
    // By default every row of the turn is on screen: work and answer alike.
    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::new()),
        vec![
            Message(0),
            TurnBlock(0),
            Message(1),
            ResponseFooter(turn_id, 1),
        ]
    );
    // Collapsing a turn is the reader's own act, and it hides the whole turn
    // behind one summary row rather than a divider that splits the answer.
    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::from([turn_id])),
        vec![Message(0), TurnCollapsed(turn_id)]
    );
}

#[test]
fn changed_files_attach_to_the_response_footer() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let first_turn = session.begin_turn("Build it");
    session.push_message(MessageRole::Assistant, "Done.");
    session.finish_active_turn(TurnStatus::Completed);
    attach_changed_files(
        &mut session,
        vec![CheckpointFile {
            path: "src/app.rs".into(),
            additions: 12,
            deletions: 3,
        }],
    );

    session.begin_turn("One more thing");
    session.status = SessionStatus::Connecting;

    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::new()),
        vec![
            Message(0),
            Message(1),
            ResponseFooter(first_turn, 1),
            Message(2),
            WorkingIndicator,
        ],
        "the footer containing the card must remain before the next prompt"
    );
    assert_eq!(response_footer_message_index(&session, first_turn), Some(1));
}

#[test]
fn response_hover_owns_every_response_row_but_not_the_prompt() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let turn_id = session.begin_turn("Build it");
    session.transcript_blocks.push(TranscriptBlock {
        after_message: 1,
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::new(
            None,
            ActivityKind::Command,
            "Inspected the project",
            None,
            true,
        )],
    });
    session.push_message(MessageRole::Assistant, "Done.");
    session.finish_active_turn(TurnStatus::Completed);

    assert_eq!(response_row_turn_id(&session, Message(0)), None);
    for row in [
        TurnBlock(0),
        TurnCollapsed(turn_id),
        Message(1),
        ResponseFooter(turn_id, 1),
        ChangedFiles(turn_id),
    ] {
        assert_eq!(response_row_turn_id(&session, row), Some(turn_id));
    }
    assert_eq!(response_row_turn_id(&session, WorkingIndicator), None);
}

#[test]
fn changed_files_remain_visible_when_an_interrupted_turn_has_no_answer() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let turn_id = session.begin_turn("Make the change");
    session.transcript_blocks.push(TranscriptBlock {
        after_message: 1,
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::new(
            None,
            ActivityKind::Command,
            "Edited a file",
            None,
            true,
        )],
    });
    session.push_message(MessageRole::Assistant, "");
    session.finish_active_turn(TurnStatus::Interrupted);
    attach_changed_files(
        &mut session,
        vec![CheckpointFile {
            path: "src/lib.rs".into(),
            additions: 1,
            deletions: 0,
        }],
    );

    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::new()),
        vec![
            Message(0),
            TurnBlock(0),
            Message(1),
            ChangedFiles(turn_id),
        ],
        "an interrupted turn with no answer keeps its work visible by default"
    );
    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::from([turn_id])),
        vec![Message(0), TurnCollapsed(turn_id)]
    );
    assert_eq!(response_footer_message_index(&session, turn_id), None);
}

#[test]
fn changed_files_surface_appears_only_for_a_ready_nonempty_checkpoint() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let turn_id = session.begin_turn("Build it");
    session.push_message(MessageRole::Assistant, "Done.");
    session.finish_active_turn(TurnStatus::Completed);

    attach_changed_files(&mut session, Vec::new());
    assert!(
        !folded_transcript_row_kinds(&session, &HashSet::new()).contains(&ChangedFiles(turn_id))
    );

    attach_changed_files(
        &mut session,
        vec![CheckpointFile {
            path: "src/main.rs".into(),
            additions: 2,
            deletions: 1,
        }],
    );
    assert_eq!(response_footer_message_index(&session, turn_id), Some(1));
    assert!(
        !folded_transcript_row_kinds(&session, &HashSet::new()).contains(&ChangedFiles(turn_id)),
        "a response with visible text hosts the card inside its footer"
    );
    session.turns[0]
        .checkpoint
        .as_mut()
        .expect("checkpoint")
        .status = CheckpointStatus::Unavailable;
    assert!(
        !folded_transcript_row_kinds(&session, &HashSet::new()).contains(&ChangedFiles(turn_id))
    );
    assert_eq!(response_footer_message_index(&session, turn_id), Some(1));
}

#[test]
fn checkpoint_completion_invalidates_the_cached_transcript_rows() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let turn_id = session.begin_turn("Build it");
    session.push_message(MessageRole::Assistant, "Done.");
    session.finish_active_turn(TurnStatus::Completed);
    let before = transcript_rows_fingerprint(&session, &HashSet::new());

    attach_changed_files(
        &mut session,
        vec![CheckpointFile {
            path: "src/main.rs".into(),
            additions: 2,
            deletions: 1,
        }],
    );

    assert_ne!(
        transcript_rows_fingerprint(&session, &HashSet::new()),
        before
    );
    assert_eq!(response_footer_message_index(&session, turn_id), Some(1));
    assert!(
        !folded_transcript_row_kinds(&session, &HashSet::new()).contains(&ChangedFiles(turn_id)),
        "checkpoint completion changes the existing footer row's height"
    );
}

#[test]
fn an_inline_checkpoint_keeps_followup_row_identity() {
    let turn_id = Uuid::new_v4();
    let previous = vec![
        Message(0),
        Message(1),
        ResponseFooter(turn_id, 1),
        Message(2),
        WorkingIndicator,
    ];
    let with_checkpoint = previous.clone();

    assert_eq!(
        transcript_row_splice(&previous, &with_checkpoint),
        None,
        "the card remeasures its footer instead of shifting the following prompt"
    );
}

/// `refresh_transcript_row_kinds` skips the fold while this fingerprint holds
/// still, so anything that moves the rows has to move the fingerprint too.
/// Missing one leaves the transcript rendering stale rows, which drops every
/// reasoning block and tool activity from the session.
#[test]
fn the_row_fingerprint_moves_whenever_the_rows_do() {
    let mut base = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let turn_id = base.begin_turn("Build it");
    base.transcript_blocks.push(TranscriptBlock {
        after_message: 1,
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::from_reasoning(
            ReasoningBlock {
                content: "Looking around".into(),
                started_at_ms: 1_000,
                finished_at_ms: 2_000,
            },
            true,
        )],
    });
    base.push_message(MessageRole::Assistant, "I found the relevant code.");
    base.transcript_blocks.push(TranscriptBlock {
        after_message: 2,
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::new(
            None,
            ActivityKind::Command,
            "Ran tests",
            None,
            true,
        )],
    });
    base.push_message(MessageRole::Assistant, "Done. The change is ready.");
    base.finish_active_turn(TurnStatus::Completed);

    let settled = HashSet::new();
    // Both states, because a mutation inside the fold only moves rows once the
    // reader opens it — and those rows have to be right when they do.
    let rows = |session: &AgentSession| {
        (
            folded_transcript_row_kinds(session, &settled),
            folded_transcript_row_kinds(session, &HashSet::from([turn_id])),
        )
    };
    let baseline_rows = rows(&base);
    let baseline_fingerprint = transcript_rows_fingerprint(&base, &settled);

    // Collapsing lives outside the session, so check it against the same base.
    assert_ne!(
        transcript_rows_fingerprint(&base, &HashSet::from([turn_id])),
        baseline_fingerprint,
        "collapsing a turn"
    );

    type Mutation = (&'static str, fn(&mut AgentSession));
    let mutations: Vec<Mutation> = vec![
        ("a new message", |session| {
            session.push_message(MessageRole::User, "One more thing");
        }),
        ("a new transcript block", |session| {
            session.transcript_blocks.push(TranscriptBlock {
                after_message: session.messages.len(),
                turn_id: session.turns.first().map(|turn| turn.id),
                activities: vec![ActivityItem::new(
                    None,
                    ActivityKind::Command,
                    "Ran tests",
                    None,
                    true,
                )],
            });
        }),
        // `update_activity` re-anchors the block it is still appending to, so
        // the anchor cannot be treated as fixed at insertion.
        ("an existing block re-anchored", |session| {
            session.transcript_blocks[0].after_message = 2;
        }),
        ("a turn returning to running", |session| {
            session.turns[0].status = TurnStatus::Running;
        }),
        ("a message reassigned to no turn", |session| {
            session.messages[1].turn_id = None;
        }),
        // The response footer exists only for a turn that produced visible
        // text, so emptying the whole answer removes a row.
        ("the whole answer emptied", |session| {
            session.messages[1].content.clear();
            session.messages[2].content.clear();
        }),
    ];

    for (description, mutate) in mutations {
        let mut session = base.clone();
        mutate(&mut session);
        assert_ne!(
            rows(&session),
            baseline_rows,
            "{description} should change the rows — the case no longer proves anything"
        );
        assert_ne!(
            transcript_rows_fingerprint(&session, &settled),
            baseline_fingerprint,
            "{description} changed the rows but not the fingerprint, so the \
             cached rows would go stale"
        );
    }

    // The point of the guard: streamed text lands in an existing message
    // without moving a single row, and must not trigger a refold.
    let mut streamed = base.clone();
    streamed.messages[2].content.push_str(" Let me know.");
    assert_eq!(
        transcript_rows_fingerprint(&streamed, &settled),
        baseline_fingerprint,
        "appending to a message leaves the rows exactly where they were"
    );
}

/// A settled turn folds nothing by default. This is the guard against the
/// regression that motivated the redesign: any code path that hides work
/// behind a disclosure while the reader has not asked for it breaks here.
///
/// The strongest form of the assertion: with no turn collapsed, the folded row
/// list must be byte-identical to the unfolded one. If someone reintroduces a
/// default fold, this fails immediately.
#[test]
fn nothing_is_hidden_by_default() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let turn_id = session.begin_turn("Build it");
    session.transcript_blocks.push(TranscriptBlock {
        after_message: 1,
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::from_reasoning(
            ReasoningBlock {
                content: "Looking around".into(),
                started_at_ms: 1_000,
                finished_at_ms: 2_000,
            },
            true,
        )],
    });
    session.push_message(MessageRole::Assistant, "I found the relevant code.");
    session.transcript_blocks.push(TranscriptBlock {
        after_message: 2,
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::new(
            None,
            ActivityKind::Command,
            "Ran tests",
            None,
            true,
        )],
    });
    session.push_message(MessageRole::Assistant, "Done. The change is ready.");
    session.finish_active_turn(TurnStatus::Completed);

    let anchors = session
        .transcript_blocks
        .iter()
        .map(|block| block.after_message)
        .collect::<Vec<_>>();
    let unfolded = transcript_row_kinds(session.messages.len(), &anchors);
    let by_default = folded_transcript_row_kinds(&session, &HashSet::new());

    // The only row folding adds is the response footer, which is a settled
    // turn's own action row rather than hidden content.
    let without_footer = by_default
        .iter()
        .copied()
        .filter(|row| !matches!(row, ResponseFooter(_, _)))
        .collect::<Vec<_>>();
    assert_eq!(
        without_footer, unfolded,
        "with no turn collapsed, folding must hide nothing"
    );
    assert!(
        !by_default
            .iter()
            .any(|row| matches!(row, TurnCollapsed(_))),
        "no turn may be collapsed unless the reader asked"
    );
    // Every reasoning and activity row the turn produced is present.
    assert_eq!(
        by_default
            .iter()
            .filter(|row| matches!(row, TurnBlock(_)))
            .count(),
        2,
        "both work blocks must be visible: {by_default:?}"
    );
}

/// A turn without an answer — interrupted, or tool-only — still never folds
/// on its own: the raw work is what the reader has to judge.
#[test]
fn an_answerless_turn_still_shows_its_work() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let turn_id = session.begin_turn("Build it");
    session.transcript_blocks.push(TranscriptBlock {
        after_message: 1,
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::new(
            None,
            ActivityKind::Command,
            "Ran tests",
            None,
            true,
        )],
    });
    session.push_message(MessageRole::Assistant, "");
    session.finish_active_turn(TurnStatus::Failed);

    let by_default = folded_transcript_row_kinds(&session, &HashSet::new());
    assert_eq!(by_default, vec![Message(0), TurnBlock(0), Message(1)]);
    assert!(
        !by_default
            .iter()
            .any(|row| matches!(row, TurnCollapsed(_))),
        "a failed turn must show what it did: {by_default:?}"
    );
}

/// Settling a turn replaces its working indicator with its response footer and
/// moves no other row. Identical row sets at identical content means an
/// identical height, so the transcript cannot jump when the stream ends.
#[test]
fn settling_a_turn_only_swaps_the_indicator_for_the_footer() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let turn_id = session.begin_turn("Build it");
    session.transcript_blocks.push(TranscriptBlock {
        after_message: 1,
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::new(
            None,
            ActivityKind::Command,
            "Ran tests",
            None,
            true,
        )],
    });
    session.push_message(MessageRole::Assistant, "Done.");
    session.status = SessionStatus::Working;

    let running = folded_transcript_row_kinds(&session, &HashSet::new());
    assert_eq!(
        running.last(),
        Some(&WorkingIndicator),
        "a live turn closes with the working indicator: {running:?}"
    );

    session.finish_active_turn(TurnStatus::Completed);
    session.status = SessionStatus::Idle;
    let settled = folded_transcript_row_kinds(&session, &HashSet::new());

    let without_indicator = running
        .iter()
        .copied()
        .filter(|row| *row != WorkingIndicator)
        .collect::<Vec<_>>();
    let without_footer = settled
        .iter()
        .copied()
        .filter(|row| !matches!(row, ResponseFooter(_, _)))
        .collect::<Vec<_>>();
    assert_eq!(
        without_indicator, without_footer,
        "the settled rows must match the streaming rows apart from the footer"
    );
}

/// Every navigation anchor is a user prompt. Dropping the old fold row must not
/// let some other row kind become an anchor.
#[test]
fn navigation_anchors_are_exactly_the_user_prompts() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let first = session.begin_turn("Build it");
    session.transcript_blocks.push(TranscriptBlock {
        after_message: 1,
        turn_id: Some(first),
        activities: vec![ActivityItem::new(
            None,
            ActivityKind::Command,
            "Ran tests",
            None,
            true,
        )],
    });
    session.push_message(MessageRole::Assistant, "Done.");
    session.finish_active_turn(TurnStatus::Completed);
    let second = session.begin_turn("One more thing");
    session.push_message(MessageRole::Assistant, "Also done.");
    session.finish_active_turn(TurnStatus::Completed);

    let rows = folded_transcript_row_kinds(&session, &HashSet::from([first]));
    let anchors = transcript_navigation_turns(&session, &rows);

    assert_eq!(anchors.len(), 2, "one anchor per prompt: {rows:?}");
    let prompt_count = session
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .count();
    assert_eq!(
        anchors.len(),
        prompt_count,
        "anchors and prompts must stay in lockstep"
    );
    for anchor in &anchors {
        assert_eq!(
            session.messages[anchor.message_index].role,
            MessageRole::User,
            "an anchor must land on the prompt itself"
        );
    }
    // The collapsed turn is still anchored: its prompt row is outside the fold.
    assert!(rows.contains(&Message(0)), "the prompt survives collapsing");
    assert_eq!(anchors[0].message_index, 0);
}

#[test]
fn a_settled_turn_shows_all_of_its_work_with_the_answer() {
    let project_id = Uuid::new_v4();
    let mut session = AgentSession::new(project_id, ProviderKind::Codex);
    let turn_id = session.begin_turn("Build it");
    session.transcript_blocks.push(TranscriptBlock {
        after_message: 1,
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::from_reasoning(
            ReasoningBlock {
                content: "Looking around".into(),
                started_at_ms: 1_000,
                finished_at_ms: 2_000,
            },
            true,
        )],
    });
    session.push_message(MessageRole::Assistant, "I found the relevant code.");
    session.transcript_blocks.push(TranscriptBlock {
        after_message: 2,
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::new(
            None,
            ActivityKind::Command,
            "Ran tests",
            None,
            true,
        )],
    });
    session.push_message(MessageRole::Assistant, "Done. The change is ready.");
    session.finish_active_turn(TurnStatus::Completed);

    // The turn's real order, with work in place between its text parts — the
    // default presentation of a settled turn.
    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::new()),
        vec![
            Message(0),
            TurnBlock(0),
            Message(1),
            TurnBlock(1),
            Message(2),
            ResponseFooter(turn_id, 2),
        ]
    );
    // Collapsing hides the turn as a whole, footer included: the summary row
    // stands in for the entire turn.
    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::from([turn_id])),
        vec![Message(0), TurnCollapsed(turn_id)]
    );
}

/// Providers split one answer across several text parts. They arrive with no
/// work between them, so they are all answer and none of them folds.
#[test]
fn consecutive_trailing_text_parts_all_stay_out_of_the_fold() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let turn_id = session.begin_turn("Build it");
    session.transcript_blocks.push(TranscriptBlock {
        after_message: 1,
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::from_reasoning(
            ReasoningBlock {
                content: "Looking around".into(),
                started_at_ms: 1_000,
                finished_at_ms: 2_000,
            },
            true,
        )],
    });
    session.push_message(MessageRole::Assistant, "First half of the answer.");
    session.push_message(MessageRole::Assistant, "Second half of the answer.");
    session.finish_active_turn(TurnStatus::Completed);

    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::new()),
        vec![
            Message(0),
            TurnBlock(0),
            Message(1),
            Message(2),
            ResponseFooter(turn_id, 2),
        ]
    );
}

/// An interrupted turn that never produced text has nothing to stay visible,
/// so the whole turn folds behind its summary rather than spilling raw work.
#[test]
fn a_turn_without_an_answer_folds_completely() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let turn_id = session.begin_turn("Build it");
    session.transcript_blocks.push(TranscriptBlock {
        after_message: 1,
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::new(
            None,
            ActivityKind::Command,
            "Ran tests",
            None,
            true,
        )],
    });
    // The streaming placeholder never received any text.
    session.push_message(MessageRole::Assistant, "");
    session.finish_active_turn(TurnStatus::Interrupted);

    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::new()),
        vec![Message(0), TurnBlock(0), Message(1)],
        "an answerless turn is never hidden once it settles"
    );
    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::from([turn_id])),
        vec![Message(0), TurnCollapsed(turn_id)]
    );
}

#[test]
fn assistant_response_footer_is_owned_by_the_terminal_part_and_copies_the_visible_answer() {
    let project_id = Uuid::new_v4();
    let mut session = AgentSession::new(project_id, ProviderKind::Codex);
    let turn_id = session.begin_turn("Build it");
    session.transcript_blocks.push(TranscriptBlock {
        after_message: 1,
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::from_reasoning(
            ReasoningBlock {
                content: "Looking around".into(),
                started_at_ms: 1_000,
                finished_at_ms: 2_000,
            },
            true,
        )],
    });
    session.push_message(MessageRole::Assistant, "Interim commentary.");
    session.transcript_blocks.push(TranscriptBlock {
        after_message: 2,
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::new(
            None,
            ActivityKind::Command,
            "Ran tests",
            None,
            true,
        )],
    });
    session.push_message(MessageRole::Assistant, "First half of the answer.");
    session.push_message(MessageRole::Assistant, "Second half of the answer.");
    session.finish_active_turn(TurnStatus::Completed);
    session.messages[3].created_at = 100;
    session.turns.last_mut().unwrap().completed_at = Some(200);

    assert_eq!(assistant_response_footer_index(&session, 1), Some(3));
    assert_eq!(assistant_response_footer_index(&session, 3), Some(3));
    assert_eq!(assistant_response_footer(&session, 1), None);
    // The interim commentary hides behind the "Worked for X" fold, so copying
    // the message must skip it and combine only the trailing answer parts.
    assert_eq!(
        assistant_response_footer(&session, 3).as_deref(),
        Some("First half of the answer.\n\nSecond half of the answer.")
    );
    assert_eq!(assistant_response_footer_time(&session, 1), None);
    assert_eq!(assistant_response_footer_time(&session, 3), Some(200));
    assert!(
        session.messages[1..]
            .iter()
            .all(|message| message.turn_id == Some(turn_id))
    );
}

#[test]
fn response_footer_follows_trailing_tool_activity() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let turn_id = session.begin_turn("Inspect it");
    session.push_message(MessageRole::Assistant, "I’ll inspect the implementation.");
    session.transcript_blocks.push(TranscriptBlock {
        after_message: 2,
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::new(
            None,
            ActivityKind::Command,
            "Searched the source",
            None,
            true,
        )],
    });
    session.finish_active_turn(TurnStatus::Interrupted);

    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::new()),
        vec![
            Message(0),
            Message(1),
            TurnBlock(0),
            ResponseFooter(turn_id, 1),
        ]
    );
}

/// A blank part breaks the answer run the same way work does — the fold hides
/// the text before it, so the copied message must leave that text out too.
#[test]
fn assistant_response_footer_treats_a_blank_part_as_work() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let turn_id = session.begin_turn("Build it");
    session.push_message(MessageRole::Assistant, "First text part.");
    session.push_message(MessageRole::Assistant, "  ");
    session.push_message(MessageRole::Assistant, "Final text part.");
    session.finish_active_turn(TurnStatus::Completed);

    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::new()),
        vec![
            Message(0),
            Message(1),
            Message(2),
            Message(3),
            ResponseFooter(turn_id, 3),
        ]
    );
    assert_eq!(
        assistant_response_footer(&session, 3).as_deref(),
        Some("Final text part.")
    );
}

#[test]
fn running_assistant_response_withholds_its_footer() {
    let project_id = Uuid::new_v4();
    let mut session = AgentSession::new(project_id, ProviderKind::Codex);
    session.begin_turn("Keep going");
    session.push_message(MessageRole::Assistant, "Interim text.");

    assert_eq!(assistant_response_footer_index(&session, 1), None);
    assert_eq!(assistant_response_footer(&session, 1), None);
}

#[test]
fn unkeyed_assistant_message_keeps_a_standalone_footer() {
    let project_id = Uuid::new_v4();
    let mut session = AgentSession::new(project_id, ProviderKind::Codex);
    session
        .messages
        .push(Message::new(MessageRole::Assistant, "Standalone response."));
    session.messages[0].created_at = 300;

    assert_eq!(assistant_response_footer_index(&session, 0), Some(0));
    assert_eq!(
        assistant_response_footer(&session, 0).as_deref(),
        Some("Standalone response.")
    );
    assert_eq!(assistant_response_footer_time(&session, 0), Some(300));
}

#[test]
fn turn_collapse_splice_preserves_surrounding_message_rows() {
    let turn_id = Uuid::new_v4();
    let expanded = vec![
        Message(0),
        TurnBlock(0),
        Message(1),
        TurnBlock(1),
        Message(2),
        ResponseFooter(turn_id, 2),
    ];
    let collapsed = vec![Message(0), TurnCollapsed(turn_id)];

    let collapse_splice = transcript_row_splice(&expanded, &collapsed);
    assert_eq!(collapse_splice, Some((1..6, 1)));
    assert_eq!(
        transcript_row_splice(&collapsed, &expanded),
        Some((1..2, 5))
    );
    assert_eq!(transcript_row_splice(&expanded, &expanded), None);
}

#[test]
fn running_turn_keeps_its_ordered_work_visible() {
    let project_id = Uuid::new_v4();
    let mut session = AgentSession::new(project_id, ProviderKind::Codex);
    let turn_id = session.begin_turn("Keep going");
    session.transcript_blocks.push(TranscriptBlock {
        after_message: 1,
        turn_id: Some(turn_id),
        activities: vec![ActivityItem::from_reasoning(
            ReasoningBlock {
                content: "Still thinking".into(),
                started_at_ms: 1_000,
                finished_at_ms: 2_000,
            },
            false,
        )],
    });
    session.push_message(MessageRole::Assistant, "Interim update");

    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::new()),
        vec![Message(0), TurnBlock(0), Message(1)]
    );
}

#[test]
fn plain_settled_response_does_not_add_an_empty_work_fold() {
    let project_id = Uuid::new_v4();
    let mut session = AgentSession::new(project_id, ProviderKind::Codex);
    let turn_id = session.begin_turn("Answer directly");
    session.push_message(MessageRole::Assistant, "The answer.");
    session.finish_active_turn(TurnStatus::Completed);

    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::new()),
        vec![Message(0), Message(1), ResponseFooter(turn_id, 1)]
    );
}

#[test]
fn worked_duration_uses_readable_units() {
    assert_eq!(format_worked_duration(1), "1 second");
    assert_eq!(format_worked_duration(28), "28 seconds");
    assert_eq!(format_worked_duration(60), "1 minute");
    assert_eq!(format_worked_duration(88), "1 minute 28 seconds");
    assert_eq!(format_worked_duration(7_320), "2 hours 2 minutes");
}

#[test]
fn sidebar_time_labels_prefer_the_live_turn_over_the_last_reply() {
    use super::sidebar::{format_time_ago, session_time_label};

    assert_eq!(format_time_ago(0), "just now");
    assert_eq!(format_time_ago(59), "just now");
    assert_eq!(format_time_ago(300), "5m");
    assert_eq!(format_time_ago(7_200), "2h");
    assert_eq!(format_time_ago(420 * 86_400), "420d");

    // Never replied, nothing running: the row stays quiet.
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    assert_eq!(session_time_label(&session, 1_000), None);

    // A live turn counts up instead of showing the previous reply's age.
    session.last_reply_at = Some(40);
    session.begin_turn("go");
    session.status = SessionStatus::Working;
    session.turns[0].started_at = 100;
    assert_eq!(
        session_time_label(&session, 109).as_deref(),
        Some("Working for 9s")
    );

    // Settled again: back to how long ago the agent last replied.
    session.finish_active_turn(TurnStatus::Completed);
    session.status = SessionStatus::Idle;
    session.last_reply_at = Some(500);
    assert_eq!(session_time_label(&session, 800).as_deref(), Some("5m"));
}

/// The time-label wake-up chain arms exactly one timer, aimed at the next
/// instant a visible label rolls over. Firing early leaves a stale label on
/// screen for a unit; firing often burns wake-ups an idle window shouldn't
/// pay — so the boundary math is pinned here.
#[test]
fn time_label_wakes_land_exactly_on_label_boundaries() {
    use super::next_time_label_change;

    // Nothing on the clock: no sessions, or none that ever replied.
    assert_eq!(next_time_label_change(&[], 1_000), None);
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    assert_eq!(
        next_time_label_change(std::slice::from_ref(&session), 1_000),
        None
    );

    // "just now" becomes "1m" sixty seconds after the reply.
    session.last_reply_at = Some(1_000);
    let sessions = [session];
    assert_eq!(next_time_label_change(&sessions, 1_030), Some(30));
    // "1m" → "2m" at the next minute multiple, not on a fixed cadence.
    assert_eq!(next_time_label_change(&sessions, 1_090), Some(30));
    // Hours-old labels wake hourly…
    assert_eq!(
        next_time_label_change(&sessions, 1_000 + 3 * 3_600 + 1_200),
        Some(2_400)
    );
    // …and day-old labels daily.
    assert_eq!(
        next_time_label_change(&sessions, 1_000 + 2 * 86_400 + 3_600),
        Some(82_800)
    );

    // The earliest boundary across sessions wins.
    let mut fresher = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    fresher.last_reply_at = Some(1_000 + 2 * 86_400 + 3_550);
    let sessions = [&sessions[0], &fresher]
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        next_time_label_change(&sessions, 1_000 + 2 * 86_400 + 3_600),
        Some(10)
    );

    // A live turn pins the chain to seconds for its elapsed counter.
    let mut busy = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    busy.begin_turn("go");
    busy.status = SessionStatus::Working;
    let sessions = [busy];
    assert_eq!(next_time_label_change(&sessions, 1_030), Some(1));
}

#[test]
fn working_elapsed_stays_compact() {
    assert_eq!(format_working_elapsed(0), "0s");
    assert_eq!(format_working_elapsed(9), "9s");
    assert_eq!(format_working_elapsed(59), "59s");
    assert_eq!(format_working_elapsed(60), "1m");
    assert_eq!(format_working_elapsed(65), "1m 5s");
    assert_eq!(format_working_elapsed(3_600), "1h");
    assert_eq!(format_working_elapsed(3_720), "1h 2m");
}

/// The working indicator is on screen for the whole live turn: from before
/// the first chunk, below streamed content once chunks arrive, through the
/// permission pause — and gone the moment the session stops being busy.
#[test]
fn a_busy_turn_pins_the_working_indicator_after_the_last_row() {
    let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let turn_id = session.begin_turn("Build it");
    session.status = SessionStatus::Working;

    // No chunks yet: the indicator alone follows the prompt.
    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::new()),
        vec![Message(0), WorkingIndicator]
    );

    // Streamed content pushes it down, never off.
    session.push_message(MessageRole::Assistant, "Starting on it.");
    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::new()),
        vec![Message(0), Message(1), WorkingIndicator]
    );

    // A pending permission keeps the turn — and the indicator — alive.
    session.status = SessionStatus::Waiting;
    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::new()),
        vec![Message(0), Message(1), WorkingIndicator]
    );

    // A driver error can fail the session while its last turn is still
    // marked running. Busy-ness is the only input that moved, so the
    // fingerprint must move with it or the stale indicator lingers.
    session.status = SessionStatus::Working;
    let busy_fingerprint = transcript_rows_fingerprint(&session, &HashSet::new());
    session.status = SessionStatus::Failed;
    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::new()),
        vec![Message(0), Message(1)]
    );
    assert_ne!(
        transcript_rows_fingerprint(&session, &HashSet::new()),
        busy_fingerprint,
        "dropping the busy status changed the rows but not the fingerprint"
    );

    // A settled turn swaps the indicator for its final transcript shape.
    session.status = SessionStatus::Working;
    session.finish_active_turn(TurnStatus::Completed);
    session.status = SessionStatus::Idle;
    assert_eq!(
        folded_transcript_row_kinds(&session, &HashSet::new()),
        vec![Message(0), Message(1), ResponseFooter(turn_id, 1)]
    );
}

#[test]
fn model_picker_highlight_wraps_at_both_ends() {
    // Nothing highlighted yet: down opens on the first row, up on the last.
    assert_eq!(next_picker_highlight(None, 3, "down"), Some(0));
    assert_eq!(next_picker_highlight(None, 3, "up"), Some(2));

    assert_eq!(next_picker_highlight(Some(0), 3, "down"), Some(1));
    assert_eq!(next_picker_highlight(Some(2), 3, "down"), Some(0));
    assert_eq!(next_picker_highlight(Some(0), 3, "up"), Some(2));

    // Keys the filter field owns must not move the cursor.
    assert_eq!(next_picker_highlight(Some(1), 3, "home"), None);
    assert_eq!(next_picker_highlight(Some(1), 3, "enter"), None);

    // An empty result list has nothing to land on.
    assert_eq!(next_picker_highlight(None, 0, "down"), None);

    // A single row is its own neighbour in both directions. Written against
    // the arithmetic rather than a clamp, because `min(index + 1, len - 1)`
    // would also pass this and then break the wrap on longer lists.
    assert_eq!(next_picker_highlight(None, 1, "down"), Some(0));
    assert_eq!(next_picker_highlight(None, 1, "up"), Some(0));
    assert_eq!(next_picker_highlight(Some(0), 1, "down"), Some(0));
    assert_eq!(next_picker_highlight(Some(0), 1, "up"), Some(0));
}

/// Isolated so a sabotage of the single-row case cannot be masked by the
/// longer-list assertions that precede it in the wrap test above.
#[test]
fn model_picker_highlight_stays_put_on_a_single_row() {
    for key in ["down", "up"] {
        assert_eq!(
            next_picker_highlight(Some(0), 1, key),
            Some(0),
            "{key} on a one-row list must stay on the only row"
        );
        assert_eq!(
            next_picker_highlight(None, 1, key),
            Some(0),
            "{key} on a one-row list must open on the only row"
        );
    }
}

#[test]
fn settings_search_filters_pages_for_arrow_cycling() {
    use super::SettingsPage;

    let pages = |query: &str| {
        visible_settings_pages(query)
            .map(|(page, ..)| page)
            .collect::<Vec<_>>()
    };

    // An empty query keeps every page in sidebar order, so the arrows cycle
    // the full navigation even before anything is typed.
    let mut all_pages = vec![
        SettingsPage::General,
        SettingsPage::Appearance,
        SettingsPage::Providers,
        SettingsPage::Skills,
        SettingsPage::Usage,
        SettingsPage::Daemon,
    ];
    if cfg!(debug_assertions) {
        all_pages.push(SettingsPage::ComputerUse);
    }
    assert_eq!(pages(""), all_pages);

    assert_eq!(pages("theme"), vec![SettingsPage::Appearance]);
    assert_eq!(pages("skill"), vec![SettingsPage::Skills]);

    // A keyword shared across pages keeps them all reachable.
    let mut codex_pages = vec![
        SettingsPage::Providers,
        SettingsPage::Skills,
        SettingsPage::Usage,
    ];
    if cfg!(debug_assertions) {
        codex_pages.push(SettingsPage::ComputerUse);
    }
    assert_eq!(pages("codex"), codex_pages);

    assert_eq!(pages("no such setting"), vec![]);
}

#[test]
fn computer_use_navigation_is_debug_only() {
    use super::SettingsPage;

    assert!(SettingsPage::General.is_visible_in_navigation());
    assert_eq!(
        SettingsPage::ComputerUse.is_visible_in_navigation(),
        cfg!(debug_assertions)
    );
}

#[test]
fn switched_off_providers_leave_the_picker_except_for_their_locked_session() {
    use super::ModelPickerTab;
    use super::composer::visible_picker_models;
    use crate::model::{FavoriteModel, ProviderModel, ProviderProbe};

    let probe = |provider: ProviderKind, model: &str| ProviderProbe {
        provider,
        installed: true,
        path: Some(std::path::PathBuf::from(format!("/bin/{}", provider.id()))),
        models: vec![ProviderModel::new(model, model)],
        agent_presets: Vec::new(),
    };
    let probes = [
        probe(ProviderKind::Claude, "claude-sonnet-5"),
        probe(ProviderKind::Codex, "gpt-5.6-sol"),
    ];
    let favorites = [FavoriteModel {
        provider: ProviderKind::Claude,
        model: "claude-sonnet-5".into(),
    }];
    let disabled = [ProviderKind::Claude];

    // Provider tab and favorites both stop offering the switched-off provider.
    let models = visible_picker_models(
        &probes,
        &favorites,
        &disabled,
        None,
        ModelPickerTab::Provider(ProviderKind::Claude),
        "",
    );
    assert!(models.is_empty());
    let models = visible_picker_models(
        &probes,
        &favorites,
        &disabled,
        None,
        ModelPickerTab::Favorites,
        "",
    );
    assert!(models.is_empty());
    let models = visible_picker_models(
        &probes,
        &favorites,
        &disabled,
        None,
        ModelPickerTab::Provider(ProviderKind::Codex),
        "",
    );
    assert_eq!(models.len(), 1);

    // Search cannot resurface it either.
    let models = visible_picker_models(
        &probes,
        &favorites,
        &disabled,
        None,
        ModelPickerTab::Provider(ProviderKind::Codex),
        "claude",
    );
    assert!(models.is_empty());

    // A session already locked to the provider keeps its models.
    let models = visible_picker_models(
        &probes,
        &favorites,
        &disabled,
        Some(ProviderKind::Claude),
        ModelPickerTab::Provider(ProviderKind::Claude),
        "",
    );
    assert_eq!(models.len(), 1);
}

#[test]
fn model_picker_subtitle_deduplicates_the_provider_name() {
    use super::composer::model_picker_subtitle;

    assert_eq!(
        model_picker_subtitle(ProviderKind::DeepSeek, Some("DeepSeek")),
        "DeepSeek"
    );
    assert_eq!(
        model_picker_subtitle(ProviderKind::DeepSeek, Some("OpenAI")),
        "OpenAI · DeepSeek"
    );
}

#[test]
fn tab_cycle_walks_favorites_then_usable_providers_in_rail_order() {
    use super::ModelPickerTab;
    use super::composer::visible_picker_tabs;
    use crate::model::{ProviderModel, ProviderProbe};

    let probe = |provider: ProviderKind, installed: bool| ProviderProbe {
        provider,
        installed,
        path: installed.then(|| std::path::PathBuf::from(format!("/bin/{}", provider.id()))),
        models: vec![ProviderModel::new("model", "model")],
        agent_presets: Vec::new(),
    };
    let probes = [
        probe(ProviderKind::Claude, true),
        probe(ProviderKind::Codex, true),
        probe(ProviderKind::Cursor, false),
    ];

    // Uninstalled providers never join the cycle; favorites leads.
    assert_eq!(
        visible_picker_tabs(&probes, &[], None),
        vec![
            ModelPickerTab::Favorites,
            ModelPickerTab::Provider(ProviderKind::Claude),
            ModelPickerTab::Provider(ProviderKind::Codex),
        ]
    );

    // Switched-off providers leave the cycle like they leave the rail.
    assert_eq!(
        visible_picker_tabs(&probes, &[ProviderKind::Claude], None),
        vec![
            ModelPickerTab::Favorites,
            ModelPickerTab::Provider(ProviderKind::Codex),
        ]
    );

    // A locked session cycles between favorites and its own provider only,
    // even when that provider was switched off after the session started.
    assert_eq!(
        visible_picker_tabs(&probes, &[ProviderKind::Claude], Some(ProviderKind::Claude)),
        vec![
            ModelPickerTab::Favorites,
            ModelPickerTab::Provider(ProviderKind::Claude),
        ]
    );
}

#[test]
fn the_picker_is_empty_only_once_detection_has_answered() {
    use super::composer::picker_has_no_providers;
    use crate::model::{ProviderModel, ProviderProbe};

    let probe = |provider: ProviderKind, installed: bool| ProviderProbe {
        provider,
        installed,
        path: installed.then(|| std::path::PathBuf::from(format!("/bin/{}", provider.id()))),
        models: vec![ProviderModel::new("model", "model")],
        agent_presets: Vec::new(),
    };
    // What every probe looks like before detection answers: seeded with a
    // fallback catalog and not yet installed.
    let undetected = [
        probe(ProviderKind::Claude, false),
        probe(ProviderKind::Codex, false),
    ];
    let detected = [
        probe(ProviderKind::Claude, true),
        probe(ProviderKind::Codex, false),
    ];

    // An unsettled first pass reads as "not known yet", so the composer keeps
    // showing the remembered model instead of flashing an empty state.
    assert!(!picker_has_no_providers(&undetected, &[], None, false));
    // Once it settles, the same probes really do mean nothing is installed.
    assert!(picker_has_no_providers(&undetected, &[], None, true));
    // One detected CLI is enough to keep the picker populated...
    assert!(!picker_has_no_providers(&detected, &[], None, true));
    // ...until it is switched off, which empties the picker just as surely as
    // never having been installed.
    assert!(picker_has_no_providers(
        &detected,
        &[ProviderKind::Claude],
        None,
        true
    ));
    // A session already locked to that provider keeps it, switched off or not.
    assert!(!picker_has_no_providers(
        &detected,
        &[ProviderKind::Claude],
        Some(ProviderKind::Claude),
        true
    ));
}

#[test]
fn the_rail_draws_only_installed_providers_the_settings_left_on() {
    use super::composer::picker_rail_shows_provider;
    use crate::model::{ProviderModel, ProviderProbe};

    let probe = |provider: ProviderKind, installed: bool| ProviderProbe {
        provider,
        installed,
        path: installed.then(|| std::path::PathBuf::from(format!("/bin/{}", provider.id()))),
        models: vec![ProviderModel::new("model", "model")],
        agent_presets: Vec::new(),
    };
    let probes = [
        probe(ProviderKind::Claude, true),
        probe(ProviderKind::Codex, true),
        probe(ProviderKind::Cursor, false),
    ];

    // An undetected CLI and a switched-off provider both leave the rail
    // outright, rather than sitting in it dimmed.
    assert!(!picker_rail_shows_provider(
        &probes,
        &[],
        None,
        ProviderKind::Cursor
    ));
    assert!(!picker_rail_shows_provider(
        &probes,
        &[ProviderKind::Claude],
        None,
        ProviderKind::Claude
    ));

    // A provider only the *current* session locks out stays drawn: that is a
    // fact about this session, not about what the user configured.
    assert!(picker_rail_shows_provider(
        &probes,
        &[],
        Some(ProviderKind::Codex),
        ProviderKind::Claude
    ));

    // ...and the locked session keeps its own tab even once it is switched
    // off, since the picker is its only route to another model.
    assert!(picker_rail_shows_provider(
        &probes,
        &[ProviderKind::Claude],
        Some(ProviderKind::Claude),
        ProviderKind::Claude
    ));
}

// ── Streaming deltas ──────────────────────────────────────────────────────
//
// `stream_delta_kind` and `stream_delta_text` had no test at all. They are the
// gate every incoming event passes through before it becomes visible text, so a
// mistake here either swallows a provider's output or renders reasoning as
// answer prose. `pop_stream_batch` is exercised elsewhere; these pin the pair
// it is built on.

#[test]
fn stream_delta_kind_recognizes_exactly_the_two_streaming_events() {
    assert_eq!(
        stream_delta_kind(&DriverEvent::TextDelta("hi".into())),
        Some(StreamDeltaKind::Text)
    );
    assert_eq!(
        stream_delta_kind(&DriverEvent::ReasoningDelta("thinking".into())),
        Some(StreamDeltaKind::Reasoning)
    );

    // Everything else is a boundary, not a delta: batch collection stops on
    // these, and a `Some` here would let one batch swallow a whole turn.
    for event in [
        DriverEvent::TurnStarted,
        DriverEvent::TurnParked,
        DriverEvent::TurnFinished {
            success: true,
            summary: None,
        },
        DriverEvent::Error("boom".into()),
        DriverEvent::ProcessExited,
        DriverEvent::Connected {
            provider_cursor: None,
        },
    ] {
        assert_eq!(stream_delta_kind(&event), None, "{event:?}");
    }
}

#[test]
fn stream_delta_text_only_yields_the_matching_kind() {
    let text = DriverEvent::TextDelta("answer".into());
    let reasoning = DriverEvent::ReasoningDelta("thought".into());

    assert_eq!(
        stream_delta_text(&text, StreamDeltaKind::Text),
        Some("answer")
    );
    assert_eq!(
        stream_delta_text(&reasoning, StreamDeltaKind::Reasoning),
        Some("thought")
    );

    // The mismatched pairing is the one that matters: it is what stops answer
    // text from being committed into the reasoning block, and the reverse.
    assert_eq!(stream_delta_text(&text, StreamDeltaKind::Reasoning), None);
    assert_eq!(stream_delta_text(&reasoning, StreamDeltaKind::Text), None);
    assert_eq!(
        stream_delta_text(&DriverEvent::TurnStarted, StreamDeltaKind::Text),
        None
    );

    // An empty delta still identifies itself; the emptiness is filtered later,
    // so a `None` here would change when the batch may extend across one.
    let empty = DriverEvent::TextDelta(String::new());
    assert_eq!(stream_delta_text(&empty, StreamDeltaKind::Text), Some(""));
}

/// The fingerprint exists to answer "did the rows change?" without rebuilding
/// them. A fingerprint that moves on identical input redraws forever; one that
/// holds still on a real change leaves a stale transcript on screen. Both
/// directions are pinned here — the existing mutation test only covers the
/// second.
#[test]
fn the_row_fingerprint_is_deterministic_and_order_sensitive() {
    let build = || {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let turn_id = session.begin_turn("Build it");
        session.transcript_blocks.push(TranscriptBlock {
            after_message: 1,
            turn_id: Some(turn_id),
            activities: vec![ActivityItem::new(
                None,
                ActivityKind::Command,
                "Ran tests",
                None,
                true,
            )],
        });
        session.push_message(MessageRole::Assistant, "Done.");
        session.finish_active_turn(TurnStatus::Completed);
        session
    };
    let fixed_id = Uuid::new_v4();
    let with_id = || {
        let mut session = build();
        session.id = fixed_id;
        for turn in &mut session.turns {
            turn.id = Uuid::nil();
        }
        for message in &mut session.messages {
            if message.turn_id.is_some() {
                message.turn_id = Some(Uuid::nil());
            }
        }
        for block in &mut session.transcript_blocks {
            block.turn_id = Some(Uuid::nil());
        }
        session
    };

    // Determinism: the same session content must fingerprint identically on
    // every call and across equal-but-distinct sessions. Ids are normalized
    // because they are random per construction and are folded in on purpose.
    let a = with_id();
    let b = with_id();
    assert_eq!(
        transcript_rows_fingerprint(&a, &HashSet::new()),
        transcript_rows_fingerprint(&a, &HashSet::new()),
        "repeat calls on an unchanged session must agree"
    );
    assert_eq!(
        transcript_rows_fingerprint(&a, &HashSet::new()),
        transcript_rows_fingerprint(&b, &HashSet::new()),
        "two sessions with equal content must agree"
    );

    // Order matters: the transcript is a sequence, so reordering two otherwise
    // identical messages must move the fingerprint. Swapping roles keeps the
    // multiset of folded values identical, which is what makes this a real
    // order test rather than a content test.
    let mut forward = with_id();
    forward.messages[0].role = MessageRole::User;
    forward.messages[1].role = MessageRole::Assistant;
    let mut reversed = forward.clone();
    reversed.messages[0].role = MessageRole::Assistant;
    reversed.messages[1].role = MessageRole::User;
    assert_ne!(
        transcript_rows_fingerprint(&forward, &HashSet::new()),
        transcript_rows_fingerprint(&reversed, &HashSet::new()),
        "reordering two messages must change the fingerprint"
    );

    // Session identity is part of the fingerprint: two projects' sessions that
    // happen to hold the same messages must not collide, or switching between
    // them could reuse the other's cached rows.
    let mut other_session = with_id();
    other_session.id = Uuid::new_v4();
    assert_ne!(
        transcript_rows_fingerprint(&a, &HashSet::new()),
        transcript_rows_fingerprint(&other_session, &HashSet::new()),
        "a different session id must not share a fingerprint"
    );
}

/// Collision probe: sessions whose rows genuinely differ must land on
/// different fingerprints, and sessions whose rows are identical must not.
///
/// The mixing is a cheap FNV-shaped fold, not a cryptographic hash, so this is
/// not a proof of collision freedom. What it does pin is that every field the
/// fold reads is mixed in, in a way a single-field edit cannot cancel out. A
/// collision would surface as "the transcript changed but the UI did not
/// redraw" — the hardest kind of bug to see, because nothing looks broken.
///
/// The second half pins the deliberate *non*-distinction: the fold mixes
/// `status.is_busy()` and nothing finer, so the four busy statuses share one
/// fingerprint. That is the point of the optimisation — they render the same
/// working indicator — and a future change to fold the status itself would
/// false-invalidate the cached rows on every `Working` → `Waiting` pause.
#[test]
fn fingerprints_separate_what_renders_differently_and_share_what_does_not() {
    // ── Must differ: each of these changes the rows themselves. ────────────
    let mut distinct: Vec<(&str, AgentSession)> = Vec::new();
    for count in 0..12 {
        let mut session = AgentSession::new(Uuid::nil(), ProviderKind::Codex);
        session.id = Uuid::nil();
        let turn_id = session.begin_turn("prompt");
        for index in 0..count {
            session.push_message(
                if index % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                format!("message {index}"),
            );
        }
        session.transcript_blocks.push(TranscriptBlock {
            after_message: count.min(session.messages.len()),
            turn_id: Some(turn_id),
            activities: vec![ActivityItem::new(
                None,
                ActivityKind::Command,
                format!("command {count}"),
                None,
                true,
            )],
        });
        session.finish_active_turn(TurnStatus::Completed);
        distinct.push(("growing transcript", session));
    }

    // Turn status feeding the fold is a settled/running distinction: a settled
    // turn folds its work away, a running one keeps every row. This starts
    // from a live turn because `finish_active_turn` only moves a running one.
    let mut live = AgentSession::new(Uuid::nil(), ProviderKind::Codex);
    live.id = Uuid::nil();
    live.begin_turn("prompt");
    live.push_message(MessageRole::Assistant, "streaming answer");
    live.status = SessionStatus::Working;
    for status in [TurnStatus::Running, TurnStatus::Completed] {
        let mut session = live.clone();
        session.turns[0].status = status;
        distinct.push(("turn status", session));
    }

    // Busy versus settled: the working indicator row appears and disappears
    // without any turn or message changing. One settled status is enough here;
    // `Idle` and `Failed` are asserted to agree further down, because the fold
    // reads `is_busy()` and both render identically.
    let mut settled = live.clone();
    settled.status = SessionStatus::Idle;
    distinct.push(("settled session", settled));

    // The expansion set lives outside the session and the fold reads it.
    let expanded_probe = {
        let mut session = distinct[1].1.clone();
        session.id = Uuid::nil();
        session
    };
    let turn_id = expanded_probe.turns[0].id;
    assert_ne!(
        transcript_rows_fingerprint(&expanded_probe, &HashSet::new()),
        transcript_rows_fingerprint(&expanded_probe, &HashSet::from([turn_id])),
        "expanding a fold must move the fingerprint"
    );

    let mut seen = std::collections::HashMap::new();
    for (label, session) in &distinct {
        let fingerprint = transcript_rows_fingerprint(session, &HashSet::new());
        if let Some(previous) = seen.insert(fingerprint, *label) {
            panic!(
                "two transcript states with different rows shared fingerprint \
                 {fingerprint:x}: {previous} and {label}"
            );
        }
    }
    assert!(distinct.len() >= 14, "the probe must span enough states");

    // ── Must agree: these render identically, so refolding is wasted work. ──
    let busy_fingerprints = [
        SessionStatus::Connecting,
        SessionStatus::Working,
        SessionStatus::Waiting,
        SessionStatus::Background,
    ]
    .map(|status| {
        let mut session = live.clone();
        session.status = status;
        (
            status,
            transcript_rows_fingerprint(&session, &HashSet::new()),
        )
    });
    let baseline = busy_fingerprints[1].1;
    for (status, fingerprint) in busy_fingerprints {
        assert_eq!(
            fingerprint, baseline,
            "{status:?} renders the same working indicator, so it must not \
             invalidate the cached rows"
        );
    }
    // ...and a settled status is never mistaken for a busy one.
    let mut idle = live.clone();
    idle.status = SessionStatus::Idle;
    let settled_fingerprint = transcript_rows_fingerprint(&idle, &HashSet::new());
    assert_ne!(
        settled_fingerprint, baseline,
        "settling must move the fingerprint"
    );
    // `Failed` is settled too: the busy check exists precisely so a driver
    // error can drop the working indicator while its turn is still Running, and
    // both settled statuses render that same indicator-free transcript.
    let mut failed = live.clone();
    failed.status = SessionStatus::Failed;
    assert_eq!(
        transcript_rows_fingerprint(&failed, &HashSet::new()),
        settled_fingerprint,
        "Idle and Failed render identically, so they must share a fingerprint"
    );
}

/// The navigation rail's prompt preview is grapheme-truncated on purpose, so a
/// CJK prompt or an emoji prompt must never be split into broken bytes.
#[test]
fn navigation_preview_snippet_never_splits_a_grapheme_cluster() {
    // Grapheme counting is the whole point: `chars().take()` and byte slicing
    // both cut these apart, and the rail then shows a replacement glyph.
    let emoji = "😀😀😀😀😀";
    assert_eq!(navigation_preview_snippet(emoji, 3), "😀😀😀…");
    assert_eq!(
        navigation_preview_snippet("👨‍👩‍👧‍👦", 1),
        "👨‍👩‍👧‍👦",
        "a ZWJ family emoji is one grapheme and must survive whole"
    );

    // CJK is one grapheme per character: cutting at 5 keeps exactly 5.
    assert_eq!(navigation_preview_snippet("中文测试内容", 5), "中文测试内…");
    assert_eq!(
        navigation_preview_snippet("中文测试内容", 6),
        "中文测试内容"
    );

    // A combining mark rides with its base character.
    assert_eq!(
        navigation_preview_snippet("e\u{0301}e\u{0301}e\u{0301}", 2),
        "e\u{0301}e\u{0301}…"
    );

    // Short text is returned as-is, and whitespace is collapsed first so the
    // limit counts visible graphemes rather than formatting.
    assert_eq!(navigation_preview_snippet("short", 20), "short");
    assert_eq!(navigation_preview_snippet("a\n\tb   c", 20), "a b c");
    assert_eq!(navigation_preview_snippet("", 5), "");

    // The ellipsis is appended on truncation only, never when the text lands
    // exactly on the limit.
    assert_eq!(navigation_preview_snippet("abcde", 5), "abcde");
    assert_eq!(navigation_preview_snippet("abcdef", 5), "abcde…");
    // A zero limit still yields a marker rather than an empty label.
    assert_eq!(navigation_preview_snippet("anything", 0), "…");
}

/// `compact_driver_error` is displayed verbatim, so truncation has to be
/// character-safe: a mid-CJK cut would render a replacement glyph in the error
/// banner, which reads as a second failure.
#[test]
fn compact_driver_error_truncates_on_character_boundaries() {
    let long_cjk = "错".repeat(1_000);
    let compact = compact_driver_error(&long_cjk);
    assert_eq!(compact.chars().count(), 800);
    assert!(compact.ends_with('…'));
    assert!(compact.chars().all(|ch| ch == '错' || ch == '…'));
    // The truncation must count characters, not bytes: 800 characters of CJK
    // are 2400 bytes, so a byte-sliced implementation would panic or shorten.
    assert!(compact.len() > 800);

    // Multibyte content shorter than the budget is untouched.
    let short_cjk = "第一行\n第二行";
    assert_eq!(compact_driver_error(short_cjk), short_cjk);
}

// ── Remaining formatting boundaries ───────────────────────────────────────

/// The existing duration tests stop at representative values; these pin the
/// exact boundaries where the unit changes, because an off-by-one there shows
/// "60 seconds" where "1 minute" belongs.
#[test]
fn duration_labels_change_units_exactly_at_their_boundaries() {
    assert_eq!(format_worked_duration(0), "0 seconds");
    assert_eq!(format_worked_duration(59), "59 seconds");
    assert_eq!(format_worked_duration(3_599), "59 minutes 59 seconds");
    assert_eq!(format_worked_duration(3_600), "1 hour");
    assert_eq!(format_worked_duration(86_400), "24 hours");
    // Singulars, which the pluralized cases never reach.
    assert_eq!(format_worked_duration(1), "1 second");
    assert_eq!(format_worked_duration(120), "2 minutes");

    assert_eq!(format_working_elapsed(0), "0s");
    assert_eq!(format_working_elapsed(59), "59s");
    assert_eq!(format_working_elapsed(3_599), "59m 59s");
    assert_eq!(format_working_elapsed(3_600), "1h");
    assert_eq!(format_working_elapsed(86_400), "24h");
}

/// Whether foreground output may adopt the running turn. The existing test
/// covers the `Connecting` recovery path only; the guard that rejects every
/// non-busy or turnless session is what keeps stray output from opening one.
#[test]
fn only_a_busy_session_with_a_running_turn_accepts_output() {
    let session = || {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        session.begin_turn("go");
        session
    };

    // No running turn: busy status alone is not enough.
    let mut turnless = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    turnless.status = SessionStatus::Working;
    assert!(turnless.active_turn_id().is_none());
    assert!(!session_accepts_turn_output(&mut turnless));

    // Every settled status refuses, and the refusal leaves no trace.
    for status in [SessionStatus::Idle, SessionStatus::Failed] {
        let mut candidate = session();
        candidate.status = status;
        assert!(
            !session_accepts_turn_output(&mut candidate),
            "{status:?} must not accept turn output"
        );
        assert_eq!(candidate.status, status, "a refusal must not retag status");
        assert!(
            !candidate.turns[0].provider_turn_started,
            "a refusal must not claim the provider started"
        );
    }

    // Every busy status accepts, keeps the turn id, and marks the provider as
    // started so a later Cmd-Enter stops queueing.
    for status in [
        SessionStatus::Connecting,
        SessionStatus::Working,
        SessionStatus::Waiting,
        SessionStatus::Background,
    ] {
        let mut candidate = session();
        candidate.status = status;
        let turn_id = candidate.active_turn_id();
        assert!(
            session_accepts_turn_output(&mut candidate),
            "{status:?} must accept turn output"
        );
        assert_eq!(candidate.active_turn_id(), turn_id);
        assert!(candidate.turns[0].provider_turn_started);
        // Only `Connecting` is upgraded to `Working`; the other busy statuses
        // are left alone because they say something `Working` does not —
        // `Waiting` is a provider pause, `Background` is detached work. Turning
        // either into `Working` would lose the pause the user is meant to see.
        let expected = if status == SessionStatus::Connecting {
            SessionStatus::Working
        } else {
            status
        };
        assert_eq!(
            candidate.status, expected,
            "{status:?} must keep its meaning"
        );
    }
}

// ── Attachment staging ────────────────────────────────────────────────────
//
// `attachment_upload_from_path` is live production code, not a test-only
// helper: `stage_attachment_paths` (composer.rs:1939) calls it for every path
// a user drops on the composer, before the daemon's ImportAttachment command
// stores it. These tests construct real files on disk — no mocked filesystem —
// because symlink identity, directory recursion and byte counts are exactly
// the parts a mock would get wrong.

/// A temporary directory that removes itself, so a failing assertion inside a
/// test cannot leave debris in the system temp dir that a later run trips on.
struct ScratchDir {
    path: std::path::PathBuf,
}

impl ScratchDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("waku-test-{label}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn file(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, bytes).expect("write scratch file");
        path
    }

    fn dir(&self, name: &str) -> std::path::PathBuf {
        let path = self.path.join(name);
        std::fs::create_dir_all(&path).expect("create scratch subdir");
        path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn attachment_error(path: &std::path::Path) -> String {
    attachment_upload_from_path(path)
        .expect_err("this path must be rejected")
        .to_string()
}

/// A plain file becomes one base64 upload carrying the file name.
///
/// Also pins that the *unmodified* bytes are what get encoded: the payload is
/// what the daemon will store, so an encoding slip here corrupts the
/// attachment silently.
#[test]
fn a_dropped_file_becomes_one_base64_upload() {
    let scratch = ScratchDir::new("file");
    let path = scratch.file("notes.txt", b"hello attachment");

    let (name, upload, image_bytes) =
        attachment_upload_from_path(&path).expect("a regular file is accepted");
    assert_eq!(name, "notes.txt");
    assert!(image_bytes.is_none(), "a text file has no preview bytes");
    let waku_client::attachments::AttachmentUpload::File { data_base64 } = upload else {
        panic!("a regular file must produce a File upload");
    };
    use base64::Engine as _;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&data_base64)
            .expect("the payload is valid base64"),
        b"hello attachment"
    );
}

/// An image is the same upload plus the bytes again for the in-app preview.
/// The preview is only ever built from the bytes, so it must not be some
/// re-encoded form of them.
#[test]
fn an_image_keeps_its_bytes_for_the_preview() {
    let scratch = ScratchDir::new("image");
    let bytes = b"\x89PNG\r\n\x1a\nfake-but-nonempty";
    let path = scratch.file("shot.PNG", bytes);

    let (name, upload, image_bytes) =
        attachment_upload_from_path(&path).expect("an image is accepted");
    assert_eq!(name, "shot.PNG");
    assert_eq!(
        image_bytes.as_deref(),
        Some(bytes.as_slice()),
        "the preview must receive the file's own bytes"
    );
    // The extension match is case-insensitive, which matters because the name
    // is what reaches the daemon and the chip label.
    assert!(matches!(
        upload,
        waku_client::attachments::AttachmentUpload::File { .. }
    ));
}

/// A symlink is refused outright, and the refusal happens before reading.
///
/// Dropping a link would upload whatever it points at — a path outside the
/// project, or one the user never meant to send. The check has to be on the
/// link itself, which is why the function uses `symlink_metadata`.
#[cfg(unix)]
#[test]
fn a_symlink_attachment_is_refused_without_following_it() {
    let scratch = ScratchDir::new("symlink");
    let target = scratch.file("real.txt", b"secret");
    let link = scratch.path().join("link.txt");
    std::os::unix::fs::symlink(&target, &link).expect("create symlink");

    let error = attachment_error(&link);
    assert!(
        error.contains("symbolic-link"),
        "the refusal must name the reason: {error}"
    );
}

/// A file over the 32 MB cap is refused by its metadata size, so the whole
/// file is never read into memory first.
///
/// This is the limit that protects the process: `std::fs::read` on an
/// unbounded pick would allocate the file's full length, and the base64 encode
/// then adds another third on top.
#[test]
fn an_oversized_file_is_refused_at_the_32_mb_boundary() {
    let scratch = ScratchDir::new("oversize");
    // One byte past the limit is enough; the check is `>` on metadata.len().
    // The file is sparse where the OS supports it, so the test stays cheap
    // without the code under test reading it.
    let path = scratch.path().join("huge.bin");
    let limit = waku_client::attachments::MAX_ATTACHMENT_BYTES as u64;
    let file = std::fs::File::create(&path).expect("create oversized file");
    file.set_len(limit + 1).expect("extend the file");
    drop(file);

    let error = attachment_error(&path);
    assert!(
        error.contains("32 MB"),
        "the refusal must name the limit: {error}"
    );
}

/// The boundary is inclusive: exactly 32 MB is accepted, one byte more is not.
///
/// An off-by-one here would either reject a legitimate file at the documented
/// cap or let one through that the daemon's own limit then refuses — the
/// second is worse, because the failure surfaces after the expensive encode.
#[test]
fn the_32_mb_cap_is_exclusive_of_one_more_byte() {
    let scratch = ScratchDir::new("boundary");
    let limit = waku_client::attachments::MAX_ATTACHMENT_BYTES as u64;

    let at_limit = scratch.path().join("at-limit.bin");
    let file = std::fs::File::create(&at_limit).expect("create file");
    file.set_len(limit).expect("extend the file");
    drop(file);
    let (_, upload, _) =
        attachment_upload_from_path(&at_limit).expect("a file exactly at the limit is accepted");
    let waku_client::attachments::AttachmentUpload::File { data_base64 } = upload else {
        panic!("a regular file must produce a File upload");
    };
    // Base64 of 32 MB is ~44.7 MB; proving the full length was encoded is what
    // shows the accepted file was read completely rather than truncated.
    assert_eq!(
        data_base64.len(),
        limit.div_ceil(3) as usize * 4,
        "the whole file must be encoded, not a truncated head"
    );
}

/// A dropped directory becomes one Directory upload with every file, keyed by
/// its path relative to the dropped root.
///
/// Relative paths are the contract the daemon resolves against, so an
/// absolute or root-prefixed path would store the file somewhere the transcript
/// cannot link to.
#[test]
fn a_dropped_directory_uploads_every_file_with_a_relative_path() {
    let scratch = ScratchDir::new("dir");
    let root = scratch.dir("bundle");
    std::fs::write(root.join("top.txt"), b"top").expect("write nested file");
    let nested = root.join("nested");
    std::fs::create_dir_all(&nested).expect("create nested dir");
    std::fs::write(nested.join("deep.txt"), b"deep").expect("write nested file");

    let (name, upload, image_bytes) =
        attachment_upload_from_path(&root).expect("a directory is accepted");
    assert_eq!(name, "bundle");
    assert!(
        image_bytes.is_none(),
        "a directory has no single preview image"
    );
    let waku_client::attachments::AttachmentUpload::Directory { entries } = upload else {
        panic!("a directory must produce a Directory upload");
    };

    let mut relative = entries
        .iter()
        .map(|entry| entry.relative_path.to_string_lossy().replace('\\', "/"))
        .collect::<Vec<_>>();
    relative.sort();
    assert_eq!(
        relative,
        ["nested/deep.txt", "top.txt"],
        "every file is included, keyed relative to the dropped root"
    );

    use base64::Engine as _;
    for entry in &entries {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&entry.data_base64)
            .expect("each entry carries valid base64");
        assert!(!decoded.is_empty(), "no entry may be empty");
    }
}

/// Symlinks inside a dropped directory are skipped, not followed.
///
/// The recursion meets links routinely — a package tree is full of them — so
/// this cannot be a hard error like a top-level link is. It must simply not
/// descend, or the walk can leave the dropped tree or loop forever.
#[cfg(unix)]
#[test]
fn a_symlink_inside_a_dropped_directory_is_skipped() {
    let scratch = ScratchDir::new("dir-symlink");
    let root = scratch.dir("tree");
    std::fs::write(root.join("kept.txt"), b"kept").expect("write real file");
    let outside = scratch.file("outside.txt", b"secret");
    std::os::unix::fs::symlink(&outside, root.join("escape.txt")).expect("create symlink");

    let (_, upload, _) = attachment_upload_from_path(&root).expect("the tree is accepted");
    let waku_client::attachments::AttachmentUpload::Directory { entries } = upload else {
        panic!("a directory must produce a Directory upload");
    };
    let names = entries
        .iter()
        .map(|entry| entry.relative_path.to_string_lossy().replace('\\', "/"))
        .collect::<Vec<_>>();
    assert_eq!(names, ["kept.txt"], "the link must not be uploaded");
}

/// A dropped directory is bounded two ways: a per-file byte budget across the
/// whole tree, and a file-count cap.
///
/// Both protect the same conversion — every entry is read into memory and then
/// base64-encoded, so an unbounded tree both exhausts memory and builds a
/// payload the transport cannot carry.
#[test]
fn an_oversized_directory_is_refused_by_total_bytes_and_by_file_count() {
    // Total bytes: several files that are individually small but together over
    // the tree budget.
    let scratch = ScratchDir::new("dir-bytes");
    let root = scratch.dir("bulk");
    let chunk = 8 * 1024 * 1024u64;
    for index in 0..5 {
        let path = root.join(format!("part-{index}.bin"));
        let file = std::fs::File::create(&path).expect("create part");
        file.set_len(chunk).expect("extend part");
        drop(file);
    }
    let error = attachment_error(&root);
    assert!(
        error.contains("32 MB"),
        "a tree over the byte budget must be refused: {error}"
    );

    // File count: one more file than the cap allows, each one tiny so the byte
    // budget is nowhere near the trigger.
    let scratch = ScratchDir::new("dir-count");
    let many = scratch.dir("many");
    for index in 0..=waku_client::attachments::MAX_ATTACHMENT_FILES {
        std::fs::write(many.join(format!("f{index}.txt")), b"x").expect("write file");
    }
    let error = attachment_error(&many);
    assert!(
        error.contains(&waku_client::attachments::MAX_ATTACHMENT_FILES.to_string()),
        "a tree over the file cap must be refused: {error}"
    );
}

/// A path that exists but is neither a file nor a directory is refused rather
/// than silently uploaded as empty.
#[cfg(unix)]
#[test]
fn a_non_file_non_directory_attachment_is_refused() {
    let scratch = ScratchDir::new("fifo");
    let fifo = scratch.path().join("pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo should be available");
    if !status.success() {
        // A platform without mkfifo is not a reason to fail the suite.
        return;
    }

    let error = attachment_error(&fifo);
    assert!(
        error.contains("not a file or directory"),
        "a fifo must be refused: {error}"
    );
}

/// A missing path reports which path failed instead of a bare I/O error, since
/// the message is shown to the user next to the composer.
#[test]
fn a_missing_attachment_names_the_path_it_could_not_read() {
    let scratch = ScratchDir::new("missing");
    let missing = scratch.path().join("not-there.txt");

    let error = attachment_error(&missing);
    assert!(
        error.contains("not-there.txt"),
        "the failure must name the path: {error}"
    );
}
