/**
 * Tests for the daemon-only `list_terminals` path (Issue #13821).
 *
 * `listTerminals()` used to fall back to a `~/.loom/state.json` snapshot when
 * the daemon was unreachable. That file's only writer was the Tauri desktop
 * app removed in #3353 (and, for the home-directory copy, the now-removed
 * `set_primary_terminal`), so the fallback could only ever return a stale or
 * empty list — the unverified result #4794 had to warn about.
 *
 * The replacement contract, asserted here: an unreachable daemon REJECTS.
 * "The daemon did not answer" must never be rendered as the indistinguishable
 * "there are no terminals" (`.loom/docs/fail-closed.md`).
 */

import { afterEach, describe, expect, it, vi } from "vitest";

const sendDaemonRequest = vi.hoisted(() => vi.fn());
vi.mock("../shared/daemon.js", () => ({ sendDaemonRequest }));

import { handleTerminalTool } from "./terminals.js";

afterEach(() => {
  vi.clearAllMocks();
});

describe("list_terminals (daemon-only since #13821)", () => {
  it("lists the terminals the daemon reports", async () => {
    sendDaemonRequest.mockResolvedValueOnce({
      type: "TerminalList",
      payload: {
        terminals: [
          {
            id: "terminal-1",
            name: "Builder",
            role: "builder",
            working_dir: "/repo",
            tmux_session: "loom-terminal-1",
            created_at: 0,
          },
        ],
      },
    });

    const [{ text }] = await handleTerminalTool("list_terminals");

    expect(text).toContain("=== Active Loom Terminals (1) ===");
    expect(text).toContain("ID: terminal-1");
  });

  it("propagates the connection error when the daemon is unreachable", async () => {
    sendDaemonRequest.mockRejectedValueOnce(new Error("connect ENOENT /tmp/loom-daemon.sock"));

    // Rejecting is the point: index.ts turns this into an `isError` tool
    // result, so the caller is told the daemon did not answer instead of
    // being handed a confident, empty, unverified list.
    await expect(handleTerminalTool("list_terminals")).rejects.toThrow("connect ENOENT");
  });

  it("reports an empty registry only when the daemon actually answered", async () => {
    sendDaemonRequest.mockResolvedValueOnce({ type: "TerminalList", payload: { terminals: [] } });

    const [{ text }] = await handleTerminalTool("list_terminals");

    expect(text).toContain("No active terminals found");
    // The retired state-file fallback appended an "UNVERIFIED" caveat here;
    // a daemon-confirmed empty registry needs no caveat.
    expect(text).not.toContain("UNVERIFIED");
  });

  it.each([
    { type: "Error", message: "registry unavailable" },
    { type: "StructuredError", payload: { message: "registry unavailable" } },
  ])("propagates the daemon's $type message", async (reply) => {
    sendDaemonRequest.mockResolvedValueOnce(reply);

    await expect(handleTerminalTool("list_terminals")).rejects.toThrow("registry unavailable");
  });

  it.each([
    { type: "Success", payload: { terminals: [] } },
    { type: "TerminalList", payload: {} },
    { type: "TerminalList", payload: { terminals: "" } },
    { type: "TerminalList", payload: { terminals: {} } },
  ])("rejects an unusable registry reply %j", async (reply) => {
    sendDaemonRequest.mockResolvedValueOnce(reply);

    await expect(handleTerminalTool("list_terminals")).rejects.toThrow(/ListTerminals/);
  });
});

describe("create_terminal", () => {
  it("stops before creation when the registry lookup fails", async () => {
    sendDaemonRequest.mockResolvedValueOnce({
      type: "StructuredError",
      payload: { message: "registry unavailable" },
    });

    const [{ text }] = await handleTerminalTool("create_terminal");

    expect(sendDaemonRequest.mock.calls).toEqual([[{ type: "ListTerminals" }]]);
    expect(text).toContain("registry unavailable");
  });
});

describe("daemon error replies keep the daemon's text (#15431)", () => {
  const REGISTRY = {
    type: "TerminalList",
    payload: {
      terminals: [
        {
          id: "terminal-1",
          name: "Builder",
          role: "builder",
          working_dir: "/repo",
          tmux_session: "loom-terminal-1",
          created_at: 0,
        },
      ],
    },
  };

  const FRAMES = [
    {
      shape: "a legacy Error with its message under payload",
      reply: { type: "Error", payload: { message: "boom" } },
      expected: "boom",
    },
    {
      shape: "a legacy Error with its message at the top level",
      reply: { type: "Error", message: "boom" },
      expected: "boom",
    },
    {
      shape: "a legacy Error with no message at all",
      reply: { type: "Error" },
      expected: "Unknown error",
    },
    {
      shape: "a StructuredError",
      reply: { type: "StructuredError", payload: { message: "boom" } },
      expected: "boom",
    },
  ];

  const TOOLS = [
    {
      tool: "create_terminal",
      heading: "Create Terminal",
      args: { role: "builder" },
      earlierReplies: [REGISTRY],
    },
    {
      tool: "delete_terminal",
      heading: "Delete Terminal",
      args: { terminal_id: "terminal-1" },
      earlierReplies: [],
    },
    {
      tool: "restart_terminal",
      heading: "Restart Terminal",
      args: { terminal_id: "terminal-1" },
      earlierReplies: [REGISTRY, { type: "Success" }],
    },
  ];

  it.each(FRAMES)(
    "list_terminals rejects with '$expected' for $shape",
    async ({ reply, expected }) => {
      sendDaemonRequest.mockResolvedValueOnce(reply);

      await expect(handleTerminalTool("list_terminals")).rejects.toThrow(expected);
    }
  );

  it.each(TOOLS.flatMap((tool) => FRAMES.map((frame) => ({ ...tool, ...frame }))))(
    "$tool reports '$expected' for $shape",
    async ({ tool, heading, args, earlierReplies, reply, expected }) => {
      earlierReplies.forEach((frame) => sendDaemonRequest.mockResolvedValueOnce(frame));
      sendDaemonRequest.mockResolvedValueOnce(reply);

      const [{ text }] = await handleTerminalTool(tool, args);

      expect(text).toBe(`=== ${heading} ===\n\nFailed\n\n${expected}`);
    }
  );
});

describe("removed state.json tools (#13821)", () => {
  it.each(["set_primary_terminal", "get_selected_terminal"])(
    "no longer advertises or handles %s",
    async (name) => {
      await expect(handleTerminalTool(name, { terminal_id: "terminal-1" })).rejects.toThrow(
        `Unknown terminal tool: ${name}`
      );
      expect(sendDaemonRequest).not.toHaveBeenCalled();
    }
  );
});
