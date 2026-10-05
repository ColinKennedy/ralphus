// Prints, as JSON on stdout, the TOML the New Squad modal's Simple tab builds
// for a spread of form states -- by running the real shipped
// `ntSimpleBuildToml` out of the board chunks, no browser and no model call.
//
// The Rust test `daemon/tests/simple_tab_submit.rs` runs this and pushes every
// emitted document through the daemon's validator and its real
// `POST /api/squads` path, so a generator/validator schema drift fails CI
// instead of surfacing as a red error in the board's confirm step.

import { boardScript } from "./board-source.mjs";

const script = boardScript();
const REGION_BEGIN = "// RALPHUS-SIMPLE-TAB:BEGIN";
const REGION_END = "// RALPHUS-SIMPLE-TAB:END";
const BUILD_BEGIN = "function ntSelectedTemplate()";
const BUILD_END = "// ---------- generic editable-list-widget primitive";

const slice = (begin, end) => {
  const from = script.indexOf(begin);
  const to = script.indexOf(end, from);
  if (from === -1 || to === -1) {
    throw new Error(`emit-simple-task-toml: could not locate ${begin} .. ${end} in the board chunks`);
  }
  return script.slice(from, to);
};

const source = `
  let ntSimple;
  const ntTemplates = [];
  ${slice(REGION_BEGIN, REGION_END)}
  ${slice(BUILD_BEGIN, BUILD_END)}
  return {
    build(state) { ntSimple = state; return ntSimpleBuildToml().toml; },
  };
`;
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point.
const { build } = new Function(source)();

const base = {
  templateName: "",
  prompt: "fix the flaky retry loop",
  label: "",
  fieldValues: {},
  agent: "claude-code",
  model: "",
  project: "demo",
  upstreamBranch: "",
  proofs: false,
  reviewMode: "none",
  generateManualChecks: false,
  skipAutoBuild: true,
  generateAutoBuild: false,
  proofItems: [],
  checkItems: [],
  buildItems: [],
  generating: false,
  confirmStep: false,
};
const proofs = [
  { label: "build", value: "cargo build --release" },
  { label: "", value: "cargo test -p demo" },
  { label: "lint", value: "cargo clippy -- -D warnings" },
];

const cases = {
  "no proofs": {},
  "three generated proofs": { proofs: true, proofItems: proofs },
  "proofs on a non-system-prompt agent": { agent: "ollama", model: "qwen3", proofItems: proofs },
  "proofs with auto review": { reviewMode: "auto", proofItems: proofs },
  "proofs with explicit review plus build steps and manual checks": {
    reviewMode: "explicit",
    proofItems: proofs,
    buildItems: [{ label: "", value: "cargo build" }],
    checkItems: [{ label: "Smoke", value: "Open the board and click around." }],
  },
  "ticket label": { label: "RAL-123", proofItems: proofs.slice(0, 1) },
  "typed upstream branch": { upstreamBranch: "staging", proofItems: proofs.slice(0, 1) },
};

const out = Object.entries(cases).map(([name, overrides]) => ({
  name,
  toml: build({ ...base, ...overrides }),
}));
process.stdout.write(JSON.stringify(out));
