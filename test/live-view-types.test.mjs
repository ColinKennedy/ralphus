import { test } from "node:test";
import assert from "node:assert/strict";
import { liveViewTypes } from "./board-live-view-types.mjs";

const { parseLiveViewType, filterLiveViewTypes } = liveViewTypes;

test("exclude-only filters matching tagged lines and keeps untagged output", () => {
  const text = "agent output\n[usage] tokens\n[tool.Bash] command\n  stderr continuation";
  assert.equal(filterLiveViewTypes(text, "-usage"), "agent output\n[tool.Bash] command\n  stderr continuation");
});

test("includes select first and exclusions win", () => {
  const text = "untagged\n[tool.Bash] shell\n[tool.Read] file\n[usage] tokens";
  assert.equal(filterLiveViewTypes(text, "tool -tool.Bash"), "[tool.Read] file");
});

test("the shared parser recognizes indented tags and the filter ignores a bare minus", () => {
  assert.equal(parseLiveViewType("  [usage] tokens"), "usage");
  assert.equal(filterLiveViewTypes("plain\n[usage] tokens", "-"), "plain\n[usage] tokens");
});
