//! orgasmic-core: parsing, schema, file formats.
//!
//! This crate owns the app-owned Org dialect parser, the typed schema views
//! built on top of it, the property-drawer-only tx writer, the append-only
//! JSONL session writer, and the strict prompt-slot compiler.
//!
//! Downstream crates (daemon, drivers, cli) depend on orgasmic-core for every
//! piece of durable state they touch. See `.orgasmic/architecture.org`
//! (arch_002, arch_003, arch_011) for the design contract.

// arch: arch_004 — see decisions dec_016
pub mod eligibility;
pub mod id;
pub mod id_repair;
pub mod identity_lint;
pub mod marker;
// arch: arch_003 — see decisions dec_017 dec_019
pub mod home;
pub mod members;
pub mod node_kind;
pub mod org;
pub mod paths;
pub mod projects;
pub mod sandbox;
pub mod schema;
pub mod schema_examples;
pub mod session;
pub mod slots;
pub mod tx;
pub mod workers;

pub use eligibility::{
    compute_all, compute_eligibility, list_allows, TaskConstraints, WorkerEligibility,
};
pub use home::{resolve_loader, Home, HomeError};
pub use id::{
    is_arch_id, is_dec_id, is_legacy_sequential_create_id, is_minted_stem,
    is_valid_greenfield_arch_id, is_valid_greenfield_artifact_id, is_valid_greenfield_dec_id,
    is_valid_greenfield_identity, is_valid_greenfield_task_id, is_valid_greenfield_term_id,
    is_valid_task_path_id, looks_like_legacy_numeric_task, mint_node_id, node_id_class_by_prefix,
    parse_parent_value, validate_parent_exists, validate_parent_pointer, validate_parent_tree,
    NodeIdClass, ParentTree, ParentTreeError, ParentTreeNode, CROCKFORD,
};
pub use id_repair::{repair_id_collisions, repair_id_collisions_with_incoming, IdRepairError};
pub use identity_lint::{
    collect_identity_occurrences, duplicate_id_groups, lint_arch_heading_id_token,
    lint_decision_heading_id_token, lint_project_identities, lint_task_heading_id_token,
    unresolved_reference_tokens, IdentityLintFinding, IdentityLintKind, REFERENCE_PROPERTY_KEYS,
};
pub use marker::{
    has_comment_token_before_marker, is_marker_id_byte, is_structured_marker_payload,
    marker_node_ids_in_line, normalize_marker_node_id, parse_marker_payload,
    should_skip_marker_path,
};
pub use members::{
    add_member, find_member_by_name, find_member_by_token, read_members, revoke_member, sha256_hex,
    MemberEntry,
};
pub use node_kind::NodeKind;
pub use org::{
    wrap_raw_body, Heading, OrgError, OrgFile, OrgRewriter, PropertyDrawer, PropertyEntry,
};
pub use paths::{
    dotorg_tasks_dir, goal_file_path, goal_file_rel, handoff_file_path, iter_task_file_paths,
    lifecycle_stage_file_name, project_dispatch_dir, project_sessions_dir, project_tmp_dir,
    prune_dispatch_stem_after_worktree, task_file_path, task_file_rel, DEFAULT_TASK_FILE,
    DEFAULT_TASK_FILE_REL, GOAL_FILE, HANDOFF_FILE, TASKS_DIR, TASK_FILE_NAMES,
};
pub use sandbox::{SandboxAllowlist, SandboxAllowlistParseError};
pub use schema::{
    ArchEdge, ArchEdgeKind, ArchEdgeTarget, ArchNode, ArchitectureNode, ArtifactNode,
    ArtifactScheme, DecisionNode, GlossaryTerm, LifecycleStage, ProjectConfig, ProjectFile,
    SchemaError, SkillMetadata, TaskHeading, TxHeadingView, WorkerKind,
};
pub use session::{
    read_session_file, BabysitterSummaryChunk, BabysitterTool, DriverEvent, Lifecycle,
    ReleaseOutcome, RunSubState, RuntimeIdentity, SessionEnvelope, SessionError, SessionEventKind,
    SessionWriter, TextStream, WorkerTool,
};
pub use slots::{
    compile as compile_slots, default_registry as default_slot_registry, dry_run as slot_dry_run,
    scan as scan_slots, DryRunReport, SlotError, SlotRef, SlotValues,
};
pub use tx::{parse_tx_file, TxEntry, TxError, TxWriter};
pub use workers::{
    is_supported_worker_pair, parse_string_list, Worker, WorkerError,
    SUPPORTED_WORKER_DRIVER_HARNESSES,
};
