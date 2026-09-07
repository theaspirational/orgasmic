# ACP chat delivery verification — 2026-09-07

Implemented on branch `codex/acp-chat-incremental` in `/Users/aspirational/Documents/code/tools/orgasmic-acp-chat`, based on `216b28fd`. The original checkout had concurrent changes, so it was preserved. At the time of this initial verification, the changes were local: no commit, merge, runtime installation, or daemon restart had been performed. See the completion recheck below for subsequent verification.

## Delivered behavior

- `/api/ws/transcript/:run_id` sends a replay once, then only new persisted envelopes. It tails file offsets, carries incomplete UTF-8/JSONL lines, detects replacement/truncation, and reconciles missed bus notifications. Streaming requires `sessions.watch` for the run's project.
- The frontend keeps an incremental reducer. Reconnect snapshots replace state; duplicate deliveries are ignored and sequence gaps trigger recovery. A separate delivery sequence handles session writers that restart their stored sequence when reopened.
- Every Chat provider uses the same Rust ACP adapter and existing owned stdio/JSON-RPC transport. Native ACP runs OpenCode, Cursor, and Hermes; pinned upstream adapters run Codex and Claude. Worker dispatch retains its established transport registry.
- Model/config choices come from ACP discovery. Unsupported choices fail explicitly. Hermes's legacy model selection works at launch; its UI requires a new chat to change models because it does not advertise the newer live config interface.
- Tool and reasoning activity is grouped between assistant prose messages. Closed groups show the latest three tool lines and a failure count. Expanding preserves activity order; reasoning remains separately expandable. Assistant text is kept in its original position.
- Original ACP payloads are retained under the existing session payload limits. Unknown updates get an inspectable fallback; schema mismatches produce a warning rather than silently dropping data. Large text/reasoning chunks are split before persistence to preserve prose while respecting the writer's per-payload limits. Oversized tool/extension payloads still use existing digest retention.

## GitHub and UI research

The shared implementation uses the [official Rust ACP schema](https://github.com/agentclientprotocol/rust-sdk) with Orgasmic's existing transport and process ownership. It does not introduce a second connection runtime. Launchers use [codex-acp](https://github.com/agentclientprotocol/codex-acp) at `1.10.0` and [claude-agent-acp](https://github.com/agentclientprotocol/claude-agent-acp) at `0.75.1`.

Native entry points are documented by [OpenCode](https://opencode.ai/docs/acp/), [Cursor](https://cursor.com/docs/cli/acp), and [Hermes](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/features/acp.md). The UI follows grouped activity and progressive disclosure patterns from [AI Elements Chain of Thought](https://elements.ai-sdk.dev/components/chain-of-thought) and [Reasoning](https://elements.ai-sdk.dev/components/reasoning), using the components already vendored in this repository.

## Live harness probes

Each provider read a three-line sample, created and checked its reversal, exercised command/tool output, and returned Markdown. The requested exit-7 command exposed different provider failure encodings. All five produced the expected `result.txt` content: `gamma`, `beta`, `alpha`. Each then completed two short prompts in one reusable session.

| Provider | Selected inexpensive model | Assistant chunks | Reasoning chunks | Tool starts / updates |
| --- | --- | ---: | ---: | ---: |
| Codex | `gpt-5.4-mini` | 166 | 14 | 5 / 5 |
| Claude | `haiku` | 8 | 57 | 9 / 30 |
| OpenCode | `opencode/mimo-v2.5-free` | 34 | 188 | 6 / 17 |
| Cursor | `gpt-5.4-nano[reasoning=medium]` | 233 | 358 | 5 / 10 |
| Hermes | `opencode-free:mimo-v2.5-free` | 1 | 37 | 4 / 2 |

Counts describe the retained first-turn fixtures, not all later probes. The fixtures retain text, reasoning, tool updates, permission events, and turn completion; bulky discovery catalogs were excluded. Their hashes and counts are in [fixtures.json](fixtures.json). Total billing was not measured.

Claude's first attempt failed with a 401 from an inherited `ANTHROPIC_AUTH_TOKEN`. A retry removed that variable in the probe subprocess and used the available native login. No credentials or global environment were changed.

The probes found and fixed dropped follow-up JSON-RPC responses, duplicate vendor user echoes, whitespace-only text loss, incorrect failures on intentional shutdown, and providers reporting `completed` alongside a nonzero numeric command exit. ACP protocol failures remain visible. The last catalog-only lifecycle checks generated no model turns, emitted one run completion per provider, and left no owned process groups alive: [cleanup.json](cleanup.json).

## Runnable checks and results

From the worktree root:

```sh
cargo test -p orgasmic-drivers --lib
cargo test -p orgasmic-daemon transcript_ --lib
cargo test -p orgasmic-daemon chat_access_modes --lib
cargo check -p orgasmic-cli
```

From `ui`:

```sh
npm test
npm run build
```

Results: 250 driver tests passed, 3 existing tests ignored; 5 daemon transcript tests and the access-mode test passed; 371 UI tests across 61 files passed; CLI check and UI typecheck/build passed. Vite reports its large-chunk warning. Logs are saved beside this report.

Replay tests feed the five real captures incrementally and as full snapshots, assert exact assistant/reasoning text equality (including whitespace), preserve tool IDs and failures, and test duplicates, gaps, writer sequence restarts, unknown/non-text payloads, and vendor echoes. Rust checks exercise actual session persistence for a long Unicode reply, permission option IDs, relative paths/symlink escapes, response correlation, and reusable sessions.

Real Chromium exercised the production transcript component and WebSocket hook with controlled frames at 1280px and 390px: three preview lines, eight expandable tools, expandable reasoning, all prose retained, reconnect without duplicate text, no session refetch requests, no horizontal overflow or browser errors. [Desktop](desktop.png), [mobile](mobile.png), and [browser results](browser.jsonl). The temporary test page was removed. The UI design detector reported no findings.

To run another explicitly billed live probe against a disposable directory:

```sh
cargo run -p orgasmic-drivers --example acp_probe -- codex /absolute/disposable/directory gpt-5.4-mini 'Your bounded prompt' 'Optional second prompt'
```

Omitting model and prompts runs discovery/release only. The example prints the owned process group, events, discovered catalog, and release result. Original local probe output remains in `/tmp/orgasmic-acp-probes`.

## Coverage limits

This is observed coverage plus forward-compatible inspection, not a guarantee that every future vendor extension has a dedicated rendering. Image/non-text content and unknown updates have synthetic fallback tests; audio, rich multimodal rendering, vendor-specific interactive forms, and every cancellation/permission outcome were not exhaustively exercised live. An upstream adapter can omit information before ACP reaches Orgasmic. Unknown payloads remain inspectable subject to the documented retention limits; the UI shows only reasoning actually emitted by the provider.

## Completion recheck — 2026-09-07

The repository sweep inspected all 42 local branches and 41 registered worktrees. All existing source branch commits were already contained in `main`; only this ACP worktree had uncommitted source changes. The other 10 live worktrees (including the CLI-owned ledger) were clean. The 30 missing checkout registrations had no staged changes or unfinished Git operations; their absent working files cannot be recovered or inspected by this sweep. No stash entries were present.

Fresh local checks passed: 250 driver tests (3 ignored), 5 daemon transcript tests including the WebSocket membership/replay test, the chat-access test, all 371 UI tests, production UI build, CLI compilation, Rust formatting, and whitespace validation. The current main branch's classifier self-test also passed all 80 cases. Completion corrections were Rust module ordering and trailing blank lines in two archived test logs. The Vite large-chunk warning remains. No workspace-wide Cargo test was run.

A fresh Chromium check loaded the production transcript component and WebSocket hook with controlled frames at 1280px and 390px. Both widths showed three collapsed previews, eight expanded tools, expandable reasoning and retained prose, with no horizontal overflow or JavaScript errors. Disconnect/reconnect replaced the snapshot without duplication; a duplicated live append appeared once; no session-refetch requests occurred. The temporary test page was removed. No provider model calls, runtime installation, or live daemon restart were performed during this recheck.
