// Gate-owned ESLint flat config (authoritative; --config + --no-config-lookup so
// the repo's eslint config is ignored). Resolves typescript-eslint from the
// toolchain installed into this dir's node_modules at image build (see
// Dockerfile), so the config import has a node_modules to find.
//
// Three layers, mirroring the residuum philosophy and reaching Rust/Python-grade
// strictness for TypeScript:
//   1. SECURITY/DISCIPLINE core — pure ESLint rules with no typescript-eslint
//      equivalent, applied to *every* JS/TS file (no eval-class footguns, no
//      debug leftovers, strict equality, no var, no param reassignment).
//   2. JS CORRECTNESS — likely-bug rules applied to JS only; TypeScript gets the
//      type-aware equivalents from layer 3 instead (applying the base rules to TS
//      double-reports or false-positives without type info).
//   3. TYPE-AWARE STRICTNESS for TS — strictTypeChecked + stylisticTypeChecked,
//      the JS analog of clippy pedantic (no-floating-promises, no-misused-promises,
//      no-unsafe-*, …). Needs a TypeScript program: the harness passes the wrapped
//      repo tsconfig via GATE_TSCONFIG (gate owns the rules; the repo's tsconfig
//      provides type/module resolution). A node project containing TS must declare
//      its tsconfig — the harness fails closed otherwise — so GATE_TSCONFIG is
//      always present when TS files exist. `no-console` is intentionally left off.
import path from "node:path";
import tseslint from "typescript-eslint";

// Layer 1 — safe on every file (no typescript-eslint counterpart to conflict).
const securityDisciplineRules = {
  "no-eval": "error",
  "no-new-func": "error",
  "no-script-url": "error",
  "no-debugger": "error",
  "no-alert": "error",
  eqeqeq: ["error", "always"],
  "no-var": "error",
  "prefer-const": "error",
  "no-param-reassign": "error",
  "no-template-curly-in-string": "error",
  "no-implicit-globals": "error",
};

// Layer 2 — JS-only; TypeScript files get type-aware equivalents from layer 3.
const jsCorrectnessRules = {
  "no-undef": "error",
  "no-unused-vars": "error",
  "no-unreachable": "error",
  "no-constant-condition": "error",
  "no-dupe-keys": "error",
  "no-dupe-args": "error",
  "no-duplicate-case": "error",
  "no-fallthrough": "error",
  "no-cond-assign": ["error", "always"],
  "no-self-compare": "error",
  "use-isnan": "error",
  "valid-typeof": "error",
  "no-implied-eval": "error",
  "no-empty": ["error", { allowEmptyCatch: false }],
  "no-unused-expressions": "error",
  "no-throw-literal": "error",
  "require-await": "error",
  "no-shadow": "error",
};

// Layer 3 is only wired when a TypeScript program is available. The harness
// guarantees GATE_TSCONFIG for any node project containing TS, so its absence
// means a JS-only project — no TS block needed (and no TS files to lint).
const tsconfigPath = process.env.GATE_TSCONFIG;
const tsBlocks = tsconfigPath
  ? [
      {
        files: ["**/*.{ts,tsx,mts,cts}"],
        extends: [
          ...tseslint.configs.strictTypeChecked,
          ...tseslint.configs.stylisticTypeChecked,
        ],
        languageOptions: {
          parserOptions: {
            project: [tsconfigPath],
            tsconfigRootDir: path.dirname(tsconfigPath),
          },
        },
        rules: { ...securityDisciplineRules },
      },
    ]
  : [];

export default tseslint.config(
  {
    files: ["**/*.{js,mjs,cjs,jsx}"],
    languageOptions: { ecmaVersion: "latest", sourceType: "module" },
    rules: { ...jsCorrectnessRules, ...securityDisciplineRules },
  },
  ...tsBlocks,
);
