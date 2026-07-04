import type { MdxNode } from '../types';
import { asOptionalString, asString } from './propUtils';
import { UnrenderableBlock } from './shared';

function isSafeImageSrc(src: string): boolean {
  return /^https?:\/\//i.test(src) || /^data:image\/(png|jpe?g|gif|webp|svg\+xml);base64,/i.test(src);
}

export function Image({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const src = asString(node.props.src);
  const alt = asString(node.props.alt, 'Artifact image');
  const caption = asOptionalString(node.props.caption);
  if (!src) return <UnrenderableBlock name="Image" message="missing `src`" />;
  if (!isSafeImageSrc(src)) {
    return <UnrenderableBlock name="Image" message={`unsupported image source scheme in \`${src.slice(0, 40)}\``} />;
  }
  return (
    <figure className="flex flex-col gap-1.5">
      <img src={src} alt={alt} className="max-w-full rounded-lg border" loading="lazy" />
      {caption ? <figcaption className="text-xs text-muted-foreground">{caption}</figcaption> : null}
    </figure>
  );
}
