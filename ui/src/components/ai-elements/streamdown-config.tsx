"use client";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import type { ComponentProps, SyntheticEvent } from "react";
import { useCallback, useState } from "react";
import type {
  Components,
  MermaidErrorComponentProps,
  MermaidOptions,
} from "streamdown";

type StreamdownImageProps = ComponentProps<"img"> & { node?: unknown };

export const StreamdownImage = ({
  alt,
  className,
  node: _node,
  onError,
  src,
  ...props
}: StreamdownImageProps) => {
  const [failed, setFailed] = useState(false);
  const handleError = useCallback(
    (event: SyntheticEvent<HTMLImageElement>) => {
      setFailed(true);
      onError?.(event);
    },
    [onError]
  );

  if (!src || failed) {
    return (
      <span
        className={cn(
          "inline-flex max-w-full rounded-md border border-border bg-muted px-2 py-1 text-muted-foreground text-xs italic",
          className
        )}
        data-streamdown="image-fallback"
        role="note"
      >
        {alt ? `Image unavailable: ${alt}` : "Image unavailable"}
      </span>
    );
  }

  return (
    <img
      alt={alt ?? ""}
      className={cn("max-w-full rounded-lg border border-border bg-muted", className)}
      data-streamdown="image"
      onError={handleError}
      src={src}
      {...props}
    />
  );
};

export const StreamdownMermaidError = ({
  chart,
  error,
  retry,
}: MermaidErrorComponentProps) => (
  <div
    className="rounded-md border border-destructive/40 bg-destructive/10 p-4 text-destructive"
    role="alert"
  >
    <p className="break-words font-mono text-sm">Mermaid error: {error}</p>
    <div className="mt-3 flex flex-wrap items-start gap-2">
      <Button
        className="border-destructive/40 text-destructive hover:bg-destructive/10 hover:text-destructive"
        onClick={retry}
        size="sm"
        type="button"
        variant="outline"
      >
        Retry diagram
      </Button>
      <details className="min-w-0 flex-1 text-xs">
        <summary className="cursor-pointer select-none font-medium">Show code</summary>
        <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap break-words rounded-md border border-border bg-background p-2 text-foreground">
          {chart}
        </pre>
      </details>
    </div>
  </div>
);

export const streamdownComponents = {
  img: StreamdownImage,
} satisfies Components;

export const streamdownMermaidOptions = {
  errorComponent: StreamdownMermaidError,
} satisfies MermaidOptions;
