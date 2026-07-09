// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('@tanstack/react-router', () => ({
  useNavigate: () => vi.fn(),
}));

const generateArtifactMock = vi.fn();
vi.mock('@/lib/api', () => ({
  generateArtifact: (...args: unknown[]) => generateArtifactMock(...args),
}));

import { GenerateArtifactDialog } from '../GenerateArtifactDialog';

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function renderDialog(props: Partial<Parameters<typeof GenerateArtifactDialog>[0]> = {}) {
  const defaults = {
    projectId: 'proj1',
    open: true,
    onOpenChange: vi.fn(),
    nodes: [] as string[],
  };
  const merged = { ...defaults, ...props };
  const view = render(<GenerateArtifactDialog {...merged} />);
  return { ...view, props: merged };
}

describe('GenerateArtifactDialog prompt suggestions', () => {
  it('offers suggestions while the prompt is empty and fills on click', async () => {
    renderDialog({ nodes: ['dec_1'], nodeLabels: ['Auth decision'] });
    const dialog = await screen.findByRole('dialog');
    const suggestion = within(dialog).getByRole('button', {
      name: /One-page brief on Auth decision/,
    });

    fireEvent.click(suggestion);

    const textarea = within(dialog).getByPlaceholderText('What should this artifact cover?');
    expect(textarea).toHaveValue('One-page brief on Auth decision: context, mechanism, open questions.');
    // Once the field has text the suggestion chips withdraw.
    expect(
      within(dialog).queryByRole('button', { name: /Review packet for Auth decision/ }),
    ).toBeNull();
  });

  it('shapes suggestions by subject count', async () => {
    renderDialog({ nodes: [] });
    const dialog = await screen.findByRole('dialog');
    expect(
      within(dialog).getByRole('button', { name: /Project overview for a new teammate/ }),
    ).toBeInTheDocument();
    cleanup();

    renderDialog({ nodes: ['a', 'b', 'c'] });
    const multi = await screen.findByRole('dialog');
    expect(
      within(multi).getByRole('button', { name: /Compare these 3 nodes/ }),
    ).toBeInTheDocument();
  });
});

describe('GenerateArtifactDialog draft preservation', () => {
  it('keeps typed text across close and reopen', async () => {
    const { rerender, props } = renderDialog();
    const dialog = await screen.findByRole('dialog');
    fireEvent.change(within(dialog).getByPlaceholderText('What should this artifact cover?'), {
      target: { value: 'Half-written prompt' },
    });

    rerender(<GenerateArtifactDialog {...props} open={false} />);
    rerender(<GenerateArtifactDialog {...props} open={true} />);

    const reopened = await screen.findByRole('dialog');
    expect(
      within(reopened).getByPlaceholderText('What should this artifact cover?'),
    ).toHaveValue('Half-written prompt');
  });

  it('clears the draft only after a successful submit', async () => {
    generateArtifactMock.mockResolvedValue({ artifact_id: 'ART-9', run_id: 'run-1' });
    const { rerender, props } = renderDialog();
    const dialog = await screen.findByRole('dialog');
    fireEvent.change(within(dialog).getByPlaceholderText('What should this artifact cover?'), {
      target: { value: 'Ship it' },
    });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Generate' }));

    await waitFor(() => expect(generateArtifactMock).toHaveBeenCalled());
    expect(generateArtifactMock).toHaveBeenCalledWith({ nodes: [], prompt: 'Ship it' }, 'proj1');

    rerender(<GenerateArtifactDialog {...props} open={false} />);
    rerender(<GenerateArtifactDialog {...props} open={true} />);
    const reopened = await screen.findByRole('dialog');
    expect(
      within(reopened).getByPlaceholderText('What should this artifact cover?'),
    ).toHaveValue('');
  });

  it('keeps the draft when the submit fails', async () => {
    generateArtifactMock.mockRejectedValue(new Error('daemon unreachable'));
    renderDialog();
    const dialog = await screen.findByRole('dialog');
    fireEvent.change(within(dialog).getByPlaceholderText('What should this artifact cover?'), {
      target: { value: 'Do not lose me' },
    });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Generate' }));

    await within(dialog).findByRole('alert');
    expect(
      within(dialog).getByPlaceholderText('What should this artifact cover?'),
    ).toHaveValue('Do not lose me');
  });
});
