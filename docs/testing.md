---
type: design
title: Slashie test coverage
description: How product user stories map to automated tests and live acceptance.
---

# Slashie test coverage

Every product behavior in the [specification](product.md) is guarded by a
named test, and a behavior change updates its guarding test in the same
commit — the same rule as the owning-document rule for docs. The [canonical
check matrix](ci.md) runs the automated layer on every push; the live layer
is a release gate, not a push gate.

Two layers, by [design](DESIGN.md):

1. **Automated (`cargo test`)** — mock transport, fake Lark CLI, fake route
   commands. No network, isolated state, deterministic. This layer is the
   regression contract: a user story with no row below is not protected.
2. **Live acceptance** — `scripts/test_feishu_coverage.py` and the Doubao
   session acceptance ([Doubao CLI](doubao.md)) against real Feishu and real
   targets. Covers what a mock cannot prove: search recall, indexing delay,
   permissions, and real thread placement.

## Trigger and reply

| User story | Guarding tests |
| --- | --- |
| Exact prefix match; `/pingpong` never matches `/ping` | `runtime::admission_rules_exact_prefix_only`, `config::rejects_blank_command_and_bad_prefix` |
| Opted-in routes accept a real leading mention of the bound user across groups and topics | `lark::mention_search_is_additive_and_opt_in`, `lark::detail_requires_a_verified_unambiguous_leading_mention`, `runtime::mentions_require_opt_in_bound_account_and_group_scope`, `worker::mention_inputs_strip_only_the_verified_command_prefix` |
| Multiline arguments arrive as one value, line breaks preserved | `worker::multiline_args_arrive_as_one_value`, `worker::inputs_preserve_multiline_and_source`, `runtime::multiline_and_metacharacters_reach_worker_as_data` |
| Topic root and topic replies eligible; ordinary replies ignored; groups like DMs | `runtime::topic_replies_are_admitted_but_plain_replies_are_not`, `runtime::group_shaped_messages_are_eligible_like_p2p` |
| Slashie's own delivered replies never execute | `runtime::own_reply_id_is_never_admitted_again` |
| Message text can never change the shell program | `worker::metacharacters_cannot_change_program` |
| Exact source link; missing link blocks the handoff | `runtime::handoff_forwards_exact_source_link`, `lark::source_link_is_exact_and_missing_link_blocks_handoff` |
| `OnIt` only after a durable claim; reaction failure never changes the work result | `lark::processing_reaction_targets_source_as_user`, `lark::reaction_failures_never_invent_ids_or_remove_unconfirmed_status`, `runtime::rejected_processing_reaction_still_runs_and_replies` |
| Waiting work receives `OneSecond`, and only Slashie's confirmed queue marker is removed on start | `lark::queue_reaction_uses_supported_emoji_and_removes_only_its_id`, `runtime::first_job_is_on_it_and_later_same_chat_job_is_queued_then_cleared` |
| `DONE` only for success; timeout/unavailable source gets `ERROR` | `runtime::timed_out_work_gets_error_not_done`, `runtime::unavailable_source_marks_error_without_starting_command`, `runtime::failed_agent_work_is_not_successful_silent_completion` |
| Status reactions independent of `reply_mode` | `runtime::software_reactions_do_not_depend_on_agent_text_delivery` |
| `agent` mode is default; `auto` is explicit | `config::agent_delivery_is_default_and_auto_is_explicit`, `transport::reply_modes` |
| Agent completes without text even when reactions are denied | `runtime::agent_completes_without_text_reply_even_if_reactions_are_denied` |
| Auto-mode reply contract: stdout, diagnostics, truncation notice | `worker::compose_reply_contracts`, `worker::failing_command_reports_diagnostics`, `runtime::reply_error_display_distinguishes_uncertain` |

## Conversation and session model

| User story | Guarding tests |
| --- | --- |
| Conversation key isolates chats and topics; workspace follows the key | `transport::channel_key_and_workspace_isolate_topics`, `state::chat_session_mapping_roundtrip` |
| Same conversation resumes its agent session; others start fresh | `runtime::chat_session_continues_only_within_the_same_chat`, `worker::default_agent_routes_start_and_continue_in_the_right_order`, `worker::session_env_and_workspace_are_passed` |
| Managed Codex waits for a verified active session, stops the original process, and resumes its exact ID | `config::steer_requires_the_stock_codex_managed_route`, `runtime::scan_requests_steering_only_for_immediate_bound_owner_follow_up`, `worker::managed_codex_stops_and_resumes_the_reported_session` |
| An interrupted turn retries removal of its own acknowledgment without sending terminal success or failure | `runtime::interrupted_ack_clear_is_durable_and_retried_by_flush` |
| Topic reply stays in its thread and its session | `runtime::topic_reply_maps_to_its_own_session_and_thread_reply`, `lark::reply_in_thread_passes_the_flag` |
| Commands in one conversation run in order; conversations run in parallel | `mailbox::same_conversation_runs_in_order_different_conversations_interleave`, `mailbox::conversations_progress_in_parallel` |
| Queue caps: bursts stay bounded, overflow is left for later scans, nothing claimed is stranded | `mailbox::full_mailbox_reports_full_and_new_conversations_exhaust`, `runtime::same_conversation_burst_is_capped_at_the_mailbox_not_silently_lost`, `runtime::queue_bound_leaves_candidates_unclaimed`, `runtime::conversation_slots_cap_claims_without_stranding_them` |

## Duplicates, claims, and recovery

| User story | Guarding tests |
| --- | --- |
| Duplicate and late-indexed delivery executes once | `runtime::duplicate_delivery_claims_once`, `runtime::job_runs_once_with_mock_reply_then_never_reruns` |
| Claims and terminal intent survive restart without repeating work | `runtime::pending_claim_interrupted_on_restart_never_reruns`, `runtime::pending_terminal_status_survives_restart_and_is_not_reexecuted`, `runtime::settle_write_recovers_terminal_status_without_repeating_work_or_text`, `runtime::terminal_failure_retries_only_status_and_not_work_or_text` |
| Corrupt or missing state blocks intake, never overwrites | `runtime::invalid_state_blocks_intake`, `runtime::blocked_intake_never_overwrites_the_state_file`, `state::corrupt_state_is_an_error` |
| Scan window, paging, incomplete results, transient failures | `transport::search_filters_by_window`, `transport::mget_missing_blocks_scan`, `transport::transient_search_failures_then_success`, `lark::search_follows_pages_and_merges_ids`, `lark::search_error_classification`, `runtime::incomplete_result_blocks_admission_for_that_scan`, `runtime::late_search_result_is_admitted_after_a_completed_scan` |

## Controls, configuration, and shutdown

| User story | Guarding tests |
| --- | --- |
| Fresh install starts paused with preset routes; the setting persists | `runtime::fresh_install_creates_ping_config_and_paused_state`, `runtime::fresh_install_flag_is_true_only_when_state_was_created`, `config::creates_presets_only_when_config_is_missing`, `state::fresh_state_is_paused`, `state::load_or_init_creates_paused_then_persists_polling`, `runtime::poll_override_updates_persisted_state` |
| Pause stops intake; accepted work can finish | `runtime::pause_stops_admissions_accepted_work_can_finish` |
| Shutdown drains queued work, settles it as shutdown, kills the running group | `mailbox::shutdown_pill_finishes_queued_jobs`, `runtime::queued_agent_work_settles_as_shutdown_without_starting`, `worker::shutdown_terminates_active_group`, `worker::timeout_kills_group`, `process::shutdown_terminates_a_sleeping_tree` |
| Configuration validation and hot reload | `config::rejects_*` family, `runtime::invalid_config_blocks_intake`, `runtime::config_edits_apply_at_the_next_scan_without_restart` |
| Offline route and log CLI; usage errors; unparsable config blocks edits | `cli::add_list_remove_round_trip`, `cli::defers_field_rules_to_the_shared_validator`, `cli::refuses_to_edit_an_unparsable_config`, `cli::rejects_duplicates_and_bad_prefixes`, `cli::usage_errors_name_the_usage_block`, `cli::log_prints_tail_filters_by_id_and_spans_rotation` |
| Identity binding gates intake and search | `runtime::identity_change_blocks_intake`, `lark::identity_missing_is_fatal`, `lark::identity_refresh_requires_a_successful_authenticated_lookup`, `lark::search_requires_bound_identity`, `lark::permission_evidence_is_not_inferred_from_login` |
| Instance lock conflicts | `lock::second_lock_conflicts` |
| Log restraint and rotation | `logging::log_appends_and_rotates`, `runtime::quiet_scans_stay_out_of_the_log` |

## Known coverage gaps

Automated coverage deliberately stops where the mock cannot reach reality.
These are live-acceptance responsibilities and stay labeled pending until a
real run proves them:

- Search recall and indexing-delay bounds against the real API (release
  acceptance in [product](product.md)).
- Real thread placement of replies and reaction visibility in Feishu
  clients; permission and scope behavior per account.
- A failed first command does not create the chat-session mapping: the
  exit-code gate is enforced in code, but no dedicated automated test
  guards it yet.
- Feishu adapter boundary tests exec a fake CLI with a Unix exec
  convention, so Windows CI covers the rest of the suite and the adapter's
  envelope handling there is pending live validation
  ([DESIGN](DESIGN.md#desktop-build)).

A new user story lands in this document in the same change as its test.
