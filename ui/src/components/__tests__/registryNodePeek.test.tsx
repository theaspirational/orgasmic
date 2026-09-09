// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import { RegistryNodePeek } from '../RegistryNodePeek';
import { NodeTypesContext } from '@/lib/nodeTypes';

const mocks = vi.hoisted(() => ({ node: vi.fn() }));
vi.mock('@/lib/api', () => ({ fetchOrgNode: mocks.node }));
vi.mock('@/components/TaskDialog', () => ({ TaskDialog: () => <div>Compiled task</div> }));
vi.mock('@/components/node-views/NodeModal', () => ({ NodeModal: () => <div>Compiled node</div> }));
vi.mock('@/components/GenericNodeView', () => ({ GenericNodeDialog: ({ type }: { type?: { collection: string } }) => <div>{type?.collection ?? 'Descriptor unavailable'}</div> }));
afterEach(() => { cleanup(); vi.clearAllMocks(); });

function peek(collection: string, prefix: string, id: string) {
  return render(<NodeTypesContext.Provider value={{ data: [{ collection, id_prefix: prefix, label: collection, label_plural: collection, states: [], transitions: {}, required_properties: [], regenerate_prompt: null, chat_prompt: null }], loading: false, error: null, refresh: vi.fn() }}>
    <RegistryNodePeek projectId="demo" nodeId={id} historyDepth={0} onBack={vi.fn()} onClose={vi.fn()} onOpenNode={vi.fn()} />
  </NodeTypesContext.Provider>);
}

it('mounts the compiled task without a preliminary node request using the registry prefix', async () => {
  mocks.node.mockReturnValue(new Promise(() => {}));
  peek('tasks', 'WORK-', 'WORK-1');
  expect(await screen.findByText('Compiled task')).toBeInTheDocument();
  expect(mocks.node).not.toHaveBeenCalled();
});

it('matches a fetched artifact by collection, not its singular API kind', async () => {
  mocks.node.mockResolvedValue({ id: 'OLD-1', kind: 'artifact', collection: 'artifacts' });
  peek('artifacts', 'art_', 'OLD-1');
  expect(await screen.findByText('artifacts')).toBeInTheDocument();
});
