import { File, Folder } from 'lucide-react';

import type { AttrValue, MdxNode } from '../types';
import { asArray, asOptionalString, asRecord, asString } from './propUtils';
import { BlockCard } from './shared';

type FileNode = { name: string; type: 'file' | 'dir'; note?: string; children: FileNode[] };

function readNode(raw: AttrValue): FileNode {
  const record = asRecord(raw);
  const type = asString(record.type) === 'dir' || asString(record.type) === 'directory' ? 'dir' : 'file';
  return {
    name: asString(record.name, asString(raw, '?')),
    type,
    note: asOptionalString(record.note),
    children: asArray(record.children).map(readNode),
  };
}

function FileTreeRow({ node, depth }: { node: FileNode; depth: number }) {
  const Icon = node.type === 'dir' ? Folder : File;
  return (
    <li>
      <div className="flex items-center gap-1.5 py-0.5" style={{ paddingLeft: `${depth * 1.1}rem` }}>
        <Icon className="size-3.5 shrink-0 text-muted-foreground" />
        <span className="truncate font-mono text-xs">{node.name}</span>
        {node.note ? <span className="truncate text-xs text-muted-foreground">— {node.note}</span> : null}
      </div>
      {node.children.length > 0 ? (
        <ul>
          {node.children.map((child, index) => (
            <FileTreeRow key={index} node={child} depth={depth + 1} />
          ))}
        </ul>
      ) : null}
    </li>
  );
}

export function FileTree({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const nodes = asArray(node.props.nodes ?? node.props.tree).map(readNode);
  if (nodes.length === 0) return null;
  return (
    <BlockCard padded={false} className="p-2">
      <ul>
        {nodes.map((entry, index) => (
          <FileTreeRow key={index} node={entry} depth={0} />
        ))}
      </ul>
    </BlockCard>
  );
}
