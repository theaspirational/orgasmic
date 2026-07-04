import { useMemo } from 'react';

import type { MdxNode } from '../types';
import { textBody } from '../parseMdx';
import { renderableFragment } from '../sanitize';
import { asBool, asString } from './propUtils';
import { UnrenderableBlock, WireframeFrame, isWireframeSurface } from './shared';

export function Wireframe({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const rawHtml = textBody(node, 'html');
  const surfaceProp = asString(node.props.surface, 'browser');
  const skeleton = asBool(node.props.skeleton);
  const html = useMemo(() => renderableFragment(rawHtml), [rawHtml]);

  if (!rawHtml.trim()) return null;
  if (!isWireframeSurface(surfaceProp)) {
    return <UnrenderableBlock name="Wireframe" message={`unknown surface "${surfaceProp}"`} />;
  }

  return <WireframeFrame surface={surfaceProp} html={html} skeleton={skeleton} />;
}
