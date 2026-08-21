import { useEffect, useMemo, useState } from 'react';
import { MessageCircle } from 'lucide-react';

import { fetchManagerChatCatalog } from '@/lib/api';
import { useResource } from '@/lib/useResource';

import { ChatControls } from './ChatControls';
import {
  availableChatProviders,
  setupCatalogMessage,
  setupModels,
  type ChatSelection,
} from './chatProviders';
import { ManagerComposer } from './ManagerComposer';
import { ReadOnlySessionBar } from './ReadOnlySessionBar';

export function ChatSetup({
  projectId,
  readOnly,
  onStart,
}: {
  projectId: string | null;
  readOnly: boolean;
  onStart: (selection: ChatSelection, message: string) => Promise<boolean>;
}) {
  const catalog = useResource(
    `rundock-chat-catalog:${projectId ?? 'none'}`,
    () => fetchManagerChatCatalog(projectId ?? ''),
    { enabled: Boolean(projectId) },
  );
  const providers = useMemo(
    () => (catalog.loading ? [] : availableChatProviders(catalog.data)),
    [catalog.data, catalog.loading],
  );
  const [selection, setSelection] = useState<ChatSelection>({
    provider: 'codex',
    model: '',
    effort: '',
    serviceTier: '',
    access: 'full-access',
  });

  useEffect(() => {
    if (providers.length === 0 || providers.includes(selection.provider)) return;
    const provider = providers[0];
    if (provider) {
      setSelection((current) => ({ ...current, provider, model: '', effort: '' }));
    }
  }, [providers, selection.provider]);

  const ready = Boolean(projectId && providers.includes(selection.provider));
  const unavailableCopy = !projectId
    ? 'Select a project to start chatting.'
    : catalog.loading
      ? 'Checking Chat providers…'
      : catalog.error
        ? 'Chat catalog is unavailable. Update or restart the Orgasmic daemon, then try again.'
      : providers.length === 0
        ? 'Install or sign in to Codex, Claude, or OpenCode to start a chat.'
        : 'The selected provider is unavailable.';

  return (
    <div className="flex h-full min-h-0 flex-col bg-muted/20">
      <div className="flex min-h-0 flex-1 justify-center overflow-y-auto px-6 py-10 text-center [align-items:safe_center]">
        <div className="max-w-md">
          <span className="mx-auto mb-4 flex size-11 items-center justify-center rounded-xl bg-muted text-foreground">
            <MessageCircle className="size-5" />
          </span>
          <h2 className="text-xl font-semibold tracking-tight">Start a project chat</h2>
          <p className="mt-2 text-sm leading-relaxed text-muted-foreground">
            Talk directly to Codex, Claude, or OpenCode in the current checkout. The conversation
            stays in RunDock while the provider works.
          </p>
        </div>
      </div>
      <div className="shrink-0 px-3 pb-3 pt-2 sm:px-4 sm:pb-4">
        <div className="mx-auto w-full max-w-3xl">
          {readOnly ? (
            <ReadOnlySessionBar />
          ) : (
            <ManagerComposer
              runId={ready ? `chat:${projectId}` : null}
              connectionState="open"
              placeholder="Ask about this project"
              readyLabel="Enter to start chat · Shift+Enter for a new line"
              unavailableLabel={unavailableCopy}
              onSend={(message) => onStart(selection, message)}
              controls={
                <ChatControls
                  value={selection}
                  onChange={setSelection}
                  models={setupModels(catalog.data, selection.provider)}
                  availableProviders={providers}
                  disabled={!ready}
                  catalogLoading={catalog.loading}
                  catalogMessage={setupCatalogMessage(catalog.data, selection.provider)}
                  showAccess
                />
              }
            />
          )}
        </div>
      </div>
    </div>
  );
}
