// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { GenericNodeDialog, GenericNodeView } from '../GenericNodeView';
import { NodeTypesContext } from '@/lib/nodeTypes';
import type { NodeTypeDescriptor } from '@/lib/api';
import type { OrgNodeDoc } from '@/lib/orgdoc/types';

const mocks = vi.hoisted(() => ({ graph: vi.fn(), node: vi.fn(), edit: vi.fn(), navigate: vi.fn(), write: true, read: true }));
vi.mock('@/lib/api', async () => ({
  ...await vi.importActual<typeof import('@/lib/api')>('@/lib/api'),
  fetchGraphNodes: mocks.graph, fetchOrgNode: mocks.node, postOrgNodeEdit: mocks.edit,
}));
vi.mock('@/hooks/useMe', () => ({ useMe: () => ({ isMember: true, can: (_project: string, action: string) => action === 'nodes.write' ? mocks.write : mocks.read }) }));
vi.mock('@tanstack/react-router', async () => ({
  ...await vi.importActual<typeof import('@tanstack/react-router')>('@tanstack/react-router'),
  useNavigate: () => mocks.navigate,
  useRouterState: ({ select }: { select: (state: unknown) => unknown }) => select({ location: { pathname: '/projects/demo/nodes/meetings' } }),
}));

const type: NodeTypeDescriptor = {
  collection: 'meetings', id_prefix: 'MEET-', label: 'Meeting', label_plural: 'Meetings',
  required_properties: ['ID', 'LOCATION'], states: ['active', 'archived'],
  transitions: { active: ['archived'], archived: ['active'] }, regenerate_prompt: null, chat_prompt: null,
};
const doc: OrgNodeDoc = {
  id: 'MEET-1', kind: 'meetings', title: 'Planning session', todo: 'ACTIVE', tags: [],
  body: 'Meeting notes', properties: [{ key: 'ID', value: 'MEET-1' }, { key: 'LOCATION', value: 'Office' }, { key: 'HOST', value: 'Sam' }],
  sections: [{ title: 'Agenda', body: 'Review changes' }], source: { file: 'meetings/MEET-1/node.org', base_version: 'v1' },
};
const callbacks = { historyDepth: 1, onBack: vi.fn(), onClose: vi.fn(), onOpenNode: vi.fn() };
const registry = (data: NodeTypeDescriptor[] | null) => ({ data, error: null, loading: !data, refresh: vi.fn() });

beforeEach(() => {
  vi.clearAllMocks(); mocks.write = true; mocks.read = true;
  mocks.node.mockResolvedValue(doc);
  mocks.graph.mockResolvedValue([{ id: doc.id, title: doc.title, layer: 'meetings', todo: doc.todo, outgoing: [], source_file: doc.source.file }]);
  mocks.edit.mockImplementation(async (_id, body) => ({ ...doc, title: body.ops.find((op: { op: string }) => op.op === 'set_title')?.title ?? doc.title, source: { ...doc.source, base_version: 'v2' } }));
});
afterEach(cleanup);

it('loads a descriptor and list without changing hook order, then opens the node', async () => {
  const view = render(<NodeTypesContext.Provider value={registry(null)}><GenericNodeView projectId="demo" collection="meetings" /></NodeTypesContext.Provider>);
  expect(screen.getByText('Loading node types…')).toBeInTheDocument();
  view.rerender(<NodeTypesContext.Provider value={registry([type])}><GenericNodeView projectId="demo" collection="meetings" /></NodeTypesContext.Provider>);
  expect(await screen.findByText('Planning session')).toBeInTheDocument();
  expect(mocks.graph).toHaveBeenCalledWith('demo', 'meetings');
  fireEvent.change(screen.getByRole('textbox', { name: 'Search Meetings' }), { target: { value: 'missing' } });
  expect(screen.getByText('No matching nodes.')).toBeInTheDocument();
  fireEvent.change(screen.getByRole('textbox', { name: 'Search Meetings' }), { target: { value: '' } });
  fireEvent.click(screen.getByText('Planning session'));
  expect(mocks.navigate).toHaveBeenCalledOnce();
});

it('edits a custom title, body, required property and discovered property through the existing route', async () => {
  render(<GenericNodeDialog projectId="demo" type={type} initialDocument={doc} {...callbacks} />);
  await waitFor(() => expect(screen.getByRole('button', { name: 'Edit' })).toBeEnabled());
  fireEvent.click(screen.getByRole('button', { name: 'Edit' }));
  fireEvent.change(screen.getByRole('textbox', { name: 'Title' }), { target: { value: 'Revised meeting' } });
  fireEvent.change(screen.getByRole('textbox', { name: 'Body' }), { target: { value: 'Revised notes' } });
  fireEvent.change(screen.getByRole('textbox', { name: 'LOCATION' }), { target: { value: 'Remote' } });
  fireEvent.change(screen.getByRole('textbox', { name: 'HOST' }), { target: { value: 'Lee' } });
  expect(screen.queryByRole('textbox', { name: 'ID' })).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole('button', { name: 'Save' }));
  await waitFor(() => expect(mocks.edit).toHaveBeenCalledWith('MEET-1', {
    baseVersion: 'v1', ops: expect.arrayContaining([
      { op: 'set_title', title: 'Revised meeting' }, { op: 'set_body', body: 'Revised notes' },
      { op: 'set_property', key: 'LOCATION', value: 'Remote' }, { op: 'set_property', key: 'HOST', value: 'Lee' },
    ]),
  }, 'demo', 'meetings'));
});

it('uses descriptor transitions and the current version for state changes', async () => {
  render(<GenericNodeDialog projectId="demo" type={type} initialDocument={doc} {...callbacks} />);
  const select = screen.getByRole('combobox', { name: 'State' });
  await waitFor(() => expect(select).toBeEnabled());
  await act(async () => fireEvent.change(select, { target: { value: 'archived' } }));
  expect(mocks.edit).toHaveBeenCalledWith('MEET-1', { baseVersion: 'v1', ops: [{ op: 'set_state', state: 'archived' }] }, 'demo', 'meetings');
});

it('keeps viewer and unrecognized-state documents read only', async () => {
  mocks.write = false;
  const view = render(<GenericNodeDialog projectId="demo" type={type} initialDocument={doc} {...callbacks} />);
  expect(await screen.findByText('Meeting notes')).toBeInTheDocument();
  expect(screen.queryByRole('button', { name: 'Edit' })).not.toBeInTheDocument();
  expect(screen.getByRole('combobox', { name: 'State' })).toBeDisabled();
  view.unmount();
  mocks.write = true;
  mocks.node.mockResolvedValue({ ...doc, todo: null });
  render(<GenericNodeDialog projectId="demo" type={type} initialDocument={{ ...doc, todo: null }} {...callbacks} />);
  expect(await screen.findByText('Meeting notes')).toBeInTheDocument();
  expect(screen.queryByRole('button', { name: 'Edit' })).not.toBeInTheDocument();
  expect(mocks.edit).not.toHaveBeenCalled();
});

it('does not fetch collection data without graph permission', async () => {
  mocks.read = false;
  render(<NodeTypesContext.Provider value={registry([type])}><GenericNodeView projectId="demo" collection="meetings" /></NodeTypesContext.Provider>);
  expect(screen.getByRole('alert')).toHaveTextContent('permission');
  expect(mocks.graph).not.toHaveBeenCalled();
});

it('keeps a node with an incompatible stored plugin schema read only', async () => {
  const incompatible = { ...doc, schema_matches: false };
  mocks.node.mockResolvedValue(incompatible);
  render(<GenericNodeDialog projectId="demo" type={type} initialDocument={incompatible} {...callbacks} />);
  expect(await screen.findByText('Meeting notes')).toBeInTheDocument();
  expect(screen.queryByRole('button', { name: 'Edit' })).not.toBeInTheDocument();
  expect(screen.getByRole('combobox', { name: 'State' })).toBeDisabled();
});
