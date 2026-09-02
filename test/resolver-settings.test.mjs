// Regression coverage for RAL-316's raw review resolver-model control.

import test from "node:test";
import assert from "node:assert/strict";
import { resolverSettings, boardSource } from "./board-resolver-settings.mjs";

const { resolverDraftFor, resolverSettingsBody } = resolverSettings;

test("loading a review starts a resolver draft with its stored agent and model", () => {
  assert.deepEqual(resolverDraftFor("g1", "codex", "gpt-5.6-luna", null), {
    gid: "g1",
    agent: "codex",
    model: "gpt-5.6-luna",
  });
});

test("editing an agent preserves the staged model", () => {
  const draft = resolverDraftFor("g1", "ollama", "qwen3:8b", {
    gid: "g1",
    agent: "ollama",
    model: "gpt-5.6-luna",
  });
  draft.agent = "codex";
  assert.deepEqual(resolverSettingsBody(draft), {
    resolver_agent: "codex",
    resolver_model: "gpt-5.6-luna",
  });
});

test("a draft for another review cannot leak into the selected review", () => {
  assert.deepEqual(
    resolverDraftFor("g2", "claude-code", "claude-opus", {
      gid: "g1",
      agent: "codex",
      model: "gpt-5.6-luna",
    }),
    { gid: "g2", agent: "claude-code", model: "claude-opus" },
  );
});

test("saving trims an unrestricted model string", () => {
  assert.deepEqual(
    resolverSettingsBody({ gid: "g1", agent: "codex", model: "  provider/new-model  " }),
    { resolver_agent: "codex", resolver_model: "provider/new-model" },
  );
});

test("saving an empty or whitespace-only model explicitly clears it", () => {
  for (const model of ["", "   ", "\t\n"]) {
    assert.equal(resolverSettingsBody({ gid: "g1", agent: "codex", model }).resolver_model, "");
  }
});

test("the model control is raw text with staged Save and Cancel actions", () => {
  assert.match(boardSource, /id="resolver-model-input"[^>]*type="text"|type="text"[^>]*id="resolver-model-input"/);
  assert.match(boardSource, /id="resolver-model-pending"/);
  assert.match(boardSource, /data-click="saveResolver"/);
  assert.match(boardSource, /data-click="cancelResolver"/);
  assert.match(boardSource, /Any model name is accepted/);
});

test("approved and deployed reviews freeze both resolver controls", () => {
  assert.match(boardSource, /const resolverFrozen = \["approved", "deployed"\]\.includes\(g\.status\)/);
  assert.match(boardSource, /resolver agent<\/span><span class="v mono"/);
  assert.match(boardSource, /resolver model<\/span><span class="v mono"/);
});

test("saving posts the combined resolver request through the restart-aware settings endpoint", () => {
  const body = boardSource.slice(boardSource.indexOf("async function saveResolver(gid)"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.match(fn, /guardianAction\(`\/api\/guardians\/\$\{gid\}\/settings`, resolverSettingsBody\(draft\)\)/);
});
