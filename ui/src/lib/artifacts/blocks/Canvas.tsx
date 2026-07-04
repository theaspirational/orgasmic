import type { MdxNode } from '../types';
import { textBody } from '../parseMdx';
import { renderableFragment } from '../sanitize';
import { asBool, asOptionalString, asString } from './propUtils';
import { UnrenderableBlock, WireframeFrame, isWireframeSurface } from './shared';

/**
 * Multiple wireframe artboards laid out together (~canvas.md). The real
 * spatial board (x/y lanes, connectors, floating annotations) is a
 * deliberately deferred simplification here — this renders each `<Screen>`
 * as a labeled artboard in a wrapping gallery, which is enough to browse a
 * generated multi-screen flow inline in the document.
 */
export function Canvas({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const screens = node.children.filter(
    (child): child is Extract<MdxNode, { kind: 'element' }> => child.kind === 'element' && child.name === 'Screen',
  );
  if (screens.length === 0) {
    return <UnrenderableBlock name="Canvas" message="no <Screen> artboards found" />;
  }
  return (
    <div className="flex flex-wrap gap-4">
      {screens.map((screen, index) => {
        const surface = asString(screen.props.surface, 'browser');
        const label = asOptionalString(screen.props.label);
        const html = renderableFragment(textBody(screen, 'html'));
        if (!isWireframeSurface(surface)) {
          return <UnrenderableBlock key={index} name="Screen" message={`unknown surface "${surface}"`} />;
        }
        return (
          <WireframeFrame
            key={index}
            surface={surface}
            html={html}
            skeleton={asBool(screen.props.skeleton)}
            label={label}
            className="flex-1"
          />
        );
      })}
    </div>
  );
}
