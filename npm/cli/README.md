# @lintent/cli

[lintent](https://github.com/pietervp/lintent) checks code against lint rules
written in plain language, judged by a model and scoped with tree-sitter.

```bash
npm install --save-dev @lintent/cli
```

Run it through `package.json` scripts (or `pnpm exec lintent`, `yarn lintent`):

```json
{
  "scripts": {
    "lintent": "lintent",
    "lint:intent": "lintent check --changed"
  }
}
```

```bash
npm run lintent -- init
npm run lint:intent
```

Avoid `npx lintent`: the npm package named `lintent` is an unrelated tool, and
when `@lintent/cli` is not installed in the project, npx runs that one instead.

This package holds a small launcher; the binary comes from the one
`@lintent/cli-<platform>` optional dependency that matches your machine
(macOS arm64/x64, Linux arm64/x64 with glibc, Windows x64). On anything else,
build lintent with `cargo install --git https://github.com/pietervp/lintent`
and set `LINTENT_BINARY` to its path.

The agent skill for writing rules ships in this package too:

```bash
mkdir -p .claude/skills
cp -R node_modules/@lintent/cli/skills/lintent .claude/skills/lintent
```

The full reference is the [lintent README](https://github.com/pietervp/lintent#readme).
