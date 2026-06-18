// Data contract for the generic org-node editor surface. Mirrors the daemon's
// `NodeDoc` projection and `NodeEditOp` enum (crates/orgasmic-daemon/src/api.rs).
// The UI never serializes `.org` text — it reads this document and emits ops.

export type NodeProperty = { key: string; value: string };
export type NodeSection = { title: string; body: string };
export type NodeSource = {
  file: string;
  /** Optimistic-concurrency token; echoed back on edit, 409 on drift. */
  base_version: string;
};

export type OrgNodeDoc = {
  id: string;
  kind: string;
  title: string;
  todo?: string | null;
  tags: string[];
  /** The heading's own free prose (leaf architecture nodes keep their
   *  description here rather than in a named `**` section). */
  body: string;
  properties: NodeProperty[];
  sections: NodeSection[];
  source: NodeSource;
};

export type NodeEditOp =
  | { op: 'set_body'; body: string }
  | { op: 'set_section_body'; title: string; body: string }
  | { op: 'add_section'; title: string; body: string }
  | { op: 'remove_section'; title: string }
  | { op: 'set_property'; key: string; value: string }
  | { op: 'remove_property'; key: string }
  | { op: 'set_title'; title: string }
  | { op: 'set_tags'; tags: string[] };
