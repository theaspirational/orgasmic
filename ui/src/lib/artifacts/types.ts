// AST for parsed Agent-Native artifact.mdx content. See parseMdx.ts for the
// parser and blocks/index.tsx for the renderer that consumes this tree.

/** The 22 registered top-level block names, mirrored from
 * crates/orgasmic-daemon/src/artifacts.rs::BLOCK_TYPES. */
export const BLOCK_TYPES = [
  'RichText',
  'Diagram',
  'Code',
  'AnnotatedCode',
  'Table',
  'Callout',
  'Checklist',
  'FileTree',
  'DataModel',
  'QuestionForm',
  'Wireframe',
  'Canvas',
  'Prototype',
  'Tabs',
  'Columns',
  'Section',
  'Image',
  'SequenceDiagram',
  'FlowChart',
  'Mermaid',
  'Timeline',
  'EntityRelationship',
] as const;

export type BlockName = (typeof BLOCK_TYPES)[number];

export const BLOCK_TYPE_SET: ReadonlySet<string> = new Set(BLOCK_TYPES);

/** Structural wrapper tags recognized only when nested inside a specific
 * parent block (Column inside Columns, Tab inside Tabs, Screen inside Canvas
 * or Prototype). Never valid at the document top level. */
export const STRUCTURAL_WRAPPERS = ['Column', 'Tab', 'Screen'] as const;
export type StructuralWrapper = (typeof STRUCTURAL_WRAPPERS)[number];

export type AttrValue = string | number | boolean | null | AttrValue[] | { [key: string]: AttrValue };

export type MdxNode =
  | { kind: 'text'; markdown: string }
  | { kind: 'error'; message: string; raw?: string }
  | {
      kind: 'element';
      name: string;
      props: Record<string, AttrValue>;
      children: MdxNode[];
    };

export type ParsedArtifact = {
  nodes: MdxNode[];
};
