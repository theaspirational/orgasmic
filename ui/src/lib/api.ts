// @arch arch_MK2Q2.2
import { get, HttpError, post } from './transport';
import type { NodeEditOp, OrgNodeDoc } from './orgdoc/types';
import type {
  ActivityEntry,
  ArchitectureSummary,
  ArchitectureNodesResponse,
  ArtifactCommentRequest,
  ArtifactCommentResolveResponse,
  ArtifactDetail,
  ArtifactGenerateRequest,
  ArtifactGenerateResponse,
  ArtifactRegenerateRequest,
  ArtifactSummary,
  Me,
  BoardEntry,
  DaemonStatus,
  DecisionSummary,
  FilesystemEntry,
  FilesystemRoot,
  FilesystemValidateProjectResponse,
  GlossarySummary,
  ManagerDriversResponse,
  ManagerLaunchResponse,
  ManagerState,
  OrgFileResponse,
  ParseError,
  CompiledPrompt,
  ContextPackSummary,
  PromptPartSummary,
  PromptSpecSummary,
  ProjectIndex,
  RecoveryStatus,
  RunRecoverRequest,
  RunRecoverResponse,
  RunDetailResponse,
  RunInputResponse,
  RunRuntimeOptionsCatalogResponse,
  RunRuntimeOptionsRequest,
  RunRuntimeOptionsResponse,
  RunsResponse,
  SkillSummary,
  TaskCommentRequest,
  TaskDetail,
  TaskSubtaskRequest,
  TaskSummary,
  TxRecord,
  WorkerValidationResult,
  WorkerSummary,
} from './types';

function q(project?: string | null, extra?: Record<string, string | number | undefined>): string {
  const params = new URLSearchParams();
  if (project) params.set('project', project);
  if (extra) {
    for (const [k, v] of Object.entries(extra)) {
      if (v !== undefined && v !== null) params.set(k, String(v));
    }
  }
  const s = params.toString();
  return s ? `?${s}` : '';
}

function requestId(prefix: string): string {
  return `ui-${prefix}-${Date.now().toString(36)}`;
}

export function fetchBoard(): Promise<BoardEntry[]> {
  return get<BoardEntry[]>('/board');
}

export function fetchProjects(): Promise<ProjectIndex[]> {
  return get<ProjectIndex[]>('/projects');
}

export function fetchWorkers(): Promise<WorkerSummary[]> {
  return get<WorkerSummary[]>('/workers');
}

export function fetchWorkerValidation(): Promise<WorkerValidationResult[]> {
  return get<WorkerValidationResult[]>('/workers/validate');
}

export function fetchSkills(): Promise<SkillSummary[]> {
  return get<SkillSummary[]>('/skills');
}

export function fetchPromptSpecs(): Promise<PromptSpecSummary[]> {
  return get<PromptSpecSummary[]>('/prompt-specs');
}

export function fetchPromptSpec(id: string): Promise<PromptSpecSummary> {
  return get<PromptSpecSummary>(`/prompt-specs/${encodeURIComponent(id)}`);
}

export function fetchPromptParts(): Promise<PromptPartSummary[]> {
  return get<PromptPartSummary[]>('/prompt-specs/parts');
}

export function fetchPromptPart(id: string): Promise<PromptPartSummary> {
  return get<PromptPartSummary>(`/prompt-specs/parts/${encodeURIComponent(id)}`);
}

export function savePromptPart(id: string, contents: string): Promise<PromptPartSummary> {
  return post<PromptPartSummary>(`/prompt-specs/parts/${encodeURIComponent(id)}`, { contents });
}

export function fetchContextPacks(): Promise<ContextPackSummary[]> {
  return get<ContextPackSummary[]>('/prompt-specs/context-packs');
}

export function savePromptSpec(id: string, contents: string): Promise<PromptSpecSummary> {
  return post<PromptSpecSummary>(`/prompt-specs/${encodeURIComponent(id)}`, { contents });
}

export function forkPromptSpec(id: string): Promise<PromptSpecSummary> {
  return post<PromptSpecSummary>(`/prompt-specs/${encodeURIComponent(id)}/fork`, {});
}

export function compilePromptSpec(
  id: string,
  body: { project?: string | null; renderer?: string | null; values?: Record<string, string> } = {},
): Promise<CompiledPrompt> {
  return post<CompiledPrompt>(`/prompt-specs/${encodeURIComponent(id)}/compile`, body);
}

export function fetchProject(id: string): Promise<ProjectIndex> {
  return get<ProjectIndex>(`/projects/${encodeURIComponent(id)}`);
}

export type AddProjectRequest = {
  path: string;
  scaffold?: boolean;
};

export function addProject(req: AddProjectRequest): Promise<ProjectIndex> {
  return post<ProjectIndex>('/projects', {
    path: req.path,
    scaffold: req.scaffold ?? false,
  });
}

export function fetchProjectTasks(id: string): Promise<TaskSummary[]> {
  return get<TaskSummary[]>(`/projects/${encodeURIComponent(id)}/tasks`);
}

export function fetchTask(projectId: string, taskId: string): Promise<TaskDetail> {
  return get<TaskDetail>(
    `/projects/${encodeURIComponent(projectId)}/tasks/${encodeURIComponent(taskId)}`,
  );
}

export function fetchTaskActivity(taskId: string): Promise<ActivityEntry[]> {
  return get<ActivityEntry[]>(`/tasks/${encodeURIComponent(taskId)}/activity`);
}

export function postTaskComment(
  taskId: string,
  body: TaskCommentRequest,
): Promise<unknown> {
  return post(`/tasks/${encodeURIComponent(taskId)}/comments`, {
    ...body,
    request_id: requestId(`comment-${taskId}`),
  });
}

export function postTaskSubtask(
  taskId: string,
  body: TaskSubtaskRequest,
): Promise<unknown> {
  return post(`/tasks/${encodeURIComponent(taskId)}/subtasks`, {
    ...body,
    request_id: requestId(`subtask-${taskId}`),
  });
}

export function postRunRelease(runId: string): Promise<unknown> {
  return post(`/runs/${encodeURIComponent(runId)}/release`, {
    request_id: requestId(`release-${runId}`),
  });
}

// True when a run-scoped call failed because the run is no longer live (the
// daemon 404s "active run <id>"). Callers treat this as "already stopped"
// rather than a failure.
export function isRunGoneError(err: unknown): boolean {
  return err instanceof HttpError && err.status === 404;
}

export function postRunInput(runId: string, input: string): Promise<RunInputResponse> {
  return post<RunInputResponse>(`/runs/${encodeURIComponent(runId)}/input`, { input });
}

export function postRunRuntimeOptions(
  runId: string,
  body: RunRuntimeOptionsRequest,
): Promise<RunRuntimeOptionsResponse> {
  return post<RunRuntimeOptionsResponse>(
    `/runs/${encodeURIComponent(runId)}/runtime-options`,
    body,
  );
}

export function fetchRunRuntimeOptions(
  runId: string,
): Promise<RunRuntimeOptionsCatalogResponse> {
  return get<RunRuntimeOptionsCatalogResponse>(
    `/runs/${encodeURIComponent(runId)}/runtime-options`,
  );
}

export function fetchManagerState(): Promise<ManagerState> {
  return get<ManagerState>('/manager/state');
}

export function fetchDaemonStatus(): Promise<DaemonStatus> {
  return get<DaemonStatus>('/daemon/status');
}

export function fetchFilesystemRoots(): Promise<FilesystemRoot[]> {
  return get<FilesystemRoot[]>('/filesystem/roots');
}

export function fetchFilesystemEntries(path: string): Promise<FilesystemEntry[]> {
  return get<FilesystemEntry[]>(`/filesystem/entries${q(null, { path })}`);
}

export function validateFilesystemProject(path: string): Promise<FilesystemValidateProjectResponse> {
  return post<FilesystemValidateProjectResponse>('/filesystem/validate-project', { path });
}

export function fetchRecoveryStatus(): Promise<RecoveryStatus> {
  return get<RecoveryStatus>('/recovery/status');
}

export function fetchRuns(): Promise<RunsResponse> {
  return get<RunsResponse>('/runs');
}

export function fetchRun(id: string): Promise<RunDetailResponse> {
  return get<RunDetailResponse>(`/runs/${encodeURIComponent(id)}`);
}

export function fetchParseErrors(): Promise<ParseError[]> {
  return get<ParseError[]>('/graph/parse-errors');
}

export function fetchWhoami(): Promise<{ authenticated: boolean; boot_id: string }> {
  return get('/auth/whoami');
}

export function fetchTx(project?: string | null, limit = 50): Promise<TxRecord[]> {
  return get<TxRecord[]>(`/tx${q(project, { limit })}`);
}

export function fetchDecisions(project?: string | null): Promise<DecisionSummary[]> {
  return get<DecisionSummary[]>(`/decisions${q(project)}`);
}

export function createDecision(body: {
  project?: string | null;
  title?: string | null;
  properties?: Record<string, string>;
  body?: string | null;
}): Promise<{ id: string; action: string; tx_id: string }> {
  return post('/decisions', {
    ...body,
    request_id: requestId('decision-create'),
  });
}

export function fetchArchitecture(project?: string | null): Promise<ArchitectureSummary[]> {
  return get<ArchitectureSummary[]>(`/architecture${q(project)}`);
}

export function fetchArchitectureNodes(project?: string | null): Promise<ArchitectureNodesResponse> {
  return get<ArchitectureNodesResponse>(`/architecture/nodes${q(project)}`);
}

export function fetchGlossary(project?: string | null): Promise<GlossarySummary[]> {
  return get<GlossarySummary[]>(`/glossary${q(project)}`);
}

export function fetchManagerDrivers(): Promise<ManagerDriversResponse> {
  return get<ManagerDriversResponse>('/managers/drivers');
}

export function postManagerLaunch(body: {
  project_id: string;
  mode: string;
  harness: string;
  /** Launch-time model override, threaded into the harness CLI argv
   * (`claude --model <m>`). Session-pinned — never rewrites the operator's
   * saved harness default the way an in-session `/model` does. */
  model?: string | null;
  /** Reasoning-effort override for harnesses that support it. */
  effort?: string | null;
  /** Extra argv appended verbatim to the harness CLI by the PTY modes — the
   * launcher's escape hatch for harnesses without typed options. */
  harness_args?: string[];
  /** rmux only: spawn the session detached from the daemon so it survives a
   * daemon restart/rebuild. Default ON for the manager. */
  system_wide?: boolean;
}): Promise<ManagerLaunchResponse> {
  return post<ManagerLaunchResponse>('/manager/launch', body);
}

export function postTx(body: Record<string, unknown>): Promise<unknown> {
  return post('/tx', body);
}

export function fetchOrgFile(
  path: string,
  project?: string | null,
): Promise<OrgFileResponse> {
  return get<OrgFileResponse>(`/org/file${q(project, { path })}`);
}

export function fetchOrgNode(
  id: string,
  project?: string | null,
  kind?: string,
): Promise<OrgNodeDoc> {
  return get<OrgNodeDoc>(`/org/node${q(project, { id, kind })}`);
}

export function postOrgNodeEdit(
  id: string,
  body: { baseVersion: string; ops: NodeEditOp[] },
  project?: string | null,
  kind?: string,
): Promise<OrgNodeDoc> {
  return post<OrgNodeDoc>(`/org/node/${encodeURIComponent(id)}/edit`, {
    project,
    kind,
    base_version: body.baseVersion,
    ops: body.ops,
    request_id: requestId(`org-node-${id}`),
  });
}

export function postOrgFile(
  path: string,
  contents: string,
  project?: string | null,
): Promise<OrgFileResponse> {
  return post<OrgFileResponse>('/org/file', {
    project,
    path,
    contents,
    request_id: requestId(`org-${path.replace(/[^a-z0-9]+/gi, '-')}`),
  });
}

export function postRunRecover(
  id: string,
  body: RunRecoverRequest = {},
): Promise<RunRecoverResponse> {
  return post<RunRecoverResponse>(`/runs/${encodeURIComponent(id)}/recover`, {
    ...body,
    request_id: body.request_id ?? requestId(`run-recover-${id}`),
  });
}

export function postStage(stage: 'grill' | 'architect' | 'plan', project?: string | null): Promise<unknown> {
  return post(`/${stage}`, {
    project,
    request_id: requestId(stage),
  });
}

export function fetchArtifacts(project?: string | null): Promise<ArtifactSummary[]> {
  return get<ArtifactSummary[]>(`/artifacts${q(project)}`);
}

export function fetchArtifact(
  id: string,
  project?: string | null,
  version?: number,
  includeConsumed?: boolean,
): Promise<ArtifactDetail> {
  return get<ArtifactDetail>(
    `/artifacts/${encodeURIComponent(id)}${q(project, {
      version,
      include_consumed: includeConsumed ? 'true' : undefined,
    })}`,
  );
}

export function generateArtifact(
  body: ArtifactGenerateRequest,
  project?: string | null,
): Promise<ArtifactGenerateResponse> {
  return post<ArtifactGenerateResponse>(`/artifacts/generate${q(project)}`, body);
}

export function regenerateArtifact(
  id: string,
  body: ArtifactRegenerateRequest = {},
  project?: string | null,
): Promise<ArtifactGenerateResponse> {
  return post<ArtifactGenerateResponse>(`/artifacts/${encodeURIComponent(id)}/regenerate${q(project)}`, body);
}

// Post a member/admin comment on an artifact. The author is resolved from the
// session identity server-side — never sent from the client.
export function postArtifactComment(
  id: string,
  body: ArtifactCommentRequest,
  project?: string | null,
): Promise<unknown> {
  return post(`/artifacts/${encodeURIComponent(id)}/comments${q(project)}`, body);
}

// Toggle the people-facing "resolved" axis on a comment thread.
export function resolveArtifactComment(
  id: string,
  cid: string,
  resolved: boolean,
  project?: string | null,
): Promise<ArtifactCommentResolveResponse> {
  return post<ArtifactCommentResolveResponse>(
    `/artifacts/${encodeURIComponent(id)}/comments/${encodeURIComponent(cid)}/resolve${q(project)}`,
    { resolved },
  );
}

// GET /me — the identity + per-project capability snapshot backing useMe.
export function fetchMe(): Promise<Me> {
  return get<Me>('/me');
}
