import { Tabs as TabsRoot, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import type { MdxNode } from '../types';
import { asString } from './propUtils';
import { UnrenderableBlock } from './shared';
import { renderNodes } from './index';

/** Multiple states/comparisons/a file rail. Each tab is a nested `<Tab
 * label="...">` wrapper (recognized only here); its children go back through
 * the full block dispatch — a vertical `tabs` of `code` children is the
 * standard file-rail pattern. */
export function Tabs({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const tabs = node.children.filter(
    (child): child is Extract<MdxNode, { kind: 'element' }> => child.kind === 'element' && child.name === 'Tab',
  );
  if (tabs.length === 0) {
    return <UnrenderableBlock name="Tabs" message="no <Tab> children found" />;
  }
  // Radix Tabs activates by `value` — key it off the index, not the
  // human-readable label, so two tabs sharing a label (duplicate filenames,
  // repeated "Before"/"After" panes) don't fight over the same active state.
  return (
    <TabsRoot defaultValue="tab-0">
      <TabsList>
        {tabs.map((tab, index) => (
          <TabsTrigger key={index} value={`tab-${index}`}>
            {asString(tab.props.label, `Tab ${index + 1}`)}
          </TabsTrigger>
        ))}
      </TabsList>
      {tabs.map((tab, index) => (
        <TabsContent key={index} value={`tab-${index}`}>
          {renderNodes(tab.children, `tab-${index}`)}
        </TabsContent>
      ))}
    </TabsRoot>
  );
}
