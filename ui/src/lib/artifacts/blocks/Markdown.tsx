import { memo } from 'react';
import ReactMarkdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';

import { cn } from '@/lib/utils';

const COMPONENTS: Components = {
  h1: ({ node: _node, ...props }) => <h1 className="mb-2 mt-4 text-lg font-semibold first:mt-0" {...props} />,
  h2: ({ node: _node, ...props }) => <h2 className="mb-2 mt-4 text-base font-semibold first:mt-0" {...props} />,
  h3: ({ node: _node, ...props }) => <h3 className="mb-1.5 mt-3 text-sm font-semibold first:mt-0" {...props} />,
  h4: ({ node: _node, ...props }) => <h4 className="mb-1 mt-3 text-sm font-medium first:mt-0" {...props} />,
  p: ({ node: _node, ...props }) => <p className="leading-relaxed [&:not(:first-child)]:mt-2" {...props} />,
  a: ({ node: _node, ...props }) => (
    <a className="text-primary underline-offset-2 hover:underline" target="_blank" rel="noreferrer" {...props} />
  ),
  ul: ({ node: _node, ...props }) => <ul className="ml-5 list-disc space-y-1" {...props} />,
  ol: ({ node: _node, ...props }) => <ol className="ml-5 list-decimal space-y-1" {...props} />,
  li: ({ node: _node, ...props }) => <li className="leading-relaxed" {...props} />,
  blockquote: ({ node: _node, ...props }) => (
    <blockquote className="border-l-2 border-border pl-3 text-muted-foreground" {...props} />
  ),
  code: ({ node: _node, className, ...props }) => (
    <code
      className={cn('rounded bg-muted px-1 py-0.5 font-mono text-[0.9em] text-muted-foreground', className)}
      {...props}
    />
  ),
  pre: ({ node: _node, ...props }) => (
    <pre className="overflow-x-auto rounded-md border bg-muted/40 p-3 font-mono text-xs" {...props} />
  ),
  table: ({ node: _node, ...props }) => (
    <div className="overflow-x-auto">
      <table className="w-full border-collapse text-sm" {...props} />
    </div>
  ),
  thead: ({ node: _node, ...props }) => <thead className="border-b" {...props} />,
  th: ({ node: _node, ...props }) => <th className="px-2 py-1.5 text-left font-medium text-muted-foreground" {...props} />,
  td: ({ node: _node, ...props }) => <td className="border-b px-2 py-1.5 align-top" {...props} />,
  hr: ({ node: _node, ...props }) => <hr className="border-border" {...props} />,
  strong: ({ node: _node, ...props }) => <strong className="font-semibold" {...props} />,
};

/** Untrusted markdown prose (RichText body, Callout body, plain text between
 * blocks). react-markdown never executes embedded HTML/JS — it only ever
 * produces the whitelisted element set above. */
export const Markdown = memo(function Markdown({ text, className }: { text: string; className?: string }) {
  if (!text.trim()) return null;
  return (
    <div className={cn('text-sm text-foreground', className)}>
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={COMPONENTS} skipHtml>
        {text}
      </ReactMarkdown>
    </div>
  );
});
