import { expect, it } from 'vitest';
import { openTab, parseView, projectTabTarget } from '../tabsStore';

it('remembers generic collections and passes their names as route params', () => {
  expect(parseView('nodes/meetings')).toBe('nodes/meetings');
  expect(parseView('nodes/')).toBeNull();
  expect(parseView('nodes/../tasks')).toBeNull();
  expect(parseView('nodes/..')).toBeNull();
  openTab('plugin-ui-qa', 'nodes/meetings');
  expect(openTab('plugin-ui-qa')).toBe('nodes/meetings');
  expect(projectTabTarget('plugin-ui-qa', 'nodes/team meetings')).toEqual({
    to: '/projects/$projectId/nodes/$collection', params: { projectId: 'plugin-ui-qa', collection: 'team meetings' },
  });
});
