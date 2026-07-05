import { useEffect, useState, type FormEvent } from 'react';
import { Loader2 } from 'lucide-react';

import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { useMe } from '@/hooks/useMe';
import type { BackendProfile, ConnectionTest } from '@/lib/backend';
import { HttpError } from '@/lib/transport';

type ConnectGateProps = {
  open: boolean;
  activeProfile: BackendProfile;
  updateProfile: (id: string, patch: Partial<Omit<BackendProfile, 'id' | 'createdAt'>>) => void;
  testConnection: (profile?: BackendProfile) => Promise<ConnectionTest>;
  onConnected: () => void;
};

export function ConnectGate({
  open,
  activeProfile,
  updateProfile,
  testConnection,
  onConnected,
}: ConnectGateProps) {
  const { login } = useMe();
  const [tab, setTab] = useState<'admin' | 'member'>('admin');

  // Admin bearer flow (unchanged).
  const [token, setToken] = useState('');
  const [validating, setValidating] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Member session flow.
  const [memberToken, setMemberToken] = useState('');
  const [memberValidating, setMemberValidating] = useState(false);
  const [memberError, setMemberError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setToken(activeProfile.token ?? '');
    setValidating(false);
    setError(null);
    setMemberToken('');
    setMemberValidating(false);
    setMemberError(null);
  }, [open, activeProfile.token]);

  async function handleConnect(event?: FormEvent) {
    event?.preventDefault();
    const trimmed = token.trim();
    if (!trimmed || validating) return;

    setValidating(true);
    setError(null);

    try {
      const result = await testConnection({ ...activeProfile, token: trimmed });
      if (result.ok) {
        updateProfile(activeProfile.id, { token: trimmed });
        onConnected();
      } else {
        setError(result.error ?? 'Connection failed');
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Connection failed');
    } finally {
      setValidating(false);
    }
  }

  async function handleMemberConnect(event?: FormEvent) {
    event?.preventDefault();
    const trimmed = memberToken.trim();
    if (!trimmed || memberValidating) return;

    setMemberValidating(true);
    setMemberError(null);

    try {
      await login(trimmed);
      onConnected();
    } catch (err) {
      if (err instanceof HttpError && err.status === 401) {
        setMemberError('Invalid or expired member token.');
      } else {
        setMemberError(err instanceof Error ? err.message : 'Login failed');
      }
    } finally {
      setMemberValidating(false);
    }
  }

  return (
    <Dialog open={open}>
      <DialogContent
        showCloseButton={false}
        onPointerDownOutside={(event) => event.preventDefault()}
        onEscapeKeyDown={(event) => event.preventDefault()}
        className="sm:max-w-md"
      >
        <DialogHeader>
          <DialogTitle>Connect to daemon</DialogTitle>
          <DialogDescription>
            Authenticate with the orgasmic daemon on this origin.
          </DialogDescription>
        </DialogHeader>

        <Tabs value={tab} onValueChange={(value) => setTab(value as 'admin' | 'member')}>
          <TabsList className="w-full">
            <TabsTrigger value="admin" className="flex-1">
              Admin token
            </TabsTrigger>
            <TabsTrigger value="member" className="flex-1">
              Member token
            </TabsTrigger>
          </TabsList>

          <TabsContent value="admin">
            <form className="flex flex-col gap-4" onSubmit={handleConnect}>
              <div className="flex flex-col gap-2">
                <Input
                  type="password"
                  aria-label="Bearer token"
                  placeholder="Bearer token"
                  value={token}
                  onChange={(event) => setToken(event.target.value)}
                  autoFocus
                  disabled={validating}
                />
                <p className="text-xs text-muted-foreground">
                  Find it in <code className="font-mono">$ORGASMIC_HOME/user/auth/token</code>
                </p>
                {error ? (
                  <p className="text-sm text-destructive" role="alert" aria-live="polite">
                    {error}
                  </p>
                ) : null}
              </div>
              <DialogFooter className="mx-0 mb-0 mt-2 rounded-md">
                <Button type="submit" disabled={!token.trim() || validating}>
                  {validating ? <Loader2 className="animate-spin" /> : null}
                  {validating ? 'Connecting…' : 'Connect'}
                </Button>
              </DialogFooter>
            </form>
          </TabsContent>

          <TabsContent value="member">
            <form className="flex flex-col gap-4" onSubmit={handleMemberConnect}>
              <div className="flex flex-col gap-2">
                <Input
                  type="password"
                  aria-label="Member token"
                  placeholder="Member token"
                  value={memberToken}
                  onChange={(event) => setMemberToken(event.target.value)}
                  disabled={memberValidating}
                />
                <p className="text-xs text-muted-foreground">
                  Paste the token you were given (from <code className="font-mono">orgasmic member add</code>).
                </p>
                {memberError ? (
                  <p className="text-sm text-destructive" role="alert" aria-live="polite">
                    {memberError}
                  </p>
                ) : null}
              </div>
              <DialogFooter className="mx-0 mb-0 mt-2 rounded-md">
                <Button type="submit" disabled={!memberToken.trim() || memberValidating}>
                  {memberValidating ? <Loader2 className="animate-spin" /> : null}
                  {memberValidating ? 'Signing in…' : 'Sign in'}
                </Button>
              </DialogFooter>
            </form>
          </TabsContent>
        </Tabs>
      </DialogContent>
    </Dialog>
  );
}
