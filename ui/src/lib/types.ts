// @arch arch_MK2Q2.2
export type BoardEntry = {
  id: string;
  path: string;
  branch: string;
  status: string;
};

export type WorkerSummary = {
  id: string;
  kind: string;
  driver: string;
  harness: string;
};

export type WorkerValidationDiagnostic = {
  code: string;
  message: string;
};

export type WorkerValidationResult = {
  id?: string | null;
  source_path?: string | null;
  ok: boolean;
  errors: WorkerValidationDiagnostic[];
  worker?: WorkerSummary | null;
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
  worker?: string | null;
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
};

export type TaskCommentRequest = {
  actor: string;
  body: string;
  run_id?: string | null;
  artifacts?: string[];
  in_reply_to?: string | null;
};

export type TaskSubtaskRequest = {
  title: string;
  worker?: string | null;
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

export type ArchitectureSummary = {
  id: string;
  label: string;
  motivated_by: string[];
  glossary_refs: string[];
  interface: string[];
  constraints: string[];
  depends_on: string[];
  source_paths?: string[];
  tests?: string[];
  parent_id?: string | null;
  description?: string | null;
  source_file: string;
};

export type ArchitectureArtifactSummary = {
  id: string;
  kind: string;
  scheme: string;
  name: string;
};

export type ArchitectureGraphNode = {
  id: string;
  kind: string;
  label?: string | null;
  parent_id?: string | null;
  source_paths?: string[];
  tests?: string[];
  scheme?: string | null;
  name?: string | null;
};

export type ArchitectureEdgeSummary = {
  kind: string;
  from: string;
  to: string;
};

export type ArchitectureNodesResponse = {
  nodes: ArchitectureGraphNode[];
  edges: ArchitectureEdgeSummary[];
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
  architecture: ArchitectureSummary[];
  glossary: GlossarySummary[];
  nodes: GraphNodeSummary[];
};

export type ParseError = {
  path: string;
  message: string;
  line?: number | null;
  at: string;
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
  parse_errors: number;
  tx_count: number;
  rebuilt_at?: string | null;
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

export type RecoveredRun = {
  run_id: string;
  runtime_id: string;
  boot_id: string;
  session_path: string;
  classification: string;
  reason: string;
  recovery_actions?: RecoveryAction[];
};

export type RecoveryStatus = {
  boot_id: string;
  acquisition_paused: boolean;
  live_runs: RunSummary[];
  interrupted_runs: RecoveredRun[];
  reattached_runs: RecoveredRun[];
  terminal_noop_runs: RecoveredRun[];
  ambiguous_runs: RecoveredRun[];
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
  /// Run surface: 'worker' | 'babysitter'.
  kind: string;
  worker_id?: string | null;
  /// Who is working right now — the resolved worker's kind
  /// ('implementer', 'reviewer', 'babysitter', 'manager', …).
  role?: string | null;
  driver?: string | null;
  harness?: string | null;
  project_id?: string | null;
  sub_state?: string | null;
  identity: RuntimeIdentity;
  session_path: string;
  babysitter_target?: string | null;
  event_count: number;
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
  /** Mode-level binary requirement (e.g. rmux needs a separately provisioned
   * `rmux` daemon binary), checked independently of the harness binary. */
  mode_binary?: string | null;
  /** Whether {@link mode_binary} resolves. Absent for modes with no extra
   * binary requirement. */
  mode_installed?: boolean | null;
};

export type ManagerDriversResponse = {
  drivers: ManagerDriverProfile[];
};

export type ManagerLaunchResponse = {
  run_id: string;
};

export type RunsResponse = {
  live: RunSummary[];
  interrupted: RecoveredRun[];
  reattached: RecoveredRun[];
  terminal_noop: RecoveredRun[];
  ambiguous: RecoveredRun[];
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
  | 'sessions.watch'
  | 'sessions.interact'
  | 'artifacts.read'
  | 'artifacts.comment'
  | 'artifacts.generate'
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
  babysitter?: {
    mode?: string;
    harness?: string;
    harness_args?: string[];
    model?: string | null;
    effort?: string | null;
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
  | 'architecture'
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
