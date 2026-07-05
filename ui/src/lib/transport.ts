// @arch arch_MK2Q2.2
// Single seam for all UI ⇄ daemon traffic.
// Rule: do not call fetch/WebSocket directly outside this module.

export class HttpError extends Error {
  status: number;
  body: string;
  detail?: string;

  constructor(status: number, body: string) {
    let message = `${status} ${body}`;
    let detail: string | undefined;
    try {
      const parsed = JSON.parse(body) as { error?: string; detail?: string; message?: string };
      if (parsed && typeof parsed === 'object') {
        detail = parsed.detail ?? parsed.message ?? parsed.error;
        if (detail) message = `${status} ${detail}`;
      }
    } catch {
      /* plain text */
    }
    super(message);
    this.status = status;
    this.body = body;
    this.detail = detail;
  }
}

type RequestInit = {
  method?: 'GET' | 'POST' | 'PUT' | 'DELETE' | 'PATCH';
  body?: unknown;
  contentType?: string;
};

type UnauthorizedHandler = (error: HttpError) => void;

const API_PREFIX = '/api';

let unauthorizedHandler: UnauthorizedHandler | null = null;

export function setUnauthorizedHandler(handler: UnauthorizedHandler | null): void {
  unauthorizedHandler = handler;
}

export type TransportProfile = {
  baseUrl: string;
  token?: string | null;
};

let activeProfile: TransportProfile = {
  baseUrl: window.location.origin,
  token: null,
};

// Auth mode. The default `bearer` mode is the admin flow: an `Authorization:
// Bearer <token>` header on HTTP and `?token=<token>` on the WS URL. `member`
// mode is the identity-scoped flow: a member has no admin token, so we drop the
// bearer/query token entirely and authenticate via the HttpOnly session cookie
// set by POST /login — sent by attaching `credentials: 'include'` to fetch and
// relying on the browser to carry the cookie on same-origin WS.
type AuthMode = 'bearer' | 'member';
let authMode: AuthMode = 'bearer';

export function setAuthMode(mode: AuthMode): void {
  authMode = mode;
}

export function getAuthMode(): AuthMode {
  return authMode;
}

export function setActiveProfileForTransport(profile: TransportProfile): void {
  activeProfile = {
    baseUrl: normalizeBaseUrl(profile.baseUrl),
    token: profile.token?.trim() || null,
  };
}

function normalizeBaseUrl(baseUrl: string): string {
  return (baseUrl || window.location.origin).replace(/\/+$/, '');
}

function apiPath(path: string): string {
  if (/^https?:\/\//i.test(path) || /^wss?:\/\//i.test(path)) return path;
  const normalized = path.startsWith('/') ? path : `/${path}`;
  if (normalized === API_PREFIX || normalized.startsWith(`${API_PREFIX}/`)) return normalized;
  return `${API_PREFIX}${normalized}`;
}

function resolveHttpUrl(path: string, profile: TransportProfile): string {
  if (/^https?:\/\//i.test(path)) return path;
  return new URL(apiPath(path), `${normalizeBaseUrl(profile.baseUrl)}/`).toString();
}

function resolveWsUrl(path: string, profile: TransportProfile): string {
  const base = normalizeBaseUrl(profile.baseUrl);
  const httpUrl = /^wss?:\/\//i.test(path)
    ? path
    : new URL(apiPath(path), `${base}/`).toString();
  const url = new URL(httpUrl);
  if (url.protocol === 'http:') url.protocol = 'ws:';
  if (url.protocol === 'https:') url.protocol = 'wss:';
  // Member mode carries auth via the session cookie the browser attaches to a
  // same-origin WS automatically — never put a token on the query (admin-only).
  if (authMode === 'bearer' && profile.token) url.searchParams.set('token', profile.token);
  return url.toString();
}

type BuiltRequest = {
  url: string;
  init: {
    method: RequestInit['method'];
    headers: Record<string, string>;
    body?: string;
    credentials?: RequestCredentials;
  };
};

function buildRequest(path: string, init: RequestInit, profile: TransportProfile): BuiltRequest {
  const method = init.method ?? 'GET';
  const headers: Record<string, string> = {};
  let body: string | undefined;
  if (init.body !== undefined && init.body !== null) {
    if (typeof init.body === 'string') {
      body = init.body;
      headers['content-type'] = init.contentType ?? 'text/plain';
    } else {
      body = JSON.stringify(init.body);
      headers['content-type'] = init.contentType ?? 'application/json';
    }
  } else if (method !== 'GET' && method !== 'DELETE') {
    body = '{}';
    headers['content-type'] = init.contentType ?? 'application/json';
  }
  // Member mode authenticates by cookie, so never attach the admin bearer.
  if (authMode === 'bearer' && profile.token) headers.authorization = `Bearer ${profile.token}`;
  return {
    url: resolveHttpUrl(path, profile),
    init: { method, headers, body, credentials: authMode === 'member' ? 'include' : 'same-origin' },
  };
}

export async function requestWithProfile<T>(
  profile: TransportProfile,
  path: string,
  init: RequestInit = {},
): Promise<T> {
  const req = buildRequest(path, init, profile);
  const res = await fetch(req.url, req.init);
  if (!res.ok) {
    const error = new HttpError(res.status, await res.text());
    if (res.status === 401) unauthorizedHandler?.(error);
    throw error;
  }
  if (res.status === 204) return undefined as unknown as T;
  return (await res.json()) as T;
}

async function browserRequest<T>(path: string, init: RequestInit = {}): Promise<T> {
  return requestWithProfile<T>(activeProfile, path, init);
}

function browserWebSocket(path: string, protocols?: string | string[]): WebSocket {
  return new WebSocket(resolveWsUrl(path, activeProfile), protocols);
}

export const transport = {
  request: browserRequest,
  openWebSocket: browserWebSocket,
};

export type MemberLoginResult = {
  name: string;
  expires_at: string;
};

// POST /login with the member token in the body. Always sent cookie-first
// (credentials:'include') and without a bearer, independent of the current
// auth mode, so the daemon's Set-Cookie is stored. On 200 the caller switches
// the transport into member mode via setAuthMode('member').
export async function memberLogin(token: string): Promise<MemberLoginResult> {
  const url = resolveHttpUrl('/login', activeProfile);
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ token }),
    credentials: 'include',
  });
  if (!res.ok) throw new HttpError(res.status, await res.text());
  return (await res.json()) as MemberLoginResult;
}

export function get<T>(path: string): Promise<T> {
  return transport.request<T>(path);
}

export function post<T>(path: string, body?: unknown): Promise<T> {
  return transport.request<T>(path, { method: 'POST', body });
}
