// @arch arch_MK2Q2.2
// Member-session identity + capability context. The default (no member session)
// is the ADMIN bearer flow: identity 'admin', can() always true, nothing
// fetched — the existing app behaves exactly as before. A member enters via
// POST /login (see ConnectGate); we then fetch GET /me and gate the UI on the
// returned per-project capabilities.
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from 'react';

import { fetchMe } from '@/lib/api';
import { meCan, meVisibleProjects } from '@/lib/capabilities';
import {
  HttpError,
  memberLogin,
  setAuthMode,
  type MemberLoginResult,
} from '@/lib/transport';
import type { Me, MeIdentity, MeProject, MemberCapability } from '@/lib/types';

const MEMBER_FLAG_KEY = 'orgasmic.member.session';
const MEMBER_ME_KEY = 'orgasmic.member.me';

type MeContextValue = {
  me: Me | null;
  identity: MeIdentity;
  isMember: boolean;
  loading: boolean;
  /** Admin ⇒ always true. Member ⇒ capability granted for that project. */
  can: (projectId: string | null | undefined, action: MemberCapability) => boolean;
  /** Member's granted projects, or null for admin (= all projects). */
  visibleProjects: MeProject[] | null;
  login: (token: string) => Promise<MemberLoginResult>;
  logout: () => void;
  refresh: () => Promise<void>;
  /** Called on a daemon 401: drops a dead member session back to login. */
  onUnauthorized: () => boolean;
};

const MeContext = createContext<MeContextValue | null>(null);

function readMemberFlag(): boolean {
  if (typeof window === 'undefined') return false;
  return window.localStorage.getItem(MEMBER_FLAG_KEY) === '1';
}

function readCachedMe(): Me | null {
  if (typeof window === 'undefined') return null;
  try {
    const raw = window.localStorage.getItem(MEMBER_ME_KEY);
    return raw ? (JSON.parse(raw) as Me) : null;
  } catch {
    return null;
  }
}

function persistMemberSession(me: Me): void {
  if (typeof window === 'undefined') return;
  window.localStorage.setItem(MEMBER_FLAG_KEY, '1');
  window.localStorage.setItem(MEMBER_ME_KEY, JSON.stringify(me));
}

function clearMemberSession(): void {
  if (typeof window === 'undefined') return;
  window.localStorage.removeItem(MEMBER_FLAG_KEY);
  window.localStorage.removeItem(MEMBER_ME_KEY);
}

export function MeProvider({ children }: { children: ReactNode }) {
  // Hydrate optimistically from cache so a reloaded member session gates the nav
  // immediately, before /me re-validates.
  const hadSession = readMemberFlag();
  const [isMember, setIsMember] = useState<boolean>(hadSession);
  const [me, setMe] = useState<Me | null>(hadSession ? readCachedMe() : null);
  const [loading, setLoading] = useState<boolean>(hadSession);
  const isMemberRef = useRef(isMember);
  isMemberRef.current = isMember;

  // Match the transport auth mode to the (possibly cached) session synchronously
  // so the first requests of this render already carry the right credentials.
  if (hadSession) setAuthMode('member');

  const logout = useCallback(() => {
    setAuthMode('bearer');
    clearMemberSession();
    isMemberRef.current = false;
    setIsMember(false);
    setMe(null);
    setLoading(false);
  }, []);

  const refresh = useCallback(async () => {
    if (!isMemberRef.current) return;
    setLoading(true);
    try {
      const next = await fetchMe();
      setMe(next);
      persistMemberSession(next);
    } catch (err) {
      // A dead cookie (401) ends the member session; other errors keep the
      // cached snapshot so a transient blip doesn't blank the UI.
      if (err instanceof HttpError && err.status === 401) logout();
    } finally {
      setLoading(false);
    }
  }, [logout]);

  const login = useCallback(async (token: string) => {
    const result = await memberLogin(token);
    setAuthMode('member');
    isMemberRef.current = true;
    try {
      const next = await fetchMe();
      setMe(next);
      persistMemberSession(next);
      // AppShell treats isMember as the signal that protected routes may mount.
      // Publish it only after /me confirms and the session snapshot is persisted.
      setIsMember(true);
    } catch (err) {
      // Login succeeded but the follow-up /me failed — unwind so we don't strand
      // the app in a half-authenticated state.
      logout();
      throw err;
    }
    return result;
  }, [logout]);

  const onUnauthorized = useCallback(() => {
    if (!isMemberRef.current) return false;
    logout();
    return true;
  }, [logout]);

  // On mount, re-validate a hydrated member session against the daemon.
  useEffect(() => {
    if (hadSession) void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const can = useCallback(
    (projectId: string | null | undefined, action: MemberCapability) =>
      meCan(isMember ? me : null, projectId, action),
    [isMember, me],
  );

  const value = useMemo<MeContextValue>(
    () => ({
      me: isMember ? me : null,
      identity: isMember ? 'member' : 'admin',
      isMember,
      loading,
      can,
      visibleProjects: meVisibleProjects(isMember ? me : null),
      login,
      logout,
      refresh,
      onUnauthorized,
    }),
    [isMember, me, loading, can, login, logout, refresh, onUnauthorized],
  );

  return <MeContext.Provider value={value}>{children}</MeContext.Provider>;
}

// Default admin value for components rendered outside a provider (isolated tests
// / storybook). Admin ⇒ full access, so this preserves the pre-member behavior.
const ADMIN_FALLBACK: MeContextValue = {
  me: null,
  identity: 'admin',
  isMember: false,
  loading: false,
  can: () => true,
  visibleProjects: null,
  login: async () => {
    throw new Error('MeProvider is missing');
  },
  logout: () => undefined,
  refresh: async () => undefined,
  onUnauthorized: () => false,
};

export function useMe(): MeContextValue {
  return useContext(MeContext) ?? ADMIN_FALLBACK;
}
