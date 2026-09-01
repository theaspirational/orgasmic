export type BoardEntry = {
  id: string;
  path: string;
  branch: string;
  status: string;
};

export type SkillSummary = {
  id: string;
  title: string;
  description?: string | null;
  triggers: string[];
  absolute_path?: string | null;
  source_path: string;
};

export type PromptSpecSummary = {
  id: string;
  kind: string;
  version?: string | null;
  default_renderer?: string | null;
  output_contract?: string | null;
  extends?: string | null;
  uses_parts: string[];
  context_packs: string[];
  source_path: string;
  section_titles: string[];
  source: string;
};

export type PromptPartSummary = {
  id: string;
  target_section: string;
  version?: string | null;
  source_path: string;
  preview: string;
  body: string;
  source: string;
};

export type ContextPackSummary = {
  id: string;
  source_kind: string;
  version?: string | null;
  render_policy?: string | null;
  source_path: string;
  preview: string;
};

export type PromptDiagnostic = {
  level: string;
  message: string;
  source_path?: string | null;
  section?: string | null;
};

export type PromptSourceMapEntry = {
  section: string;
  source_kind: string;
  item_id: string;
  source_path: string;
};

export type CompiledPrompt = {
  spec: PromptSpecSummary;
  renderer: string;
  text: string;
  diagnostics: PromptDiagnostic[];
  included_parts: string[];
  included_context_packs: string[];
  source_map: PromptSourceMapEntry[];
  char_count: number;
  approx_tokens: number;
};

export type LifecycleStage =
  | 'backlog'
  | 'todo'
  | 'in_progress'
  | 'in_review'
  | 'done'
  | 'cancelled';

export const LIFECYCLE_STAGES: LifecycleStage[] = [
  'backlog',
  'todo',
  'in_progress',
  'in_review',
  'done',
  'cancelled',
];

export const LIFECYCLE_ACTIVE_STAGES: LifecycleStage[] = [
  'backlog',
  'todo',
  'in_progress',
  'in_review',
  'done',
];

export const LIFECYCLE_STAGE_LABELS: Record<LifecycleStage, string> = {
  backlog: 'Backlog',
  todo: 'Todo',
  in_progress: 'In Progress',
  in_review: 'In Review',
  done: 'Done',
  cancelled: 'Cancelled',
};

export function lifecycleStageLabel(stage: LifecycleStage | string | null | undefined): string {
  if (!stage) return 'Unknown';
  return (
    (LIFECYCLE_STAGE_LABELS as Record<string, string>)[stage] ??
    stage
      .split(/[_\s-]+/)
      .filter(Boolean)
      .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
      .join(' ')
  );
}

export type TaskOwner = string;

export type TaskSummary = {
  id: string;
  title: string;
  lifecycle_stage: LifecycleStage | string;
  parent_task?: string | null;
  owner: TaskOwner;
  run_id?: string | null;
  priority?: string | null;
  blocked_by?: string[] | string | null;
  tags: string[];
  last_updated?: string | null;
  source_file: string;
};

export type AcceptanceState = 'checked' | 'partial' | 'unchecked';

export type AcceptanceItem = {
  state: AcceptanceState;
  text: string;
};

export type TaskBody = {
  description: string;
  acceptance_criteria: AcceptanceItem[];
  evidence: string[];
  notes: string;
  worklog: string[];
  reviewer_pass: string[];
};

export type TaskDetail = TaskSummary & {
  body: TaskBody;
};

export type ActivityKind = 'comment' | 'state_transition' | 'run_lifecycle';

export type ActivityEntry = {
  tx_id: string;
  time: string;
  kind: ActivityKind | string;
  actor: string;
  body: string;
  artifacts: string[];
  in_reply_to?: string | null;
  edited_by?: string | null;
  edited_at?: string | null;
  deleted_by?: string | null;
  deleted_at?: string | null;
};

export type TaskCommentRequest = {
  /** Optional admin scripting override. Member comments are always stamped
   * from the authenticated session and the UI intentionally omits this. */
  actor?: string;
  body: string;
  run_id?: string | null;
  artifacts?: string[];
  in_reply_to?: string | null;
};

export type TaskSubtaskRequest = {
  title: string;
  description?: string | null;
};

export type ProjectIndex = {
  project_id: string;
  root: string;
  repo_url: string;
  branch: string;
  status: string;
  tasks: TaskSummary[];
  graph: GraphIndex;
  last_loaded_at?: string | null;
};

export type ProjectLoadState = 'unloaded' | 'loading' | 'ready' | 'failed' | 'delayed';

export type ProjectCatalogEntry = {
  project_id: string;
  root: string;
  repo_url: string;
  branch: string;
  status: string;
  load: {
    state: ProjectLoadState;
    generation: number;
    last_attempt_at?: string | null;
    last_loaded_at?: string | null;
    cooldown_until?: string | null;
    error?: string | null;
  };
  task_stats?: {
    total: number;
    active: number;
    blocked: number;
    done: number;
  } | null;
};

export type DecisionSummary = {
  id: string;
  title: string;
  tags: string[];
  parent?: string | null;
  children?: string[];
  depth?: number | null;
  path?: string | null;
  glossary_refs: string[];
  decided_at?: string | null;
  preview?: string | null;
  source_file: string;
  superseded?: boolean;
};

export type GlossarySummary = {
  id: string;
  canonical?: string | null;
  avoid?: string | null;
  relates_to: string[];
  definition?: string | null;
  source_file: string;
};

export type GraphNodeSummary = {
  id: string;
  layer: string;
  outgoing: string[];
  source_file: string;
  superseded?: boolean;
};

export type GraphIndex = {
  decisions: DecisionSummary[];
  glossary: GlossarySummary[];
  nodes: GraphNodeSummary[];
};

export type ParseError = {
  project_id?: string | null;
  path: string;
  message: string;
  line?: number | null;
  at: string;
};

export type ParseErrorCoverage = {
  state: 'complete' | 'partial' | 'unknown';
  detail: string | null;
  failures: Record<string, string>;
};

export type ParseErrorsResult = {
  errors: ParseError[];
  coverage: ParseErrorCoverage;
};

export type TxResult = {
  records: TxRecord[];
  coverage: ParseErrorCoverage;
};

export type DaemonStatus = {
  name: string;
  version: string;
  runtime_version?: string;
  boot_id: string;
  pid: number;
  started_at: string;
  home: string;
  machine?: string;
  bind_host?: string;
  bind_port?: number;
  local_only?: boolean;
  ui_asset_hash?: string;
  projects: number;
  registered_projects?: number;
  unloaded_projects?: string[];
  loading_projects?: string[];
  ready_projects?: string[];
  delayed_projects?: Record<string, string>;
  failed_projects?: Record<string, string>;
  parse_errors: number;
  tx_count: number;
  rebuilt_at?: string | null;
  index_refresh?: {
    pending_targets: number;
    in_flight_targets: number;
    stale_blocking_scans: number;
    scan_timeout_ms: number;
    requests_total: number;
    scans_total: number;
    coalesced_total: number;
    discarded_total: number;
    last_scan_duration_ms: number;
    max_scan_duration_ms: number;
  };
};

export type FilesystemRoot = {
  path: string;
  display_name: string;
  kind: string;
};

export type FilesystemEntry = {
  path: string;
  display_name: string;
  kind: string;
  accessible: boolean;
  orgasmic_project: boolean;
  project_id?: string | null;
};

export type FilesystemValidateProjectResponse = {
  path: string;
  exists: boolean;
  is_directory: boolean;
  orgasmic_project: boolean;
  project_id?: string | null;
  default_branch?: string | null;
};

export type RecoveryAction = {
  kind: 'reattach_tmux' | 'resume_native_fork' | 'start_recovery_run' | string;
  label: string;
  target: 'manager' | 'worker' | string;
};

/** What would clear a `recovery_unobserved` refusal. Mirrors the daemon's
 *  `orgasmic_daemon::recovery_claim::Remediation::class()`. */
export type RecoveryRemediation =
  | 'repair_session_file'
  | 'repair_session_store'
  | 'repair_auth_key'
  | 'repair_claim_store'
  | string;

export type RecoveredRun = {
  run_id: string;
  runtime_id: string;
  boot_id: string;
  session_path: string;
  classification: string;
  reason: string;
  recovery_actions?: RecoveryAction[];
  /** Set when the daemon REFUSED to decide recovery authority rather than
   *  finding nothing: the origin enumeration did not complete.
   *  orgasmic:TASK-2QK4P.1.1.1.1 F3 — the refusal is project-wide and does not
   *  self-repair, so the operator surface has to name the file and the fix. */
  recovery_unobserved?: string | null;
  /** Sanitized, project-relative identity of the file the observation failed
   *  on. Never an absolute host path. */
  recovery_unobserved_subject?: string | null;
  recovery_unobserved_remediation?: RecoveryRemediation | null;
  /** The actions this record would have offered had the enumeration completed.
   *  Present only alongside `recovery_unobserved`; NOT actionable. */
  suppressed_recovery_actions?: RecoveryAction[];
};

/** The operator-facing repair for each remediation class, kept beside the type
 *  so the UI never has to invent one. The daemon ships the same sentence in the
 *  503 body's `remediation_hint`; this is the fallback for the inventory list,
 *  where only the class travels. */
export const RECOVERY_REMEDIATION_HINTS: Record<string, string> = {
  repair_session_file:
    'The named session file could not be read as a complete event log. Restore read access to it, or move that one file out of the project\'s .orgasmic/sessions/ directory to quarantine it; recovery resumes on the next request.',
  repair_session_store:
    'The project\'s .orgasmic/sessions/ directory could not be opened. Restore read and execute access to it and retry.',
  repair_auth_key:
    'The daemon could not read its host auth material at <home>/auth/token. Restore read access to that file — do not delete or regenerate it, which would invalidate every live recovery claim.',
  repair_claim_store:
    'The daemon-owned claim store under <home>/state/recovery-claims/ could not be opened, listed or read. Restore read and execute access to it and retry.',
};

/** One line an operator can act on: what failed, on which file, and the fix. */
export function recoveryUnobservedNotice(run: RecoveredRun): string | null {
  if (!run.recovery_unobserved) return null;
  const subject = run.recovery_unobserved_subject ?? 'an unnamed file';
  const remediation = run.recovery_unobserved_remediation ?? '';
  const hint = RECOVERY_REMEDIATION_HINTS[remediation] ?? 'Retry once the file is readable.';
  return `Recovery authority unresolved (${run.recovery_unobserved}) at ${subject}. ${hint}`;
}

/// What one run-inventory pass touched. Counts and byte totals only — never
/// session contents. Present so the run list can show that enumeration cost
/// tracks record count rather than transcript size.
export type InventoryStageMetrics = {
  session_files: number;
  session_file_bytes: number;
  bytes_inspected: number;
  truncated_scans: number;
  unreadable_sessions: number;
  attach_probes_started: number;
  attach_probes_timed_out: number;
  origin_index_files: number;
  origin_index_bytes_inspected: number;
  interrupted: number;
  reattached: number;
  failed_recoverable: number;
  terminal_noop: number;
  ambiguous: number;
  duration_ms: number;
};

export type RecoveryStatus = {
  boot_id: string;
  acquisition_paused: boolean;
  live_runs: RunSummary[];
  interrupted_runs: RecoveredRun[];
  reattached_runs: RecoveredRun[];
  terminal_noop_runs: RecoveredRun[];
  ambiguous_runs: RecoveredRun[];
  inventory?: InventoryStageMetrics;
  note: string;
};

export type RuntimeIdentity = {
  run_id: string;
  runtime_id: string;
  boot_id: string;
};

export type RunSummary = {
  run_id: string;
  task_id: string;
  /// Run surface.
  kind: string;
  worker_id?: string | null;
  /// Who is working right now — the resolved worker's kind
  /// ('implementer', 'reviewer', 'manager', …).
  role?: string | null;
  driver?: string | null;
  harness?: string | null;
  project_id?: string | null;
  sub_state?: string | null;
  identity: RuntimeIdentity;
  session_path: string;
  event_count: number;
  /**
   * Present for an app terminal that has atomically taken the project's
   * manager lease. This is intentionally only a display-state bit: the
   * provider and the run-scoped claim capability never leave the daemon.
   */
  claimed_manager?: boolean;
};

export type ManagerState = {
  acquisition_paused: boolean;
  runs: RunSummary[];
};

export type ManagerDriverProfile = {
  mode: string;
  harness: string;
  binary: string;
  display_name: string;
  mode_label: string;
  harness_label: string;
  installed: boolean;
};

export type ManagerChatCatalogModel = {
  id: string;
  label: string;
  legacy: boolean;
  reasoning_efforts: string[];
};

export type ManagerChatCatalogProvider = {
  id: 'codex' | 'claude' | 'opencode';
  source: string;
  models: ManagerChatCatalogModel[];
  message?: string | null;
};

export type ManagerChatCatalogResponse = {
  providers: ManagerChatCatalogProvider[];
};

export type ManagerDriversResponse = {
  drivers: ManagerDriverProfile[];
};

export type ManagerLaunchResponse = {
  run_id: string;
};

/** `GET /runs/live` — supervisor-local liveness. No durable history is read to
 * produce it, so it stays correct (and cheap) on a board whose session files
 * are large, slow, or unreadable. */
export type LiveRunsResponse = {
  boot_id: string;
  acquisition_paused: boolean;
  live: RunSummary[];
};

/** `GET /runs` — the crash-recovery inventory, not a general run list. Every
 * bucket but `live` is classified by scanning durable session JSONL across the
 * whole board. Use {@link LiveRunsResponse} when only live state is wanted. */
export type RecoveryInventoryResponse = {
  live: RunSummary[];
  interrupted: RecoveredRun[];
  reattached: RecoveredRun[];
  /** The bucket `GET /runs` has always serialized and this type used to omit.
   *  orgasmic:TASK-2QK4P.1.1.1.1.1 P1a — it is the ONLY bucket the daemon
   *  decorates with `recovery_unobserved*`, so omitting it here dropped the F3
   *  operator diagnostic AND every permanently refused recovery out of the run
   *  table. `tsc` cannot catch that: a field the backend sends and this type
   *  omits is not a type error, so the pin is a rendering test, not a
   *  typecheck. */
  failed_recoverable: RecoveredRun[];
  terminal_noop: RecoveredRun[];
  ambiguous: RecoveredRun[];
  inventory?: InventoryStageMetrics;
};

export type RunDetailResponse = {
  classification?: string;
  source: string;
  run: RunSummary | RecoveredRun;
};

export type RunInputResponse = {
  run_id: string;
  accepted: boolean;
  message?: string | null;
};

export type RuntimeSpeed = 'normal' | 'fast';

export type RunRuntimeOptionsRequest = {
  provider?: string | null;
  model?: string | null;
  reasoning_effort?: string | null;
  speed?: RuntimeSpeed | null;
};

export type RunRuntimeOptionsResponse = {
  run_id: string;
  accepted: boolean;
  message?: string | null;
};

export type RuntimeOptionsState = {
  provider?: string | null;
  model?: string | null;
  reasoning_effort?: string | null;
  speed?: RuntimeSpeed | null;
};

export type RuntimeModelOption = {
  id: string;
  label: string;
  provider?: string | null;
  current: boolean;
  reasoning_efforts: string[];
  speeds: RuntimeSpeed[];
  default_reasoning_effort?: string | null;
};

export type RuntimeProviderOption = {
  id: string;
  label: string;
  current: boolean;
  authenticated?: boolean | null;
  models: RuntimeModelOption[];
};

export type RuntimeOptionsCatalog = {
  source: string;
  provider_switching: boolean;
  live_switching?: boolean;
  current: RuntimeOptionsState;
  providers: RuntimeProviderOption[];
  models: RuntimeModelOption[];
  efforts: string[];
  speeds: RuntimeSpeed[];
};

export type RunRuntimeOptionsCatalogResponse = {
  run_id: string;
  catalog: RuntimeOptionsCatalog;
};

export type ManagerSize = 'peek' | 'workbench' | 'focus';

export type RunRecoverRequest = {
  action?: string;
  project?: string | null;
  request_id?: string;
  force_inert?: boolean;
};

export type RunRecoverResponse = {
  run_id: string;
  runtime_id: string;
  boot_id: string;
  session_path: string;
  action: string;
  target: 'manager' | 'worker' | string;
  draft_prompt?: string | null;
};

export type OrgFileResponse = {
  project: string;
  path: string;
  contents: string;
  tx_id?: string;
};

export type TxRecord = {
  project_id?: string | null;
  source_path: string;
  entry: {
    tx_id: string;
    ty: string;
    time: string;
    actor: string;
    machine: string;
    project?: string | null;
    task?: string | null;
    target?: string | null;
    reason?: string | null;
    extra: [string, string][];
  };
};

export type QuestionEntry = {
  tx_id: string;
  question_id: string;
  task_id?: string | null;
  reason?: string | null;
  time: string;
};

export type DaemonTopic = 'board' | 'task' | 'run' | 'manager' | 'graph' | 'daemon' | 'artifact';

export type ArtifactSummary = {
  id: string;
  title: string;
  subject_nodes: string[];
  version: number;
  state: string;
  open_comment_count: number;
  launch_mode?: string | null;
  launch_harness?: string | null;
  launch_harness_args?: string[] | null;
  launch_model?: string | null;
  launch_effort?: string | null;
};

export type CommentRecord = {
  cid: string;
  author: string;
  version: number;
  anchor: string;
  resolution_target: string;
  /** CID this comment replies to; empty for a top-level comment. */
  reply_to: string;
  resolved: boolean;
  consumed: boolean;
  message: string;
};

export type ArtifactDetail = ArtifactSummary & {
  prompt: string;
  content: string;
  comments: CommentRecord[];
};

export type ArtifactCommentRequest = {
  message: string;
  /** Optional selection anchor captured from the rendered artifact (pin). */
  anchor?: string;
  resolution_target?: string;
  /** CID this comment replies to (threaded reply); omit for a top-level comment. */
  reply_to?: string;
};

export type ArtifactCommentResolveResponse = {
  cid: string;
  resolved: boolean;
};

/** Action-name capability strings the daemon grants per member/project. */
export type MemberCapability =
  | 'project.read'
  | 'graph.read'
  | 'tasks.read'
  | 'tasks.comment'
  | 'sessions.watch'
  | 'sessions.interact'
  | 'artifacts.read'
  | 'artifacts.comment'
  | 'artifacts.generate'
  | 'org.write'
  | 'members.manage';

export type MeIdentity = 'admin' | 'member';

export type MeProject = {
  projectId: string;
  role: string;
  capabilities: string[];
};

/** GET /me capability snapshot. Admin lists every project with every
 * capability; a member lists only their granted projects. */
export type Me = {
  identity: MeIdentity;
  name: string | null;
  projects: MeProject[];
};

export type GovernancePatch = {
  sandbox_permissions?: {
    allow_exec?: boolean | null;
    allow_patch?: boolean | null;
    allow_network?: boolean | null;
    allow_writes_outside_cwd?: boolean | null;
  } | null;
  max_iterations?: number | null;
  context_budget_chars?: number | null;
  linked_skills?: string[] | null;
  applicable_states?: string[] | null;
  stall_timeout_secs?: number | null;
  max_run_duration_secs?: number | null;
};

export type ArtifactGenerateRequest = {
  nodes: string[];
  prompt: string;
  mode: string;
  harness: string;
  harness_args?: string[];
  model?: string | null;
  effort?: string | null;
  governance?: GovernancePatch | null;
};

export type ArtifactGenerateResponse = {
  artifact_id: string;
  run_id: string;
};

export type ArtifactRegenerateRequest = {
  extraPrompt?: string;
  mode?: string;
  harness?: string;
  harness_args?: string[];
  model?: string | null;
  effort?: string | null;
  governance?: GovernancePatch | null;
};

export type DaemonEvent = {
  seq: number;
  time: string;
  topic: DaemonTopic;
  payload: { kind: string; [key: string]: unknown };
};

export type ViewName =
  | 'board'
  | 'decisions'
  | 'glossary'
  | 'activity'
  | 'project'
  | 'tasks'
  | 'task'
  | 'runs'
  | 'prompts'
  | 'manager'
  | 'org'
  | 'status'
  | 'settings'
  | 'artifacts';

export type TasksLayout = 'list' | 'kanban';

export type WsConnectionState = 'connecting' | 'open' | 'reconnecting' | 'closed';
