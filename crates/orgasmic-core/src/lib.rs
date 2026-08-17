//! orgasmic-core: parsing, schema, file formats.
//!
//! This crate owns the app-owned Org dialect parser, the typed schema views
//! built on top of it, the property-drawer-only tx writer, the append-only
//! JSONL session writer, and the strict prompt-slot compiler.
//!
//! Downstream crates (daemon, drivers, cli) depend on orgasmic-core for every
//! piece of durable state they touch.

pub mod home;
pub mod id;
pub mod id_repair;
pub mod identity_lint;
pub mod members;
pub mod node_kind;
pub mod org;
pub mod paths;
pub mod projects;
pub mod run_id;
// orgasmic:dec_WDR5K — residue left behind by hard cutovers
pub mod retired;
pub mod sandbox;
pub mod schema;
pub mod schema_examples;
pub mod session;
pub mod slots;
pub mod tx;
pub use home::{resolve_loader, Home, HomeError};
pub use id::{
    is_dec_id, is_legacy_sequential_create_id, is_minted_stem, is_valid_greenfield_arch_id,
    is_valid_greenfield_artifact_id, is_valid_greenfield_dec_id, is_valid_greenfield_identity,
    is_valid_greenfield_task_id, is_valid_greenfield_term_id, is_valid_task_path_id,
    looks_like_legacy_numeric_task, mint_node_id, node_id_class_by_prefix, parse_parent_value,
    validate_parent_exists, validate_parent_pointer, validate_parent_tree, NodeIdClass, ParentTree,
    ParentTreeError, ParentTreeNode, CROCKFORD,
};
pub use id_repair::{repair_id_collisions, repair_id_collisions_with_incoming, IdRepairError};
pub use identity_lint::{
    collect_identity_occurrences, duplicate_id_groups, lint_decision_heading_id_token,
    lint_project_identities, lint_task_heading_id_token, unresolved_reference_tokens,
    IdentityLintFinding, IdentityLintKind, REFERENCE_PROPERTY_KEYS,
};
pub use members::{
    add_member, find_member_by_name, find_member_by_token, read_members, revoke_member, sha256_hex,
    MemberEntry,
};
pub use node_kind::NodeKind;
pub use org::{
    body_heading_lines, wrap_raw_body, Heading, HeadingLine, HeadingLineEdit, OrgError, OrgFile,
    OrgRewriter, PropertyDrawer, PropertyEntry,
};
pub use paths::{
    dispatch_record_dir, dispatch_record_report_rel, dotorg_tasks_dir, goal_file_path,
    goal_file_rel, handoff_file_path, iter_task_file_paths, lifecycle_stage_file_name,
    project_dispatch_dir, project_dispatch_records_dir, project_sessions_dir, project_tmp_dir,
    promote_validated_dispatch_attempt, prune_dispatch_stem_after_worktree,
    prune_validated_dispatch_attempt, task_file_path, task_file_rel,
    validate_dispatch_cleanup_targets, validate_dispatch_promote_targets,
    verify_dispatch_worktree_identity, DispatchAttemptArtifacts, PromoteOutcome, DEFAULT_TASK_FILE,
    DEFAULT_TASK_FILE_REL, GOAL_FILE, HANDOFF_FILE, STDOUT_PROMOTE_MAX_BYTES, TASKS_DIR,
    TASK_FILE_NAMES,
};
pub use retired::{RetiredContent, RETIRED_CONTENT};
pub use run_id::{compact_run_id_token, mint_run_id, run_id_timestamp_millis};
pub use sandbox::{SandboxAllowlist, SandboxAllowlistParseError};
pub use schema::{
    DecisionNode, GlossaryTerm, LifecycleStage, ProjectFile, SchemaError, SkillMetadata,
    TaskHeading, TxHeadingView, WorkerKind,
};
pub use session::{
    bound_driver_event_payload, driver_event_total_cap, read_session_file, scan_session_lifecycle,
    scan_session_lifecycle_complete, scan_session_lifecycle_complete_reader,
    scan_session_lifecycle_reader, BoundedDriverEvent, DriverEvent, Lifecycle,
    ProviderContentDeltaPayload, ProviderDiagnosticPayload, ProviderItemLifecyclePayload,
    ProviderRequestPayload, ProviderRuntimeEvent, ProviderRuntimeEventKind,
    ProviderSessionExitedPayload, ProviderSessionStartedPayload, ProviderThreadMetadataPayload,
    ProviderTokenUsagePayload, ProviderTurnAbortedPayload, ProviderTurnCompletedPayload,
    ProviderTurnStartedPayload, ProviderUserInputPayload, ReleaseOutcome, RunSubState,
    RuntimeIdentity, SessionEnvelope, SessionError, SessionEventKind, SessionLifecycleScan,
    SessionScanBudget, SessionWriter, TextStream, WorkerTool, DRIVER_EVENT_PAYLOAD_CAP_BYTES,
    RETENTION_TIERS,
};
pub use slots::{
    compile as compile_slots, default_registry as default_slot_registry, dry_run as slot_dry_run,
    scan as scan_slots, DryRunReport, SlotError, SlotRef, SlotValues,
};
pub use tx::{parse_tx_file, TxEntry, TxError, TxWriter};
