import { Children, Fragment, memo, type ReactNode } from 'react';
import ReactMarkdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';

import { decorateText, useRichText } from '@/lib/richText';
import { cn } from '@/lib/utils';

/** Bare TASK-/dec_ ids and glossary phrases in artifact prose get the same
 * entity-peek links as every other prose surface (manager feed, task bodies).
 * Only string children are decorated; nested elements decorate their own
 * strings, and code/pre stay untouched so record paths render literally. */
function Decorated({ children }: { children?: ReactNode }) {
  const ctx = useRichText();
  if (!ctx) return <Fragment>{children}</Fragment>;
  return (
    <Fragment>
      {Children.map(children, (child) =>
        typeof child === 'string' ? <Fragment>{decorateText(child, ctx)}</Fragment> : child,
      )}
    </Fragment>
  );
}

const COMPONENTS: Components = {
  h1: ({ node: _node, children, ...props }) => <h1 className="mb-2 mt-5 text-xl font-semibold tracking-tight first:mt-0" {...props}><Decorated>{children}</Decorated></h1>,
  h2: ({ node: _node, children, ...props }) => <h2 className="mb-2 mt-5 text-lg font-semibold tracking-tight first:mt-0" {...props}><Decorated>{children}</Decorated></h2>,
  h3: ({ node: _node, children, ...props }) => <h3 className="mb-1.5 mt-4 text-base font-semibold first:mt-0" {...props}><Decorated>{children}</Decorated></h3>,
  h4: ({ node: _node, children, ...props }) => <h4 className="mb-1 mt-3 text-sm font-semibold uppercase tracking-wide text-muted-foreground first:mt-0" {...props}><Decorated>{children}</Decorated></h4>,
  p: ({ node: _node, children, ...props }) => <p className="leading-7 [&:not(:first-child)]:mt-3" {...props}><Decorated>{children}</Decorated></p>,
  a: ({ node: _node, ...props }) => (
    <a className="text-primary underline-offset-2 hover:underline" target="_blank" rel="noreferrer" {...props} />
  ),
  ul: ({ node: _node, ...props }) => <ul className="ml-5 list-disc space-y-2 marker:text-muted-foreground/60 [&:not(:first-child)]:mt-3" {...props} />,
  ol: ({ node: _node, ...props }) => <ol className="ml-5 list-decimal space-y-2 marker:text-muted-foreground/60 [&:not(:first-child)]:mt-3" {...props} />,
  li: ({ node: _node, children, ...props }) => <li className="leading-7 pl-1" {...props}><Decorated>{children}</Decorated></li>,
  blockquote: ({ node: _node, ...props }) => (
    <blockquote className="border-l-2 border-border pl-3 text-muted-foreground" {...props} />
  ),
  em: ({ node: _node, children, ...props }) => <em {...props}><Decorated>{children}</Decorated></em>,
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
  td: ({ node: _node, children, ...props }) => <td className="border-b px-2 py-1.5 align-top" {...props}><Decorated>{children}</Decorated></td>,
  hr: ({ node: _node, ...props }) => <hr className="border-border" {...props} />,
  strong: ({ node: _node, children, ...props }) => <strong className="font-semibold" {...props}><Decorated>{children}</Decorated></strong>,
};

/** Untrusted markdown prose (RichText body, Callout body, plain text between
 * blocks). react-markdown never executes embedded HTML/JS — it only ever
 * produces the whitelisted element set above. */
export const Markdown = memo(function Markdown({ text, className }: { text: string; className?: string }) {
  if (!text.trim()) return null;
  return (
    <div className={cn('text-[15px] text-foreground', className)}>
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={COMPONENTS} skipHtml>
        {text}
      </ReactMarkdown>
    </div>
  );
});
