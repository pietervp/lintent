# lintent: plain-language rules, judged

lintent checks code against rules written in plain language. Tree-sitter cuts
every file into its classes, methods and functions. Each of those scopes is put
to a model, **Jev** by TypeSafe, once for every rule that applies to it. Jev is
served through OpenRouter's
[System One API](https://openrouter.ai/docs/api/api-reference/systemone/submit-a-system-one-request)
(`POST /api/v1/systemone`, model `typesafe/jev-1.13`). The model answers
`pass`, `fail` or `skip` with a confidence. A confident `fail` is a finding,
printed with the rule's _why_ and its _fix_.

```bash
lintent check --changed        # judge what this branch touched
lintent eval                   # prove every rule against its fixtures
lintent scopes --rule <id>     # what a rule would look at (no API call)
```

To write or review a rule, use the [lintent skill](skills/lintent/SKILL.md).
It walks an agent through finding a rule's reach, wording the rule, proving it,
and triaging the first run.

### Installing this skill

The skill is a plain directory. Copy `skills/lintent/` into the project that
uses lintent, at `.claude/skills/lintent/` (Claude Code) or
`.agents/skills/lintent/` (Codex and other agents that read that store):

```bash
mkdir -p /path/to/your-repo/.claude/skills
cp -R skills/lintent /path/to/your-repo/.claude/skills/lintent
```

With the npm package installed, the skill is at
`node_modules/@lintent/cli/skills/lintent`.

## Why: two tiers

Most repositories already have a deterministic guardrail tier: ESLint, oxlint,
clippy, ruff, custom AST checks. Those rules query syntax, cost nothing and are
never wrong. So **anything you can state precisely over syntax belongs
there**: "never import X from Y", "no `console.*` in server code", "every
handler is wrapped in `withAuth`".

Some invariants cannot be stated over syntax. "HTTP handlers stay thin", "this
error message carries no upstream content" and "this function does one thing"
are judgements a reviewer makes by reading. Usually they live only in review
comments, so they are enforced only when someone remembers them. lintent is the
tier for those judgements, and it holds them to the same principles a good
deterministic linter follows:

| Principle | lintent |
|---|---|
| Every finding cites its why | the rule's `why`, printed with each finding |
| The hint is the fix | the rule's `fix`, printed as the hint |
| No unproven rules | `lintent eval`: a rule with no fail fixture fails eval |
| Exemptions are named, with a reason | `// lintent-keep <id> -- why` |

The model returns a verdict and a confidence, never an explanation. What a
finding says comes from the rule, which a human wrote and reviewed.

lintent calls a paid API and needs a key, so it does not belong in a free,
always-on gate. Run it by hand on a branch, before review, or in CI on trusted
branches with the key as a secret. Every run is capped by a
[budget](#budget) that is checked before anything is sent.

## Install

In a JavaScript or TypeScript project, add lintent as a dev dependency, so
the version is pinned in your lockfile like any other linter:

```bash
npm install --save-dev @lintent/cli     # or pnpm add -D, yarn add -D, bun add -d
npx lintent --help
```

The package carries prebuilt binaries for macOS (arm64, x64), Linux (arm64,
x64, glibc) and Windows (x64), and installs only the one for your machine.
`LINTENT_BINARY=<path>` makes it run another build instead.

Anywhere else, download a binary from
[GitHub Releases](https://github.com/pietervp/lintent/releases), or build it
with a stable Rust toolchain:

```bash
cargo install --git https://github.com/pietervp/lintent

# Or from a clone:
git clone https://github.com/pietervp/lintent && cd lintent
cargo install --path .
```

### Releasing

Bump `version` in `Cargo.toml`, commit, then push a matching tag:

```bash
git tag v0.2.0 && git push origin v0.2.0
```

[`release.yml`](.github/workflows/release.yml) builds the five binaries,
attaches them to a GitHub release, and publishes `@lintent/cli` plus one
`@lintent/cli-<platform>` package per binary
([`npm/build.mjs`](npm/build.mjs) assembles them). Running the workflow by
hand (Actions → release → Run workflow) builds and packs everything and
publishes nothing.

npm publishing uses
[trusted publishing](https://docs.npmjs.com/trusted-publishers): npm accepts
the workflow's GitHub OIDC token, so no npm token is stored anywhere. npm
configures a trusted publisher per package, and only for a package that exists,
so the setup is once per package: run [`npm/bootstrap.sh`](npm/bootstrap.sh)
logged in as an `@lintent` owner to publish `0.0.0` placeholders, then on each
package's npmjs.com settings page add a GitHub Actions trusted publisher
(`pietervp` / `lintent` / `release.yml`) and disallow token publishing.

The 18 built-in grammars are compiled into the binary. A C compiler (Xcode CLT
on macOS, `build-essential` on Debian/Ubuntu) is needed only for
[runtime grammars](#languages) added in `lintent.toml`, which lintent compiles
on first use.

### API key

`check` and `eval` need an API key. The default provider is OpenRouter, which
reads `OPENROUTER_API_KEY`; with `provider = "typesafe"` lintent reads
`TYPESAFE_API_KEY` instead. Put the key in the environment, or in a gitignored
`.env.local` or `.env`:

```bash
export OPENROUTER_API_KEY=…            # or a line OPENROUTER_API_KEY=… in .env.local
```

lintent looks for the key in this order, and the first non-empty value wins:

1. the real process environment (so CI can override a developer's file);
2. `.env.local`, then `.env`, in the working directory;
3. `.env.local`, then `.env`, in the project root (the directory holding
   `lintent.toml`).

Make sure your `.gitignore` covers the dotenv file you use. The key is never
logged; debug output of the environment lists key names only.

These commands need no key: `init`, `rule new`, `rules`, `scopes`, `languages`,
and any `--dry-run`.

## Layout in the linted repo

```
lintent.toml                       # project config, at the repo root
.lintent/rules/<rule-id>.toml      # one file per rule
.lintent/fixtures/<rule-id>/pass/  # code the rule must pass
.lintent/fixtures/<rule-id>/fail/  # code the rule must fail
.lintent/cache.json                # verdict cache, gitignored
```

Commit the config, the rules and the fixtures. The fixtures are deliberately
broken code: lintent's default `exclude` already hides `.lintent/fixtures/**`
from `check`, but add the directory to your other tools' ignore lists too
(Prettier, ESLint, oxlint, ruff, your type checker and test runner), so they
neither lint nor run it.

## `lintent.toml`

`lintent init` writes a commented copy of this file. Unknown keys are rejected,
so a typo fails loudly instead of doing nothing.

```toml
provider = "openrouter"
model = "typesafe/jev-1.13"
base_url = "https://openrouter.ai/api/v1"
min_confidence = 0.8
concurrency = 4
isolate_rules = false
exclude = ["**/node_modules/**", "**/dist/**", "**/target/**", "**/*.gen.ts", "**/*.min.js", "**/*.min.css", ".lintent/fixtures/**"]

[budget]
max_cost_usd = 0.0001              # USD per run (0.01 US cent); `--max-cost` overrides
input_price_per_million = 0.042    # USD per million input tokens, for typesafe/jev-1.13
output_price_per_million = 0.0     # USD per million output tokens
request_overhead_tokens = 300      # provider prompt added to every request
question_overhead_tokens = 80      # added per question
```

| Key | Meaning |
|---|---|
| `provider` | `openrouter` (default; key in `OPENROUTER_API_KEY`) or `typesafe` (key in `TYPESAFE_API_KEY`). Both speak System One. The provider decides the default model, the base URL and which key is read |
| `model` | Default `typesafe/jev-1.13` (openrouter) or `jev-latest` (typesafe). `LINTENT_MODEL` overrides it |
| `base_url` | Default `https://openrouter.ai/api/v1` or `https://api.typesafe.ai/v1`. Requests go to `<base_url>/systemone`. In this file, `base_url` must be https on the provider's own host; only the path may differ. Any other endpoint comes from `LINTENT_BASE_URL` in the real environment (see [Security](#security)) |
| `min_confidence` | A `fail` below this confidence is reported as **uncertain** and never blocks. Default `0.8` |
| `concurrency` | Number of parallel requests. Default `4` |
| `isolate_rules` | `true` sends one request per (scope, rule) instead of one per scope; see [How questions are asked](#how-questions-are-asked) |
| `exclude` | Globs that no rule ever sees, on top of `.gitignore`. Setting it replaces the default list above, so keep `.lintent/fixtures/**` in it |
| `[budget]` | `max_cost_usd`, the per-million prices, and the two overhead token counts the estimate uses. All must be non-negative. Update the prices when you change `model`. See [Budget](#budget) |
| `[languages.<name>]` | Adds a grammar or adjusts a built-in; see [Languages](#languages) |

Hidden files and directories are linted too (`.github/scripts`, dotfile
configs); `.gitignore` and `exclude` still apply, and `.git/` is never entered.

## Rule files: `.lintent/rules/<id>.toml`

`lintent rule new <id>` writes a commented template. Unknown keys are rejected,
so a misspelt `exceptons` cannot silently weaken a rule.

```toml
id = "handlers-stay-thin"
description = """
An HTTP route handler only translates between HTTP and the service layer.
FAILS: the handler queries a database, builds SQL, computes prices, discounts or
other business values, or loops over records to transform them.
PASSES: it parses and validates the request, calls one service function, and
maps the result or error to a response.
"""
why = "docs/architecture.md#layers: business logic lives in src/services so it can be tested without HTTP"
fix = "Move the logic into a function in src/services/ and call it from the handler."
severity = "warning"
scopes = ["function"]
languages = ["typescript"]
include = ["src/routes/**"]
exclude = ["**/*.test.ts"]
exceptions = ["Health-check handlers that return a constant status without calling any service"]
```

| Key | Required | Meaning |
|---|---|---|
| `id` | yes | Kebab-case (`a-z`, `0-9`, single dashes); must equal the file stem |
| `description` | yes | The rule, in plain language. Say what FAILS and what PASSES |
| `scopes` | yes | One or more of `class`, `method`, `function` |
| `include` | yes | Globs relative to the project root. `*` stays within one directory and `**` crosses directories. There is no default, because a rule with no reach is a mistake |
| `why` | — | The doc, ADR, incident or review thread behind the rule, printed with every finding |
| `fix` | — | What to do instead, printed as the hint |
| `severity` | — | `error` (default; blocks with exit 1) or `warning` |
| `languages` | — | Names from `lintent languages`. Default: every language |
| `exclude` | — | Globs carved out of `include` |
| `exceptions` | — | Named cases the model must treat as passing. Each deserves a pass fixture |
| `min_confidence` | — | Per-rule override of the project value |
| `allow_skip` | — | Default `true`: the model may answer `skip` when the rule's subject is absent from the scope |

The model sees a scope's source and, for a method, its enclosing class as
context. It does **not** see the file's imports or other files, so phrase rules
over what a body shows.

## How questions are asked

**Each rule is its own question.** When a scope matches three rules, the request
carries three separately keyed `choice` questions, one per rule id. Rules are
never merged into one prompt, so one verdict always answers one rule. A
question's instructions are the rule's description plus its explicit
exceptions. The answer criteria are fixed:

- **pass:** the code complies, or an exception applies.
- **fail:** the code violates the rule, and no exception applies.
- **skip:** the rule's subject is not in this scope.

The confidence is the answer's `confidence`, or else `probabilities[choice]`.

With `isolate_rules = true`, lintent sends one HTTP request per (scope, rule).
That means more requests, each paying the provider's per-request overhead. Use
it if you suspect rules that share a request are influencing each other's
verdicts.

Questions per run = scopes in reach × rules matching them, minus cached and
kept ones. `lintent scopes --rule <id> | wc -l` is one rule's share; the
summary goes to stderr, so the count is exact. `check --dry-run` prints every
request body on stdout. On stderr it prints the count
(`dry run: N request(s) for N question(s) on N scope(s) (K cached); nothing sent`)
and the cost estimate. A real run's summary also reports the actual tokens and
cost.

## Budget

Before sending anything, lintent estimates what the run will cost. The estimate
is deliberately pessimistic, because an estimate that errs low would let an
over-budget run through. Per request it assumes:

- input tokens = `request_overhead_tokens` + `question_overhead_tokens` ×
  questions + request-body bytes / 2.5. The provider wraps every request in
  its own prompt, so a plain bytes / 3 guess comes in well under the bill;
- 40 output tokens per question;
- both priced with `[budget]`'s per-million prices.

Only questions that would actually be sent are counted; cached and kept
questions cost $0. When the estimate is over `max_cost_usd` (default $0.0001,
0.01 US cent), **nothing is sent** and lintent exits **3**. It then prints
guidance addressed to the agent or human running it:

- the costliest rules, files and directories, with their shares;
- any single scope that costs more than the whole budget;
- the ways to narrow the run: tighter `include`/`exclude`, a smaller `scopes`
  kind or fewer `languages`, `--changed`, explicit PATHS, one `--rule` at a
  time.

`check --dry-run` and `eval --dry-run` apply the same check and exit with the
same codes. `--max-cost <USD>` overrides the budget for one run.

The default is small on purpose. In a live run, one small scope judged against
one rule cost roughly 900–1,200 input tokens, about $0.00004 at $0.042 per
million. So a default run covers two or three uncached small scopes. A rule's
full reach is covered over many runs (`--changed`, explicit PATHS), and the
cache keeps every answer. Raising the budget is a decision about spend, made by
a person, not a way to get past exit 3.

## Languages

`lintent languages` lists the 18 built-ins and how each one finds scopes:
typescript, tsx, javascript, rust, python, go, java, c, cpp, csharp, ruby, php,
swift, scala, lua, elixir, kotlin and bash. Note that `.tsx` files are `tsx`,
not `typescript`.

- **`tags`:** the grammar's tags query (`queries/tags.scm`) marks
  `@definition.class`, `@definition.method` and `@definition.function`
  captures. A function nested inside a class is reported as a method.
- **`heuristic`:** there is no tags query, so scopes come from named node types.
  `*class|struct|interface|trait|impl|enum|object|module*` declarations,
  definitions, items and specifiers are classes.
  `*function|method|func|fun|procedure|sub*` declarations, definitions and
  items are functions, or methods when they sit inside a class. Kotlin and
  bash use the heuristic.

Any other tree-sitter grammar can be added in `lintent.toml`:

```toml
[languages.haskell]
extensions = ["hs"]
repo = "https://github.com/tree-sitter/tree-sitter-haskell"   # or grammar = "<dir with src/parser.c | .so | .dylib>"
rev = "<full 40-character commit SHA>"                        # required with repo
tags_query = ".lintent/queries/haskell-tags.scm"              # optional
```

A runtime grammar is native code, so it is only compiled and loaded with
`--trust-grammars` or `LINTENT_TRUST_GRAMMARS=1`; see [Security](#security).

| Key | Meaning |
|---|---|
| `extensions` | File extensions for this language. Required with `grammar` or `repo`; on a built-in, adds extensions |
| `grammar` | A grammar directory containing `src/parser.c`, or a compiled `.so` / `.dylib` |
| `repo`, `rev`, `subdir` | A git URL to clone the grammar from, the full 40-character commit SHA to check out (required; never a movable tag), and a subdirectory for repos that hold several grammars. Use either `grammar` or `repo`, not both |
| `tags_query` | Path from the project root to a tags query. If omitted, lintent uses the grammar's own `queries/tags.scm`, and falls back to the heuristic when there is none |
| `symbol` | The exported language function. Default: `tree_sitter_<name>` |

Language names are lower-case kebab-case. A grammar directory or repo is
compiled once with the system C compiler. The result is cached under the user
cache directory: `~/Library/Caches/lintent` on macOS, `$XDG_CACHE_HOME/lintent`
(usually `~/.cache/lintent`) elsewhere; `LINTENT_CACHE_DIR` overrides it. A
table named after a built-in, with no `grammar` or `repo`, only adjusts the
built-in: extra extensions, or a replacement tags query. A tags query is data,
not code, so it needs no trust. `grammar`, `subdir` and `tags_query` must be
relative paths without `..`. Check the result with `lintent scopes <file>`.

## Keep marks

```ts
// lintent-keep handlers-stay-thin -- the webhook must ack within 200 ms, so the signature check stays inline
export async function paymentWebhook(req: Request): Promise<Response> { … }
```

A keep mark silences the named rules (comma-separated:
`// lintent-keep rule-a, rule-b -- reason`) for exactly one scope. That is the
scope starting on the mark's own line, or else the first scope below it with
only blank and comment lines in between. Decorators count as part of the
scope. A mark on a class does not cover its methods, and it never leaks onto
the next function. Marks are read from the parse tree's comment nodes, so any
comment syntax the grammar has works (`//`, `#`, `--`, `/* */`). A kept scope
is not sent for those rules at all.

A mark with no reason after `--`, or one naming a rule that does not exist, is
itself a finding under `lintent/keep` and blocks with exit 1. A mark that
suppresses nothing is warned about as stale. An exemption nobody can audit, or
one that silences nothing while looking deliberate, is worse than none.

The `lintent-keep` spelling is deliberately distinct from other tools'
suppressions (`eslint-disable`, `#[allow(...)]`, `# noqa`). A suppression
written for a deterministic check can never silence a judged rule, and the
reverse.

## Cache

Every verdict is cached in `.lintent/cache.json`. The cache key is a hash of
the model, the **whole rule file** and the scope (its text and path). Unchanged
code under an unchanged rule is never asked twice. Any edit to a rule, even to
its `why`, re-judges everything the rule reaches. Several runs can share the
cache: saving merges under a lock instead of overwriting, and entries unused
for 30 days are pruned. `--no-cache` neither reads nor writes the cache.
`--refresh` (on `check`) asks again and overwrites the cached verdicts.

## Eval: proving a rule

```bash
lintent eval                                         # every rule
lintent eval --rule <id>                             # one rule; repeatable
lintent eval --rule <id> --dry-run | grep '^# POST'  # which fixture scopes get judged
```

Every file in `.lintent/fixtures/<id>/fail/` must produce at least one
confident fail. Every file in `.lintent/fixtures/<id>/pass/` must produce none.
Fixtures ignore the rule's `include` globs, but its `scopes` and `languages`
still apply. A fixture with no scope the rule applies to is reported as
**vacuous** and fails eval, because it proves nothing. A fixture file whose
extension maps to no language is not picked up at all, so check the
`--dry-run` list.

Eval exits 1 if any fixture is misjudged or vacuous, **or if a rule has no fail
fixture** (reported as **unproven**). An unproven rule is exactly what this
tool must not ship: nothing shows it can ever fire. Fixtures are best minimised
from real code: the violation that prompted the rule, and its nearest
legitimate neighbour.

## CLI

| Command | Does |
|---|---|
| `lintent init` | Writes `lintent.toml` and `.lintent/rules/`, and adds `.lintent/cache.json` to an existing `.gitignore`. Idempotent; never overwrites |
| `lintent rule new <id> [--scopes class,method,function] [--include <glob>]…` | Scaffolds `.lintent/rules/<id>.toml` and empty fixture directories. `--scopes` defaults to `function,method`; the template's severity is `error` |
| `lintent rules [--json]` | Lists the rules: id, severity, scopes, include |
| `lintent scopes [PATHS…] [--rule <id>] [--verbose]` | Prints the scopes `check` would look at (`path:line  kind  name`) and a count. With `--rule`, prints only the scopes that rule reaches; `--verbose` adds files no language handles. No API call |
| `lintent languages [--json]` | Lists the languages and how each one finds its scopes |
| `lintent check [PATHS…] [--rule <id>]… [--changed [--base <ref>]] [--json] [--dry-run] [--no-cache] [--refresh] [--max-cost <USD>]` | Judges the scopes. `PATHS` default to the project root (the directory holding `lintent.toml`) |
| `lintent eval [--rule <id>]… [--dry-run] [--no-cache] [--max-cost <USD>]` | Runs the fixtures and prints a table |

Global options:

- `--config <path>` points at a different `lintent.toml`. Otherwise lintent
  walks up from the current directory to find one.
- `--trust-grammars` allows runtime grammars to load; see
  [Security](#security).

Environment variables:

| Variable | Effect |
|---|---|
| `OPENROUTER_API_KEY` / `TYPESAFE_API_KEY` | The provider's key; see [API key](#api-key) |
| `LINTENT_MODEL` | Overrides `model` |
| `LINTENT_BASE_URL` | Overrides `base_url`. Read from the real environment only, never a dotenv file |
| `LINTENT_TRUST_GRAMMARS=1` | Same as `--trust-grammars`. Read from the real environment only |
| `LINTENT_CACHE_DIR` | Where compiled runtime grammars are cached |

Files that are not UTF-8 text are skipped with a warning. Examples are binary
files, or an MPEG-TS stream that happens to end in `.ts`.

`check --changed` judges only files changed since the merge-base with `--base`
(default `origin/main`, falling back to `main`), plus uncommitted and untracked
files.

`--json` prints `{findings, errors, stats}`. Each finding carries `path`,
`line`, `end_line`, `kind`, `name`, `rule`, `severity`, `status`, `confidence`,
`p_fail`, `why` and `fix`, plus `message` for keep-mark problems. The stats
include questions, cache hits, requests, tokens and cost.

When several exit conditions apply, the highest code wins. `--dry-run` exits
with the same codes.

| Exit | Meaning |
|---|---|
| 0 | Clean. Warnings and uncertain fails do not block |
| 1 | A confident fail of an `error` rule or an invalid keep mark; for `eval`, a misjudged or vacuous fixture or an unproven rule |
| 2 | Usage, config or API error. The run is incomplete |
| 3 | Over budget. Nothing was sent; the printed guidance says how to narrow the run |

## Security

lintent reads configuration that arrives in pull requests, so it treats that
configuration as untrusted:

- **The API key only goes to the provider's own host.** In `lintent.toml`,
  `base_url` must be https on the provider's host (`openrouter.ai` or
  `api.typesafe.ai`). Only the path may differ. Any other endpoint requires
  `LINTENT_BASE_URL` in the real process environment. A dotenv file cannot set
  it, and plain http is accepted only for loopback. A PR therefore cannot
  redirect the key by editing the config.
- **Runtime grammars are native code.** A `[languages.*]` table with `grammar`
  or `repo` is compiled with the system C compiler and loaded into the lintent
  process. lintent refuses to do that without `--trust-grammars` or
  `LINTENT_TRUST_GRAMMARS=1` in the real environment.
  - `repo` must pin `rev` to a full 40-character commit SHA.
  - `grammar`, `subdir` and `tags_query` must be relative paths without `..`.
  - Review the grammar before trusting it, and **never set the trust flag in CI
    for changes from untrusted branches or PRs**.
  - The 18 built-in grammars are compiled into the binary and need no trust.
- **Secrets stay out of output.** Key values are never logged, and debug output
  of the environment lists key names only.
- **Spend is bounded.** The [budget](#budget) is checked before any request is
  sent.

## Credit

Jev is TypeSafe's System One decision model, served here through
[OpenRouter's System One API](https://openrouter.ai/docs/api/api-reference/systemone/submit-a-system-one-request).

The Scala tags query in [queries/scala-tags.scm](queries/scala-tags.scm) is
vendored from tree-sitter-scala under the MIT License; see the file's header.
