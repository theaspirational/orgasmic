import { createContext, useContext } from 'react';
import { fetchNodeTypes, type NodeTypeDescriptor } from '@/lib/api';
import { useBackendProfiles } from '@/lib/backend';
import { useResource, type UseResourceResult } from '@/lib/useResource';

export const NodeTypesContext = createContext<UseResourceResult<NodeTypeDescriptor[]> | null>(null);

export function useNodeTypesResource(projectId: string | null, enabled: boolean) {
  const { activeProfile } = useBackendProfiles();
  const scope = `${activeProfile.id}:${activeProfile.baseUrl}:${projectId}:${enabled}`;
  const resource = useResource(scope, async () => ({ scope, types: await fetchNodeTypes(projectId!) }), {
    enabled: enabled && Boolean(projectId),
  });
  // useResource retains its last result during a key change. Never expose a
  // previous project's/backend's registry while the new request is pending.
  return { ...resource, data: resource.data?.scope === scope ? resource.data.types : null };
}

export function useNodeTypes() {
  const registry = useContext(NodeTypesContext);
  if (!registry) throw new Error('Node types require the app shell');
  return registry;
}
