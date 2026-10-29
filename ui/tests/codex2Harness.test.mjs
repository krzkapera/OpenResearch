import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import ts from "typescript";

// Fork: `codex2` is the Codex CLI on a second account; every Codex-only UI
// behaviour (service tiers) must apply to it too.
const file = ts.createSourceFile(
  "api.ts",
  readFileSync(new URL("../src/api.ts", import.meta.url), "utf8"),
  ts.ScriptTarget.Latest,
  true,
  ts.ScriptKind.TS,
);

function extract(...names) {
  const parts = names.map((name) => {
    const node = file.statements.find((s) => ts.isFunctionDeclaration(s) && s.name?.text === name)
      ?? file.statements.find((s) => ts.isVariableStatement(s) && s.declarationList.declarations.some((d) => d.name.getText(file) === name));
    assert.ok(node, name);
    return ts.transpileModule(node.getText(file).replace(/^export /, ""), {
      compilerOptions: { target: ts.ScriptTarget.ES2022 },
    }).outputText;
  });
  const m = { service_tier_standard: () => "Standard", service_tier_default_speed: () => "Default speed" };
  return new Function("m", `${parts.join("\n")}; return { ${names.join(", ")} };`)(m);
}

const api = extract("isCodexHarness", "SERVICE_TIER_DEFAULT_ID", "serviceTiersFor", "reconcileServiceTier");
const harness = (id) => ({ id, models: [{ id: "gpt", serviceTiers: [{ id: "priority", label: "Fast" }] }] });

test("codex and codex2 are both Codex-family harnesses", () => {
  assert.equal(api.isCodexHarness("codex"), true);
  assert.equal(api.isCodexHarness("codex2"), true);
  assert.equal(api.isCodexHarness("opencode"), false);
  assert.equal(api.isCodexHarness(undefined), false);
});

test("codex2 offers the same service tiers as codex", () => {
  assert.deepEqual(
    api.serviceTiersFor(harness("codex2"), "gpt").map((tier) => tier.id),
    api.serviceTiersFor(harness("codex"), "gpt").map((tier) => tier.id),
  );
  assert.deepEqual(api.serviceTiersFor(harness("codex2"), "gpt").map((tier) => tier.id), ["default", "priority"]);
  assert.deepEqual(api.serviceTiersFor(harness("opencode"), "gpt"), []);
  assert.equal(api.reconcileServiceTier(harness("codex2"), "gpt", "priority"), "priority");
  assert.equal(api.reconcileServiceTier(harness("opencode"), "gpt", "priority"), null);
});
