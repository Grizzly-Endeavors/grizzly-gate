// Gate-owned ESLint flat config (authoritative; --config + --no-config-lookup so
// the repo's eslint config is ignored). Deliberately plugin-free so it resolves
// with only the image's global eslint — no @eslint/js or typescript-eslint import
// to chase down a node_modules. TypeScript files are type-checked by tsc, not
// parsed here.
//
// This is the strong *core* baseline mirroring the residuum philosophy: no debug
// leftovers, no eval-class footguns, no silent-failure patterns, strict equality,
// no var. Deeper type-aware strictness (the JS analog of clippy pedantic) needs
// typescript-eslint and is deferred — see manifest.toml. `no-console` is left off
// on purpose, matching the decision to drop daemon-context lints elsewhere.
export default [
  {
    files: ["**/*.js", "**/*.mjs", "**/*.cjs"],
    languageOptions: {
      ecmaVersion: "latest",
      sourceType: "module",
    },
    rules: {
      // Correctness / likely bugs
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
      "no-template-curly-in-string": "error",
      "use-isnan": "error",
      "valid-typeof": "error",
      // Debug / incomplete code (JS analog of dbg_macro / todo)
      "no-debugger": "error",
      "no-alert": "error",
      // Security footguns (analog of unsafe_code)
      "no-eval": "error",
      "no-implied-eval": "error",
      "no-new-func": "error",
      "no-script-url": "error",
      // Silent-failure / sloppy patterns
      "no-empty": ["error", { allowEmptyCatch: false }],
      "no-unused-expressions": "error",
      "no-throw-literal": "error",
      "require-await": "error",
      "no-return-await": "error",
      // Discipline / consistency
      eqeqeq: ["error", "always"],
      "no-var": "error",
      "prefer-const": "error",
      "no-param-reassign": "error",
      "no-shadow": "error",
      "no-implicit-globals": "error",
    },
  },
];
