// Declarative, per-kind view descriptors for the org-node editor. A new
// editable view derived from an `.org` file is "a descriptor + the shared kit"
// (NodeDocEditor) — no bespoke component per kind.
//
// A field binds a UI control to one org construct (title / tags / a `**`
// section / a property) and picks an editor for it. NodeDocEditor reads the
// bound value out of the document and turns edits back into NodeEditOps.

import type { NodeKind } from '@/components/node-views/orgNodes';

export type FieldBinding =
  | { kind: 'title' }
  | { kind: 'tags' }
  /** The heading's own free prose (doc.body) — leaf nodes keep their
   *  description there rather than in a named `**` section. */
  | { kind: 'body' }
  | { kind: 'section'; title: string }
  | { kind: 'property'; key: string };

export type FieldEditor = 'prose' | 'text' | 'chips';

/** How a property's scalar value is tokenized into chips. */
export type ChipSeparator = 'space' | 'comma';

/** Which node list backs a link-chip's labels and autocomplete. */
export type SuggestSource = 'glossary' | 'decision' | 'architecture' | 'task';

export type NodeFieldDescriptor = {
  label: string;
  binding: FieldBinding;
  editor: FieldEditor;
  /** For chips bound to a property: token separator. Default 'space'. */
  separator?: ChipSeparator;
  /** Chips that reference other nodes — show resolved labels, navigate on click. */
  link?: boolean;
  /** Autocomplete source for chip entry. */
  suggest?: SuggestSource;
  placeholder?: string;
  hideWhenEmpty?: boolean;
};

export type NodeDescriptor = {
  kind: NodeKind | 'project' | 'task';
  editableTitle?: boolean;
  fields: NodeFieldDescriptor[];
  /** Render an editable prose field for every `**` section in the document that
   *  no explicit field already binds — in document order, after the static
   *  fields. Use for free-form documents (e.g. project.org) whose section set
   *  is authored rather than fixed by schema. */
  dynamicSections?: boolean;
};

export const DECISION_DESCRIPTOR: NodeDescriptor = {
  kind: 'decision',
  editableTitle: true,
  fields: [
    { label: 'Tags', binding: { kind: 'tags' }, editor: 'chips', placeholder: 'Add tag…' },
    { label: 'Context', binding: { kind: 'section', title: 'Context' }, editor: 'prose', placeholder: 'What forces are at play?' },
    { label: 'Decision', binding: { kind: 'section', title: 'Decision' }, editor: 'prose', placeholder: 'What did we decide?' },
    { label: 'Consequences', binding: { kind: 'section', title: 'Consequences' }, editor: 'prose', placeholder: 'What follows from this?' },
    { label: 'Glossary', binding: { kind: 'property', key: 'GLOSSARY_REFS' }, editor: 'chips', separator: 'space', link: true, suggest: 'glossary', placeholder: 'Link a glossary term…' },
  ],
};

export const GLOSSARY_DESCRIPTOR: NodeDescriptor = {
  kind: 'glossary',
  editableTitle: true,
  fields: [
    { label: 'Canonical', binding: { kind: 'property', key: 'CANONICAL' }, editor: 'text', placeholder: 'Canonical form' },
    { label: 'Definition', binding: { kind: 'property', key: 'DEFINITION' }, editor: 'prose', placeholder: 'Define the term…' },
    { label: 'Avoid', binding: { kind: 'property', key: 'AVOID' }, editor: 'chips', separator: 'comma', placeholder: 'Add a phrasing to avoid…' },
    { label: 'Relates To', binding: { kind: 'property', key: 'RELATES_TO' }, editor: 'chips', separator: 'space', link: true, suggest: 'glossary', placeholder: 'Link a related term…' },
  ],
};

export const ARCHITECTURE_DESCRIPTOR: NodeDescriptor = {
  kind: 'architecture',
  editableTitle: true,
  fields: [
    { label: 'Purpose', binding: { kind: 'section', title: 'Purpose' }, editor: 'prose', placeholder: 'What does this node own?' },
    { label: 'Interface', binding: { kind: 'property', key: 'INTERFACE' }, editor: 'chips', separator: 'space', placeholder: 'Add an interface keyword…' },
    { label: 'Constraints', binding: { kind: 'property', key: 'CONSTRAINTS' }, editor: 'chips', separator: 'space', placeholder: 'Add a constraint keyword…' },
    { label: 'Depends On', binding: { kind: 'property', key: 'DEPENDS_ON' }, editor: 'chips', separator: 'space', link: true, suggest: 'architecture', placeholder: 'Link an architecture node…' },
    { label: 'Motivating Decisions', binding: { kind: 'property', key: 'MOTIVATED_BY' }, editor: 'chips', separator: 'space', link: true, suggest: 'decision', placeholder: 'Link a decision…' },
    { label: 'Glossary', binding: { kind: 'property', key: 'GLOSSARY_REFS' }, editor: 'chips', separator: 'space', link: true, suggest: 'glossary', placeholder: 'Link a glossary term…' },
    // Leaf-node (arch_NNN.M) scoped test commands. `;`-separated; commands
    // contain spaces, so this is a single text field rather than chips.
    // hideWhenEmpty keeps it off top-level nodes, which carry no :TESTS:.
    { label: 'Tests', binding: { kind: 'property', key: 'TESTS' }, editor: 'text', placeholder: 'cargo test -p <crate>; …', hideWhenEmpty: true },
  ],
};

// Leaf component nodes (arch_NNN.M) speak a different vocabulary than
// top-level architecture nodes: their description is direct body prose (no
// `** Purpose` child), and their properties name code surfaces
// (SOURCE_PATHS / TESTS / CALLS / READS / WRITES / …) rather than
// INTERFACE / CONSTRAINTS keywords — those live on the parent node.
export const ARCHITECTURE_LEAF_DESCRIPTOR: NodeDescriptor = {
  kind: 'architecture',
  editableTitle: true,
  fields: [
    { label: 'Description', binding: { kind: 'body' }, editor: 'prose', placeholder: 'What does this component do?' },
    { label: 'Source Paths', binding: { kind: 'property', key: 'SOURCE_PATHS' }, editor: 'chips', separator: 'space', placeholder: 'Add a source path…' },
    { label: 'Tests', binding: { kind: 'property', key: 'TESTS' }, editor: 'text', placeholder: 'cargo test -p <crate>; …' },
    { label: 'Calls', binding: { kind: 'property', key: 'CALLS' }, editor: 'chips', separator: 'space', link: true, suggest: 'architecture', placeholder: 'Link a called node…' },
    { label: 'Depends On', binding: { kind: 'property', key: 'DEPENDS_ON' }, editor: 'chips', separator: 'space', link: true, suggest: 'architecture', placeholder: 'Link an architecture node…' },
    { label: 'Reads', binding: { kind: 'property', key: 'READS' }, editor: 'chips', separator: 'space', placeholder: 'file:… topic:…', hideWhenEmpty: true },
    { label: 'Writes', binding: { kind: 'property', key: 'WRITES' }, editor: 'chips', separator: 'space', placeholder: 'file:… topic:…', hideWhenEmpty: true },
    { label: 'Exposes REST', binding: { kind: 'property', key: 'EXPOSES_REST' }, editor: 'chips', separator: 'space', placeholder: 'Add an endpoint…', hideWhenEmpty: true },
    { label: 'Exposes WS', binding: { kind: 'property', key: 'EXPOSES_WS' }, editor: 'chips', separator: 'space', placeholder: 'Add a ws route…', hideWhenEmpty: true },
    { label: 'Subscribes To', binding: { kind: 'property', key: 'SUBSCRIBES_TO' }, editor: 'chips', separator: 'space', placeholder: 'Add a topic…', hideWhenEmpty: true },
    { label: 'Spawns', binding: { kind: 'property', key: 'SPAWNS' }, editor: 'chips', separator: 'space', placeholder: 'Add a spawned worker…', hideWhenEmpty: true },
  ],
};

const ARCH_LEAF_ID = /^arch_\d+\.\d+$/;

/** Pick the architecture descriptor variant for a node id: leaf component
 *  nodes (`arch_NNN.M`) get the component vocabulary, everything else the
 *  top-level one. */
export function architectureDescriptorFor(id: string): NodeDescriptor {
  return ARCH_LEAF_ID.test(id) ? ARCHITECTURE_LEAF_DESCRIPTOR : ARCHITECTURE_DESCRIPTOR;
}

// project.org's PROJECT heading has an authored, open-ended section set
// (Mission, Operating Constraints, Product Baseline, …). Render them all rather
// than pinning a fixed list, so newly authored sections appear automatically.
export const PROJECT_DESCRIPTOR: NodeDescriptor = {
  kind: 'project',
  editableTitle: false,
  dynamicSections: true,
  fields: [],
};

export const TASK_DESCRIPTOR: NodeDescriptor = {
  kind: 'task',
  editableTitle: true,
  fields: [
    { label: 'Description', binding: { kind: 'section', title: 'Description' }, editor: 'prose', placeholder: 'Describe the task...' },
    { label: 'Acceptance Criteria', binding: { kind: 'section', title: 'Acceptance Criteria' }, editor: 'prose', placeholder: 'Add concrete acceptance criteria...' },
    { label: 'Evidence', binding: { kind: 'section', title: 'Evidence' }, editor: 'prose', placeholder: 'Record verification evidence...', hideWhenEmpty: true },
    { label: 'Notes', binding: { kind: 'section', title: 'Notes' }, editor: 'prose', placeholder: 'Add implementation notes...', hideWhenEmpty: true },
    { label: 'Worklog', binding: { kind: 'section', title: 'Worklog' }, editor: 'prose', placeholder: 'Record worklog entries...', hideWhenEmpty: true },
    { label: 'Reviewer Pass', binding: { kind: 'section', title: 'Reviewer pass' }, editor: 'prose', placeholder: 'Record reviewer pass notes...', hideWhenEmpty: true },
    { label: 'Tags', binding: { kind: 'tags' }, editor: 'chips', placeholder: 'Add tag...' },
    { label: 'Parent Task', binding: { kind: 'property', key: 'PARENT_TASK' }, editor: 'chips', separator: 'space', link: true, suggest: 'task', placeholder: 'Link a parent task...', hideWhenEmpty: true },
    { label: 'Depends On', binding: { kind: 'property', key: 'DEPENDS_ON' }, editor: 'chips', separator: 'space', link: true, suggest: 'task', placeholder: 'Link a dependency...', hideWhenEmpty: true },
    { label: 'Blocked By', binding: { kind: 'property', key: 'BLOCKED_BY' }, editor: 'chips', separator: 'space', link: true, suggest: 'task', placeholder: 'Link a blocker...', hideWhenEmpty: true },
    { label: 'Write Scope', binding: { kind: 'property', key: 'WRITE_SCOPE' }, editor: 'chips', separator: 'space', placeholder: 'Add write scope...', hideWhenEmpty: true },
    { label: 'Read Scope', binding: { kind: 'property', key: 'READ_SCOPE' }, editor: 'chips', separator: 'space', placeholder: 'Add read scope...', hideWhenEmpty: true },
    { label: 'Test Command', binding: { kind: 'property', key: 'TEST_CMD' }, editor: 'text', placeholder: 'Command to verify this task...', hideWhenEmpty: true },
  ],
};

export const DESCRIPTORS: Record<NodeKind, NodeDescriptor> = {
  decision: DECISION_DESCRIPTOR,
  architecture: ARCHITECTURE_DESCRIPTOR,
  glossary: GLOSSARY_DESCRIPTOR,
};
