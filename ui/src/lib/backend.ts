// @arch arch_MK2Q2.2
import {
  requestWithProfile,
  setActiveProfileForTransport,
  type TransportProfile,
} from './transport';
import { getJson, getString, setJson, setString } from './storage';
import {
  createContext,
  createElement,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from 'react';
import { invoke } from '@tauri-apps/api/core';

export type BackendProfile = {
  id: string;
  name: string;
  baseUrl: string;
  token?: string | null;
  createdAt: string;
  lastConnectedAt?: string | null;
};

export type ConnectionTest = {
  ok: boolean;
  latencyMs: number;
  error?: string;
};

type BackendContextValue = {
  profiles: BackendProfile[];
  activeProfile: BackendProfile;
  activeProfileId: string;
  setActiveProfile: (id: string) => void;
  addProfile: (profile: Omit<BackendProfile, 'id' | 'createdAt' | 'lastConnectedAt'>) => BackendProfile;
  updateProfile: (id: string, patch: Partial<Omit<BackendProfile, 'id' | 'createdAt'>>) => void;
  removeProfile: (id: string) => void;
  testConnection: (profile?: BackendProfile) => Promise<ConnectionTest>;
};

type TauriLocalProfile = {
  id: string;
  name: string;
  baseUrl: string;
  token?: string | null;
  home: string;
};

const PROFILES_KEY = 'backend:profiles';
const ACTIVE_KEY = 'backend:active-profile';
const LOCAL_ID = 'local';
const ANDROID_EMULATOR_DAEMON_URL = 'http://10.0.2.2:4848';

const BackendContext = createContext<BackendContextValue | null>(null);

function sameOrigin(): string {
  const httpOrigin = currentHttpOrigin();
  if (httpOrigin) return httpOrigin;
  if (isAndroidTauriRuntime()) return ANDROID_EMULATOR_DAEMON_URL;
  if (isTauriRuntime()) return 'http://127.0.0.1:4848';
  return window.location.origin;
}

function isTauriRuntime(): boolean {
  return Boolean((window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__);
}

function currentHttpOrigin(): string | null {
  return window.location.protocol === 'http:' || window.location.protocol === 'https:'
    ? window.location.origin
    : null;
}

function isAndroidTauriRuntime(): boolean {
  return isTauriRuntime() && /Android/i.test(window.navigator.userAgent);
}

function nowIso(): string {
  return new Date().toISOString();
}

function normalizeBaseUrl(baseUrl: string): string {
  const trimmed = baseUrl.trim();
  if (!trimmed) return sameOrigin();
  return trimmed.replace(/\/+$/, '');
}

function makeId(name: string): string {
  const slug =
    name
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, '-')
      .replace(/^-+|-+$/g, '')
      .slice(0, 32) || 'backend';
  return `${slug}-${Date.now().toString(36)}`;
}

function localProfile(): BackendProfile {
  return {
    id: LOCAL_ID,
    name: isAndroidTauriRuntime() ? 'Android emulator host' : 'Local daemon',
    baseUrl: sameOrigin(),
    token: null,
    createdAt: 'builtin',
    lastConnectedAt: null,
  };
}

function sanitizeProfile(profile: BackendProfile): BackendProfile {
  return {
    ...profile,
    name: profile.name.trim() || 'Unnamed',
    baseUrl: normalizeBaseUrl(profile.baseUrl),
    token: profile.token?.trim() || null,
  };
}

function loadProfiles(): BackendProfile[] {
  const saved = getJson<BackendProfile[]>(PROFILES_KEY, []);
  const local = localProfile();
  const byId = new Map<string, BackendProfile>();
  byId.set(local.id, local);
  for (const profile of saved) {
    const sanitized = sanitizeProfile(profile);
    byId.set(
      sanitized.id,
      sanitized.id === LOCAL_ID
        ? { ...sanitized, baseUrl: local.baseUrl, createdAt: sanitized.createdAt || local.createdAt }
        : sanitized,
    );
  }
  return Array.from(byId.values());
}

function toTransportProfile(profile: BackendProfile): TransportProfile {
  return { baseUrl: profile.baseUrl, token: profile.token ?? null };
}

export function BackendProfileProvider({ children }: { children: ReactNode }) {
  const [profiles, setProfiles] = useState<BackendProfile[]>(() => loadProfiles());
  const [activeProfileId, setActiveProfileId] = useState<string>(() => getString(ACTIVE_KEY) ?? LOCAL_ID);

  const activeProfile = useMemo(() => {
    return profiles.find((profile) => profile.id === activeProfileId) ?? profiles[0] ?? localProfile();
  }, [activeProfileId, profiles]);

  setActiveProfileForTransport(toTransportProfile(activeProfile));

  useEffect(() => {
    setString(ACTIVE_KEY, activeProfile.id);
  }, [activeProfile.id]);

  useEffect(() => {
    setJson(PROFILES_KEY, profiles);
  }, [profiles]);

  useEffect(() => {
    if (!isTauriRuntime()) return;
    let cancelled = false;
    void invoke<TauriLocalProfile>('local_backend_profile')
      .then((profile) => {
        if (cancelled) return;
        const local = sanitizeProfile({
          id: LOCAL_ID,
          name: profile.name || 'Local daemon',
          baseUrl: currentHttpOrigin() ?? profile.baseUrl ?? 'http://127.0.0.1:4848',
          token: profile.token ?? null,
          createdAt: 'builtin',
          lastConnectedAt: null,
        });
        setProfiles((current) => {
          const seen = new Set<string>();
          const next = current.map((candidate) => {
            if (candidate.id === LOCAL_ID) {
              seen.add(LOCAL_ID);
              return { ...candidate, ...local, lastConnectedAt: candidate.lastConnectedAt };
            }
            return candidate;
          });
          if (!seen.has(LOCAL_ID)) next.unshift(local);
          return next;
        });
      })
      .catch((err) => {
        console.warn('Unable to load Tauri local backend profile', err);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const setActiveProfile = useCallback((id: string) => {
    setActiveProfileId(id);
  }, []);

  const addProfile = useCallback(
    (input: Omit<BackendProfile, 'id' | 'createdAt' | 'lastConnectedAt'>) => {
      const profile = sanitizeProfile({
        ...input,
        id: makeId(input.name),
        createdAt: nowIso(),
        lastConnectedAt: null,
      });
      setProfiles((current) => [...current, profile]);
      setActiveProfileId(profile.id);
      return profile;
    },
    [],
  );

  const updateProfile = useCallback(
    (id: string, patch: Partial<Omit<BackendProfile, 'id' | 'createdAt'>>) => {
      setProfiles((current) =>
        current.map((profile) =>
          profile.id === id
            ? sanitizeProfile({
                ...profile,
                ...patch,
                baseUrl:
                  id === LOCAL_ID ? (patch.baseUrl ?? profile.baseUrl) : (patch.baseUrl ?? profile.baseUrl),
              })
            : profile,
        ),
      );
    },
    [],
  );

  const removeProfile = useCallback((id: string) => {
    if (id === LOCAL_ID) return;
    setProfiles((current) => current.filter((profile) => profile.id !== id));
    setActiveProfileId((current) => (current === id ? LOCAL_ID : current));
  }, []);

  const testConnection = useCallback(
    async (profile: BackendProfile = activeProfile): Promise<ConnectionTest> => {
      const started = performance.now();
      try {
        await requestWithProfile(toTransportProfile(profile), '/api/healthz');
        await requestWithProfile(toTransportProfile(profile), '/api/daemon/status');
        const latencyMs = Math.max(0, Math.round(performance.now() - started));
        setProfiles((current) =>
          current.map((candidate) =>
            candidate.id === profile.id ? { ...candidate, lastConnectedAt: nowIso() } : candidate,
          ),
        );
        return { ok: true, latencyMs };
      } catch (err) {
        const latencyMs = Math.max(0, Math.round(performance.now() - started));
        return {
          ok: false,
          latencyMs,
          error: err instanceof Error ? err.message : String(err),
        };
      }
    },
    [activeProfile],
  );

  const value = useMemo<BackendContextValue>(
    () => ({
      profiles,
      activeProfile,
      activeProfileId: activeProfile.id,
      setActiveProfile,
      addProfile,
      updateProfile,
      removeProfile,
      testConnection,
    }),
    [profiles, activeProfile, setActiveProfile, addProfile, updateProfile, removeProfile, testConnection],
  );

  return createElement(BackendContext.Provider, { value }, children);
}

function useBackendContext(): BackendContextValue {
  const ctx = useContext(BackendContext);
  if (!ctx) throw new Error('BackendProfileProvider is missing');
  return ctx;
}

export function useActiveProfile(): BackendProfile {
  return useBackendContext().activeProfile;
}

export function useBackendProfiles(): BackendContextValue {
  return useBackendContext();
}
