import { Key, Link2 } from 'lucide-react';

import { cn } from '@/lib/utils';
import type { AttrValue, MdxNode } from '../types';
import { asArray, asBool, asOptionalString, asRecord, asString } from './propUtils';

export type Field = { name: string; type: string; pk: boolean; fk: boolean; nullable: boolean };
export type Entity = { name: string; fields: Field[] };
export type Relation = { from: string; to: string; label?: string };

function readField(raw: AttrValue): Field {
  const record = asRecord(raw);
  return {
    name: asString(record.name),
    type: asString(record.type),
    pk: asBool(record.pk),
    fk: asBool(record.fk),
    nullable: asBool(record.nullable),
  };
}

export function readEntity(raw: AttrValue): Entity {
  const record = asRecord(raw);
  return {
    name: asString(record.name, '?'),
    fields: asArray(record.fields).map(readField),
  };
}

export function readRelation(raw: AttrValue): Relation {
  const record = asRecord(raw);
  return { from: asString(record.from), to: asString(record.to), label: asOptionalString(record.label) };
}

export function DataModelBody({ entities, relations }: { entities: Entity[]; relations: Relation[] }) {
  if (entities.length === 0) return null;
  return (
    <div className="flex flex-col gap-3">
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
        {entities.map((entity, index) => (
          <div key={index} className="overflow-hidden rounded-lg border">
            <div className="border-b bg-muted/40 px-3 py-1.5 text-sm font-semibold">{entity.name}</div>
            <ul className="divide-y">
              {entity.fields.map((field, fieldIndex) => (
                <li key={fieldIndex} className="flex items-center justify-between gap-2 px-3 py-1 text-xs">
                  <span className="flex min-w-0 items-center gap-1.5">
                    {field.pk ? <Key className="size-3 shrink-0 text-primary" /> : null}
                    {field.fk ? <Link2 className="size-3 shrink-0 text-muted-foreground" /> : null}
                    <span className={cn('truncate font-mono', field.pk && 'font-semibold')}>{field.name}</span>
                  </span>
                  <span className="shrink-0 font-mono text-muted-foreground">
                    {field.type}
                    {field.nullable ? '?' : ''}
                  </span>
                </li>
              ))}
            </ul>
          </div>
        ))}
      </div>
      {relations.length > 0 ? (
        <div className="rounded-lg border bg-muted/20 p-2.5">
          <p className="mb-1.5 text-xs font-medium text-muted-foreground">Relations</p>
          <ul className="flex flex-col gap-1 font-mono text-xs">
            {relations.map((relation, index) => (
              <li key={index} className="flex items-center gap-1.5">
                <span>{relation.from}</span>
                <span aria-hidden="true">→</span>
                <span>{relation.to}</span>
                {relation.label ? <span className="text-muted-foreground">({relation.label})</span> : null}
              </li>
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}

export function DataModel({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const entities = asArray(node.props.entities).map(readEntity);
  const relations = asArray(node.props.relations).map(readRelation);
  return <DataModelBody entities={entities} relations={relations} />;
}
