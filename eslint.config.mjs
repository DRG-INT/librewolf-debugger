// ESLint flat config, for the WebExtension only.
//
// The extension is the one piece of this project that runs inside somebody
// else's browser session, so the rules here are about reach and correctness
// rather than style: an undefined identifier in a background script fails
// silently at run time, and `no-implied-eval` is the lint form of the rule the
// artifact audit enforces by grep.

export default [
  {
    files: ["extension/**/*.js"],
    languageOptions: {
      ecmaVersion: 2022,
      sourceType: "script",
      globals: {
        // MV2 background script, Gecko. `browser` is the promise-based
        // namespace; `chrome` is deliberately *not* declared, so using it is
        // an error rather than a silently non-awaiting call.
        browser: "readonly",
        console: "readonly",
        setTimeout: "readonly",
        clearTimeout: "readonly",
        Date: "readonly",
        Object: "readonly",
        Error: "readonly",
        String: "readonly",
        JSON: "readonly",
        performance: "readonly",
        document: "readonly",
        location: "readonly",
      },
    },
    linterOptions: {
      reportUnusedDisableDirectives: true,
    },
    rules: {
      // Correctness. Each of these is a run-time failure in a background
      // script, where there is no console anybody is watching.
      "no-undef": "error",
      "no-unused-vars": ["error", { args: "after-used" }],
      "no-dupe-keys": "error",
      "no-unreachable": "error",
      "no-constant-condition": "error",
      "require-atomic-updates": "error",
      "no-await-in-loop": "off",

      // Reach. The extension's only channel is the native messaging port.
      "no-eval": "error",
      "no-implied-eval": "error",
      "no-new-func": "error",
      "no-script-url": "error",

      // Promise discipline: an unhandled rejection in the message listener
      // loses the reply and the caller waits for its 120-second timeout.
      "no-async-promise-executor": "error",
      "no-promise-executor-return": "error",

      // Shape. The dispatch table is the security boundary, so a stray
      // fallthrough or shadowed name matters more here than elsewhere.
      "no-shadow-restricted-names": "error",
      "no-fallthrough": "error",
      eqeqeq: ["error", "smart"],
    },
  },
];
