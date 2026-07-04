import type { MdxNode } from '../types';
import { asArray } from './propUtils';
import { DataModelBody, readEntity, readRelation } from './DataModel';

// Same card-grid rendering as DataModel (an ERD is the same entities/relations
// shape) — kept as a distinct component so the block registry maps both
// PascalCase names to a real component rather than aliasing one to the other.
export function EntityRelationship({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const entities = asArray(node.props.entities).map(readEntity);
  const relations = asArray(node.props.relations).map(readRelation);
  return <DataModelBody entities={entities} relations={relations} />;
}
