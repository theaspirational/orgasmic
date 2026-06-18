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
import type { BackendProfile, ConnectionTest } from '@/lib/backend';

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
  const [token, setToken] = useState('');
  const [validating, setValidating] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setToken(activeProfile.token ?? '');
    setValidating(false);
    setError(null);
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

  return (
    <Dialog open={open}>
      <DialogContent
        showCloseButton={false}
        onPointerDownOutside={(event) => event.preventDefault()}
        onEscapeKeyDown={(event) => event.preventDefault()}
        className="sm:max-w-md"
      >
        <form className="flex flex-col gap-4" onSubmit={handleConnect}>
          <DialogHeader>
            <DialogTitle>Connect to daemon</DialogTitle>
            <DialogDescription>
              Enter your bearer token to authenticate with the orgasmic daemon on this origin.
            </DialogDescription>
          </DialogHeader>

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
      </DialogContent>
    </Dialog>
  );
}
