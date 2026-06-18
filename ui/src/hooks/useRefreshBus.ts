// @arch arch_MK2Q2.3
import { createContext, createElement, useCallback, useContext, useMemo, useState, type ReactNode } from 'react';

type RefreshContextValue = {
  token: number;
  bump: () => void;
};

const RefreshContext = createContext<RefreshContextValue>({ token: 0, bump: () => {} });

export function RefreshProvider({ children }: { children: ReactNode }) {
  const [token, setToken] = useState(0);
  const bump = useCallback(() => setToken((value) => value + 1), []);
  const value = useMemo(() => ({ token, bump }), [token, bump]);
  return createElement(RefreshContext.Provider, { value }, children);
}

export function useRefreshToken(): number {
  return useContext(RefreshContext).token;
}

export function useRefreshBump(): () => void {
  return useContext(RefreshContext).bump;
}
