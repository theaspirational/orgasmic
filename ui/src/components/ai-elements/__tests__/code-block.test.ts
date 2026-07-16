import { describe, expect, it, vi } from "vitest";

const shiki = vi.hoisted(() => ({
  codeToTokens: vi.fn((source: string) => ({
    bg: "transparent",
    fg: "inherit",
    tokens: [[{ color: "inherit", content: source }]],
  })),
}));

vi.mock("shiki", () => ({
  createHighlighter: vi.fn(async () => ({
    codeToTokens: shiki.codeToTokens,
    getLoadedLanguages: () => ["typescript"],
  })),
}));

import { highlightCode } from "../code-block";

const highlight = (source: string) =>
  new Promise<NonNullable<ReturnType<typeof highlightCode>>>((resolve) => {
    const cached = highlightCode(source, "typescript", resolve);
    if (cached) {
      resolve(cached);
    }
  });

describe("highlightCode", () => {
  it("does not reuse tokens for equal-length code with the same prefix and suffix", async () => {
    const prefix = "p".repeat(100);
    const suffix = "s".repeat(100);
    const firstSource = `${prefix}${"a".repeat(40)}${suffix}`;
    const secondSource = `${prefix}${"b".repeat(40)}${suffix}`;

    const first = await highlight(firstSource);
    const second = await highlight(secondSource);

    expect(first.tokens[0]?.[0]?.content).toBe(firstSource);
    expect(second.tokens[0]?.[0]?.content).toBe(secondSource);
    expect(shiki.codeToTokens).toHaveBeenCalledTimes(2);
  });
});
