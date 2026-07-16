// @vitest-environment jsdom
import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  scrollToBottom: vi.fn(),
}));

vi.mock("motion/react", () => ({
  useReducedMotion: () => true,
}));

vi.mock("use-stick-to-bottom", async () => {
  const React = await import("react");

  const StickToBottom = ({
    children,
    initial,
    resize,
    ...props
  }: {
    children: React.ReactNode;
    initial?: unknown;
    resize?: unknown;
  }) =>
    React.createElement("div", {
      ...props,
      "data-initial": initial,
      "data-resize": resize,
      children,
    });

  StickToBottom.Content = ({ children, ...props }: React.ComponentProps<"div">) =>
    React.createElement("div", { ...props, children });

  return {
    StickToBottom,
    useStickToBottomContext: () => ({
      isAtBottom: false,
      scrollToBottom: mocks.scrollToBottom,
    }),
  };
});

import {
  Conversation,
  ConversationContent,
  ConversationScrollButton,
} from "../conversation";

afterEach(() => {
  cleanup();
  mocks.scrollToBottom.mockReset();
});

describe("Conversation reduced motion", () => {
  it("uses instant resize, initial, and manual scrolling", () => {
    render(
      <Conversation>
        <ConversationContent>Transcript</ConversationContent>
        <ConversationScrollButton aria-label="Scroll to latest" />
      </Conversation>
    );

    expect(screen.getByRole("log")).toHaveAttribute("data-initial", "instant");
    expect(screen.getByRole("log")).toHaveAttribute("data-resize", "instant");

    fireEvent.click(screen.getByRole("button", { name: "Scroll to latest" }));
    expect(mocks.scrollToBottom).toHaveBeenCalledWith("instant");
  });
});
