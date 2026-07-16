// @vitest-environment jsdom
import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { MessageResponse } from "../message";
import {
  StreamdownImage,
  StreamdownMermaidError,
} from "../streamdown-config";

afterEach(cleanup);

describe("Streamdown integration", () => {
  it("renders dependency-only link wrapping and KaTeX math output", () => {
    const { container } = render(
      <MessageResponse mode="static">
        {"[long link](https://example.com/a/very/long/path)\n\n$$x^2 + y^2$$"}
      </MessageResponse>
    );

    expect(container.querySelector('[data-streamdown="link"]')).toHaveClass("wrap-anywhere");
    expect(container.querySelector(".katex")).toBeInTheDocument();
  });

  it("uses semantic tokens for unavailable-image and Mermaid-error fallbacks", () => {
    const retry = vi.fn();
    const image = render(<StreamdownImage alt="blocked diagram" />);

    expect(screen.getByRole("note")).toHaveClass("bg-muted", "text-muted-foreground");
    image.unmount();

    render(
      <StreamdownMermaidError chart="graph TD; A-->" error="Parse error" retry={retry} />
    );

    expect(screen.getByRole("alert")).toHaveClass(
      "border-destructive/40",
      "bg-destructive/10",
      "text-destructive"
    );
    fireEvent.click(screen.getByRole("button", { name: "Retry diagram" }));
    expect(retry).toHaveBeenCalledOnce();
  });
});
