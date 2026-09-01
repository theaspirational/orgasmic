## Changed
- `crates/orgasmic-daemon/src/api.rs:14496-14508` now extracts `Identity` and authorizes `Action::OrgWrite` before org-file path validation or project loading.
- `crates/orgasmic-daemon/src/api.rs:14572-14598` now rejects exact path components under `.orgasmic/machines/`, `.orgasmic/views/`, and `.orgasmic/tx/`, plus any `.orgasmic/**/journal.org`; `.orgasmic/gotchas.org` and the prefix collision `.orgasmic/tx-notes.org` remain allowed.
- `crates/orgasmic-daemon/src/api.rs:21221-21305` extends the existing denylist pin and adds under-privileged-before-path-validation and authorized-write success tests.
- `crates/orgasmic-daemon/src/authz.rs:17-64` adds `Action::OrgWrite` / `org.write`. No member role receives it, matching the existing admin-only role floor of whole-node/org-node writes; `MembersManage` had the same floor but the wrong domain meaning.
- Commit: `84bda242dde0ad7ae3e97cf7572986d9bf77bf0a`.

## Verification Gates
- `cargo test -p orgasmic-daemon --lib org_file` — 5 passed, 0 failed (`/tmp/TASK-MSYN4.1-org_file.log`):
  - `api::tests::org_file_artifact_label_matches_allowed_artifacts`
  - `api::tests::org_file_rewrite_refuses_ledger_paths`
  - `prompt_compiler::tests::latest_org_file_ignores_non_monthly_ledger_names`
  - `api::tests::authz_org_file_write_refuses_member_before_path_validation`
  - `api::tests::org_file_write_allows_admin_on_an_allowed_path`
- `cargo test -p orgasmic-daemon --lib authz` — 19 passed, 0 failed (`/tmp/TASK-MSYN4.1-authz.log`):
  - `authz::tests::admin_bypasses_every_check`
  - `authz::tests::allowed_topics_admin_gets_everything`
  - `authz::tests::allowed_topics_viewer_gets_board_task_graph_artifact`
  - `authz::tests::allowed_topics_artifacts_role_is_artifact_only`
  - `authz::tests::artifacts_role_is_scoped_to_artifacts_only`
  - `authz::tests::event_visible_checks_topic_then_project`
  - `authz::tests::exact_project_grant_beats_wildcard`
  - `authz::tests::event_visible_passes_project_less_payloads_when_topic_allowed`
  - `authz::tests::member_action_without_project_always_fails`
  - `authz::tests::no_matching_grant_is_forbidden`
  - `authz::tests::unknown_role_grants_nothing`
  - `authz::tests::viewer_lacks_generate_editor_has_it`
  - `authz::tests::visible_projects_wildcard_sees_everything`
  - `authz::tests::visible_projects_filters_to_grants_admin_sees_all`
  - `api::tests::authz_board_and_projects_lists_filtered_to_member_grants`
  - `api::tests::authz_org_file_write_refuses_member_before_path_validation`
  - `api::tests::authz_viewer_member_reads_tasks_and_graph`
  - `api::tests::authz_artifacts_member_gated_on_tasks_and_graph_reads`
  - `api::tests::authz_projectless_task_resolution_never_loads_an_ungranted_project`
- `cargo clippy -p orgasmic-daemon --all-targets -- -D warnings` — passed (`/tmp/TASK-MSYN4.1-clippy.log`).
- `cargo fmt --all --check` — passed (`/tmp/TASK-MSYN4.1-fmt.log`).

## Unmet Criteria
- None.

## Residual Risk
- The focused gates requested by the brief passed. The full workspace suite and a separately booted live HTTP probe were not run, per the targeted-only verification scope.
- `GET /org/file` and `writer::guard_node_write` were not changed.
