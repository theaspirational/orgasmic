import type { ComponentType, ReactNode } from 'react';

import type { MdxNode } from '../types';
import { BLOCK_TYPE_SET } from '../types';
import { AnnotatedCode } from './AnnotatedCode';
import { Callout } from './Callout';
import { Canvas } from './Canvas';
import { Checklist } from './Checklist';
import { Code } from './Code';
import { Columns } from './Columns';
import { DataModel } from './DataModel';
import { Diagram } from './Diagram';
import { EntityRelationship } from './EntityRelationship';
import { FileTree } from './FileTree';
import { FlowChart } from './FlowChart';
import { Image } from './Image';
import { Markdown } from './Markdown';
import { Mermaid } from './Mermaid';
import { Prototype } from './Prototype';
import { QuestionForm } from './QuestionForm';
import { RichText } from './RichText';
import { Section } from './Section';
import { SequenceDiagram } from './SequenceDiagram';
import { Table } from './Table';
import { Tabs } from './Tabs';
import { Timeline } from './Timeline';
import { BlockErrorBoundary, UnrenderableBlock } from './shared';
import { Wireframe } from './Wireframe';

type BlockComponent = ComponentType<{ node: Extract<MdxNode, { kind: 'element' }> }>;

/** The 22 registered block names mapped to their renderer — this table plus
 * the BLOCK_TYPES const in types.ts plus the fixture in __fixtures__ are the
 * three-way agreement this task pins (renderer / prompt spec / fixture must
 * all describe the same 22 shapes). */
const REGISTRY: Record<string, BlockComponent> = {
  RichText,
  Diagram,
  Code,
  AnnotatedCode,
  Table,
  Callout,
  Checklist,
  FileTree,
  DataModel,
  QuestionForm,
  Wireframe,
  Canvas,
  Prototype,
  Tabs,
  Columns,
  Section,
  Image,
  SequenceDiagram,
  FlowChart,
  Mermaid,
  Timeline,
  EntityRelationship,
};

/** Render one parsed node. Exported (not just used internally) so container
 * blocks (Section/Columns/Tabs) can recurse into their own nested block
 * children through the same dispatch table. */
export function renderNode(node: MdxNode, key: string): ReactNode {
  if (node.kind === 'text') return <Markdown key={key} text={node.markdown} />;
  if (node.kind === 'error') return <UnrenderableBlock key={key} message={node.message} />;

  if (!BLOCK_TYPE_SET.has(node.name)) {
    return (
      <UnrenderableBlock
        key={key}
        name={node.name}
        message={`\`${node.name}\` is not a registered artifact block type`}
      />
    );
  }
  const Component = REGISTRY[node.name];
  if (!Component) {
    return <UnrenderableBlock key={key} name={node.name} message="no renderer registered for this block type" />;
  }
  return (
    <BlockErrorBoundary key={key} name={node.name}>
      <Component node={node} />
    </BlockErrorBoundary>
  );
}

/** Render a node list (top-level document or a container block's children)
 * with vertical rhythm between blocks. */
export function renderNodes(nodes: MdxNode[], keyPrefix = 'n'): ReactNode {
  if (nodes.length === 0) return null;
  return (
    <div className="flex flex-col gap-4">
      {nodes.map((node, index) => renderNode(node, `${keyPrefix}-${index}`))}
    </div>
  );
}
