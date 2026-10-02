---
name: lintent
description: Set up lintent and write, prove and review its plain-language lint rules. A model (Jev by TypeSafe, via OpenRouter's System One API) judges each rule against every class, method and function it reaches. Use when the user says "set up lintent", "add a lint rule", "write a lint rule", "lint for X", "make sure nobody does X", "turn this review comment into a rule", asks to review, tighten or debug an existing lintent rule (false positives, a rule that never fires, cost of a run), or wants lintent to understand another language.
---

# lintent

lintent asks a model one question per rule per code scope: _does this
function / method / class comply with this rule?_ Rules live in
`.lintent/rules/<id>.toml`. The fixtures that prove them live in
`.lintent/fixtures/<id>/{pass,fail}/`. Full reference: the
[lintent README](https://github.com/pietervp/lintent#readme), or
`lintent --help` and `lintent <command> --help`.

lintent is the **judgement tier**. The deterministic tier is whatever the repo
already runs: ESLint, oxlint, clippy, ruff, custom AST checks. lintent holds
its rules to the same principles. Hold every rule to them:

- **Every finding cites its why.** A rule's `why` names the doc, ADR, incident
  or review thread it enforces. The model gives no reasons of its own; a
  finding shows only the rule's `why` and `fix`.
- **The hint is the fix, not the complaint.** `fix` says what the author does
  next.
- **No unproven rules.** Trust a rule only after `lintent eval` shows it failing
  its fail fixtures and passing its pass fixtures.
- **Exemptions are named and carry a reason.** Write
  `// lintent-keep <id> -- reason`, never a blanket skip.

**Pick the tier before writing anything.** If the invariant can be stated
precisely over syntax, it belongs in a deterministic linter. Examples: "never
import X from Y", "every handler is wrapped in `withAuth`", "no `console.*` in
server code". Those checks are free, run on every commit, and are never wrong.
lintent is for what a reviewer has to _read_ to decide: "HTTP handlers stay
thin", "this error message carries no upstream content", "this function is
named after what it does".

Run every command from the repo root (the directory holding `lintent.toml`).
Every `--dry-run`, `scopes`, `rules` and `languages` call is free: no network,
no key.

**Every run has a budget.** Before sending anything, lintent estimates the cost
of all uncached questions. If the estimate is over `[budget] max_cost_usd`
(default $0.0001, 0.01 US cent), it sends nothing and exits **3**. It then
prints guidance addressed to you: the rules, files and directories that cost
the most, the single scopes that cost more than the whole budget, and ways to
narrow the run. `--dry-run` runs the same check. When you get exit 3, narrow
the run as the guidance says. **Never raise `max_cost_usd` or pass `--max-cost`
unless the user explicitly asks for that spend.**

## 1. Setup

```bash
lintent --version || echo "not on PATH"
```

A project may pin lintent instead of putting it on PATH. Use the project's own
copy when there is one, and run every command below through it:

- `@lintent/cli` in `package.json`: `npm run lintent -- <args>` with a
  `"lintent": "lintent"` script, or `pnpm exec lintent` / `yarn lintent`.
  **Never `npx lintent`**: the npm package named `lintent` is an unrelated tool,
  and npx runs it whenever `@lintent/cli` is not installed.
- `lintent` in `dotnet-tools.json` or `.config/dotnet-tools.json`:
  `dotnet tool restore` once, then `dotnet lintent <args>`.

If lintent is missing, install it the way the project's ecosystem does,
pinned per project where it can be:

```bash
npm install --save-dev @lintent/cli                        # JavaScript / TypeScript
dotnet new tool-manifest && dotnet tool install lintent    # .NET (SDK 10+)
cargo binstall lintent                                     # Rust (or cargo install --locked lintent)
brew install pietervp/tap/lintent                          # anything else, macOS or Linux
```

A C compiler is needed only for runtime grammars (§ 4). Then:

```bash
lintent init           # lintent.toml + .lintent/rules/; never overwrites; ignores .lintent/cache.json
lintent languages      # 18 built-in grammars; anything else: § 4
```

**API key.** `check` and `eval` send requests to Jev on OpenRouter (model
`typesafe/jev-1.13`) and need `OPENROUTER_API_KEY`. With `provider = "typesafe"`
in `lintent.toml` they need `TYPESAFE_API_KEY` instead. lintent looks in this
order: the real environment, then `.env.local` and `.env` in the working
directory, then `.env.local` and `.env` in the project root. Check that the key
is present without printing it:

```bash
[ -n "$OPENROUTER_API_KEY" ] && echo "OPENROUTER_API_KEY: in env"
grep -l '^OPENROUTER_API_KEY=' .env.local .env 2>/dev/null   # file names only, never values
git check-ignore -q .env.local && echo ".env.local is gitignored"
```

If the key is missing, tell the user to export it in their shell or add it to
a gitignored `.env.local` themselves (an OpenRouter key from
openrouter.ai/settings/keys). If the dotenv file they use is not gitignored,
say so before they put a key in it. **Never** echo, write, commit or paste the
key, and never put it on a command line yourself.

The key only ever goes to the provider's own host. In `lintent.toml`,
`base_url` may only be the provider's host over https. Any other endpoint
requires `LINTENT_BASE_URL` in the real environment. A dotenv file cannot set
it, and plain http is accepted only for loopback. Do not try to point a
committed config elsewhere.

**Project-wide excludes.** Survey where the code is. Then exclude code nobody
hand-writes, and code the repo already refuses to lint:

```bash
git ls-files | awk -F/ 'NF>2{print $1"/"$2}' | sort | uniq -c | sort -rn | head -40
cat .prettierignore .eslintignore 2>/dev/null | grep -v '^#' | head -40   # the repo's own "not our source" lists
grep -n -A20 -i 'ignore' eslint.config.* .oxlintrc*.json ruff.toml pyproject.toml 2>/dev/null | head -60
lintent scopes | wc -l                     # scopes today (the summary goes to stderr)
```

Propose additions to `exclude` in `lintent.toml` and confirm them with the user.
Typical candidates:

- vendored third-party code (`vendor/**`, `third_party/**`);
- generated code (`**/*.gen.*`, `**/generated/**`, protobuf and OpenAPI
  output, migrations written by a tool);
- build output and bundles not already covered by the defaults;
- test fixtures and snapshots that are deliberately wrong.

Setting `exclude` replaces the default list, so keep the defaults
(`.lintent/fixtures/**` in particular) when you add to it. Also add
`.lintent/fixtures/` to the repo's other linters' and formatters' ignore lists:
fixtures are deliberately broken code. Re-run `scopes | wc -l` afterwards. The
count should drop by exactly what you meant to drop.

## 2. Add a rule

### 2a. Interview (short)

Ask one focused question at a time, with AskUserQuestion when it is available:

1. **What fails?** Get a concrete example, ideally a real file or PR comment.
2. **What passes?** Get the nearest legitimate code that must _not_ be flagged.
3. **Exceptions?** Get the named cases that look like violations but are fine.
4. **Why?** Get the doc, ADR, incident or review thread. If there is no why,
   the decision is not written down. Say so: the rule may need a short
   architecture note or ADR first.
5. **Fix hint.** Get what the author should do instead, in one sentence.
6. **Severity.** The default is `error`. Start a new rule at `warning`. Promote
   it once eval and a real `check` are clean.

Then decide the tier (see above). If the rule can be stated precisely over
syntax, stop and write it for the repo's deterministic linter instead (an
ESLint/oxlint rule, a clippy lint, a ruff rule, a custom AST check).

### 2b. Find the reach before writing the rule

A rule's `include` is required, and it is where most rules go wrong. Too narrow
and the rule never fires. Too wide and every run pays for thousands of
irrelevant questions. Use the rule's subject to find where the concern lives.

**1. Where do the rule's constructs, imports and names occur?** Grep for the
calls, imports, decorators or naming patterns the rule is about, and count the
hits per directory:

```bash
# e.g. a "handlers stay thin" rule: where are routes defined?
git grep -l -E 'router\.(get|post|put|delete)|app\.(get|post)|@(Get|Post)\(' \
  | xargs -n1 dirname | sort | uniq -c | sort -rn | head -30

# where does the thing the rule forbids already appear? (DB access, here)
git grep -l -E '\.query\(|prisma\.|knex\(|SELECT ' | xargs -n1 dirname | sort | uniq -c | sort -rn | head -20
```

**2. Where does the architecture say the concern lives?** Read the repo's own
guidance, if present: `AGENTS.md`, `CLAUDE.md`, `CONTRIBUTING.md`,
`ARCHITECTURE.md`, `docs/`, and any ADR directory (`docs/adr/`, `adr/`,
`docs/decisions/`). They say which directory owns a concern, for example
"handlers live in `src/routes`, business logic in `src/services`".

```bash
ls AGENTS.md CLAUDE.md CONTRIBUTING.md ARCHITECTURE.md docs/adr docs/decisions adr 2>/dev/null
git grep -n -i -E '<subject words>' -- '*.md' | head -20
```

**3. Count candidate directories,** with file counts, so the include is
neither a guess nor the whole repo:

```bash
git ls-files 'src/**' | awk -F/ '{print $1"/"$2"/"$3}' | sort | uniq -c | sort -rn | head -30
```

Then propose:

- `include` / `exclude` globs, relative to the project root. `*` stays within
  one directory and `**` crosses directories. Example:
  `include = ["src/routes/**"]`, `exclude = ["**/*.test.ts"]`.
- `scopes`: any of `function`, `method`, `class`. Choose the smallest kind that
  holds the subject. A "does too much" rule is about functions or methods; a
  "god object" rule is about classes.
- `languages`: names from `lintent languages`. Note that `.tsx` files are
  `tsx`, not `typescript`.

Scaffold the rule, then preview its reach. This makes no API call:

```bash
lintent rule new <id> --scopes function --include 'src/routes/**'
lintent scopes --rule <id> | head -40
lintent scopes --rule <id> >/dev/null  # stderr: "N scope(s) in M file(s) matched by <id>"
```

The reach is right when the listed units are the ones the rule is about. It
must also be affordable. A small scope costs roughly 900–1,200 input tokens,
about $0.00004, so the default budget covers two or three uncached scopes per
run. A rule that reaches 250 scopes can still be correct, but it gets checked
incrementally: with `--changed`, with explicit PATHS, and from the cache. It
never gets checked in one sweep. Watch for three failure modes:

- **0 scopes:** a glob, `languages` or `scopes` mismatch.
- **Most of the repo:** an accidental `**`.
- **The construct is missing from the list:** check that the code you care
  about is actually a scope (arrow functions assigned to a variable, handlers
  passed inline to a router call). If it is not, see § 4.

Iterate on the globs until the reach is right.

### 2c. Write the rule

`rule new` writes a commented template. Fill it in. The `id` must equal the
file stem, in kebab-case. Unknown keys are rejected.

```toml
id = "handlers-stay-thin"
description = """
An HTTP route handler only translates between HTTP and the service layer.
FAILS: the handler queries a database, builds SQL, computes prices, discounts
or other business values, or loops over records to transform them.
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

**Write a rule the model can judge:**

- **State FAIL and PASS concretely.** Name the calls, imports or shapes. "Keep
  handlers clean" is not a rule; "queries a database or computes prices" is.
- **One concern per rule.** Each rule is asked as its own keyed question, so a
  bundled rule such as "no SQL _and_ no console logging" gets one verdict for
  two questions. You cannot tell which half fired. Split it into two rules.
- **Judge only what is in the scope.** The model sees the scope's source plus,
  for a method, the enclosing class. It does not see the file's imports or
  other files. Phrase the rule over what the body shows: `db.query(` is
  visible, "imports the database client" is not.
- **Plain imperative, no hedging.** Drop "should", "try to" and "where
  possible". An unclear rule produces `uncertain` verdicts below
  `min_confidence`, not findings.
- **Narrow, explicit exceptions.** List them in `exceptions`, and give each one
  a pass fixture.
- **`why` links, `fix` instructs.** Both are printed with every finding. They
  are what an agent reads to correct itself.

### 2d. Prove the rule with fixtures

Write at least one `fail/` file and one `pass/` file, **minimised from real code
you found in 2b**. The fail fixture is a real violation cut down to the scope
that violates. The pass fixture is its nearest legitimate neighbour. Add a pass
fixture for each exception. `rule new` already created the directories. Name
each file after the case it proves, for example `fail/queries-db.ts` and
`pass/calls-one-service.ts`.

```bash
lintent eval --rule <id> --dry-run | grep '^# POST'   # one line per fixture scope that will be judged
lintent eval --rule <id>
```

Fixtures ignore `include`, but `scopes` and `languages` still apply. The file's
extension must map to one of the rule's languages, and the file must contain a
scope of the rule's kind. A fixture with no matching scope is reported as
**vacuous** and fails eval. A fixture whose extension maps to no language is
not picked up at all, which is why the `grep '^# POST'` line above must list
every fixture. (`lintent scopes` cannot show fixtures: the default exclude
hides `.lintent/fixtures/**`.)

A fail fixture passes eval when at least one of its scopes fails confidently. A
pass fixture passes when none does. When eval misjudges, change the
**description**, not the fixture:

- sharpen the FAIL/PASS wording;
- name the construct the model missed;
- move a fuzzy case into `exceptions`.

Re-run eval until every fixture is judged correctly. A rule with no fail fixture
is reported as **unproven** and fails eval. Do not ship it.

### 2e. Run the rule on the real code and triage

**Always dry-run first.** It costs nothing and applies the same budget check:

```bash
lintent check --rule <id> --dry-run >/dev/null; echo "exit $?"
# stderr: "dry run: N request(s) for N question(s) on N scope(s) (K cached); nothing sent"
#         "estimate: ~$… (… input tokens, …) — within budget of $0.0001"   (or OVER budget, exit 3)
```

On **exit 3**, follow the printed guidance instead of raising the budget:

- pass explicit PATHS, starting with the files the search in 2b found;
- use `--changed` to check only this branch's diff;
- tighten `include` / `exclude`, or use a smaller `scopes` kind;
- check one rule at a time with `--rule`.

Questions that were already answered are cached and cost nothing, so a rule's
reach can be covered in several small runs. When a single scope costs more than
the whole budget, the guidance names it. Exclude that file, or give it a keep
mark with a reason. Then re-run the dry run. Only when it exits 0, run for
real:

```bash
lintent check --rule <id> src/routes/orders.ts
lintent check --rule <id> --json <paths> > "${TMPDIR:-/tmp}/lintent-<id>.json"   # findings, errors, stats incl. cost
```

The summary line reports the run's real cost. Decide which case each finding is:

| Finding is… | Do |
|---|---|
| A real violation | Report it. Fix it in this change or open a follow-up. Do not widen the rule to hide it |
| The rule misjudging | Copy the scope, minimised, into `pass/` as a regression fixture. Reword the description, then re-run `eval` and `check` |
| A recurring legitimate pattern | Add it to `exceptions`, with a pass fixture |
| A one-off intentional exception | Add a named keep mark on the scope's first line, or directly above it |
| `uncertain` (below `min_confidence`) | The wording is ambiguous for that shape. Tighten the description. Never lower `min_confidence` to make it go away |

```ts
// lintent-keep handlers-stay-thin -- the webhook must ack within 200 ms, so the signature check stays inline
export async function paymentWebhook(req: Request): Promise<Response> { … }
```

A mark binds to exactly one scope. That is the scope starting on the mark's own
line, or else the first scope below it with only blank and comment lines in
between. Decorators count as part of the scope. A mark on a class does not
cover its methods, and it never leaks onto the next function. Several rules go
comma-separated (`lintent-keep rule-a, rule-b -- reason`). Keep marks work in
any comment syntax the grammar has (`//`, `#`, `--`, `/* */`, …).

A mark with no reason, or one naming a rule that does not exist, is itself a
finding under `lintent/keep`. A mark that suppresses nothing is reported as
stale. An exemption nobody can audit is the thing keep marks exist to
prevent.

The rule is done when:

- `eval --rule <id>` is clean;
- `check --rule <id>` shows only real violations (reported) or named keeps;
- the rule file, its fixtures and any keep marks are committed together.

## 3. Review or maintain a rule

```bash
lintent rules                                     # id, severity, scopes, include
lintent eval --rule <id>                          # still judged right? (models drift)
lintent scopes --rule <id> | wc -l                # reach today vs when the rule was written
lintent check --rule <id> --dry-run >/dev/null    # questions, cache hits, cost estimate; nothing sent
lintent check --rule <id> --dry-run | head -60    # the exact request bodies
```

- **Re-prove first.** Run `eval` before trusting a rule after a model change or
  a description edit. Any edit to the rule file, even to `why`, invalidates the
  rule's cached verdicts. The next `check` then re-judges everything the rule
  reaches.
- **Tighten the wording,** using the misjudged cases as new fixtures. Every
  false positive you fix becomes a pass fixture, so it stays fixed.
- **Check the cost.** Questions per run = scopes in reach × rules that match
  them. Cached and kept questions are free. The dry run's `estimate:` line,
  and on exit 3 its by-rule, by-file and by-directory tables, show where the
  spend goes. `check --changed` limits a run to this branch's diff
  (merge-base with `origin/main`, or `--base <ref>`, plus uncommitted and
  untracked files). If a rule's reach has grown to thousands of scopes, it
  probably needs a narrower `include` or a smaller scope kind. Raising the
  budget is the user's call, never yours.
- **Audit the keeps.** Run `git grep -n 'lintent-keep <id>'`; every reason
  should still be true. A rule that collects many keeps is worded wrong, or is
  the wrong rule.
- **Promote or retire.** Promote `warning` to `error` once a full `check` is
  clean. If a rule turns out to be statable over syntax, move it to the
  deterministic linter and delete it here.

## 4. Add a language, or fix which scopes it finds

`lintent languages` lists every language and how it finds scopes:

- `tags`: the grammar's `tags.scm` marks `@definition.class`,
  `@definition.method` and `@definition.function`.
- `heuristic`: no tags query exists, so scopes come from node-type names.

To add a grammar lintent does not ship, add a table to `lintent.toml`:

```toml
[languages.haskell]
extensions = ["hs"]
repo = "https://github.com/tree-sitter/tree-sitter-haskell"   # or grammar = "<dir with src/parser.c | .so | .dylib>"
rev = "<full 40-character commit SHA>"                        # required with repo; never a tag or branch
# subdir = "…"                                                # repo only: grammar inside a monorepo
# tags_query = ".lintent/queries/haskell-tags.scm"            # optional
# symbol = "tree_sitter_haskell"                              # default tree_sitter_<name>
```

**A runtime grammar is native code.** lintent compiles it with the system C
compiler and loads it into its own process. Otherwise a committed
`lintent.toml` would run code from whoever last edited it. So these
protections apply:

- Runtime grammars are refused unless `--trust-grammars` is passed or
  `LINTENT_TRUST_GRAMMARS=1` is set in the real environment.
- `repo` needs `rev` pinned to a full commit SHA, so the code you reviewed is
  the code that runs.
- `grammar`, `subdir` and `tags_query` must be relative paths without `..`.

Before passing the flag, show the user the grammar source (repo, SHA) and get
their OK. **Never** set the trust flag in CI for a change from an untrusted
branch or PR. Built-in grammars need no trust.

The compiled grammar is cached in the user cache directory
(`~/Library/Caches/lintent` on macOS, `~/.cache/lintent` on Linux;
`LINTENT_CACHE_DIR` overrides this). A table named after a built-in, with no
`grammar` or `repo`, only adjusts it, for example by adding `extensions` or
replacing its `tags_query`. That needs no trust, because a tags query is data,
not code.

Verify on a real file:

```bash
lintent --trust-grammars languages | grep haskell
lintent --trust-grammars scopes path/to/Example.hs
```

**When the heuristic picks the wrong scopes,** for example a `module` reported
as a class or a lambda missed, write a tags query and point `tags_query` at it.
Commit it under `.lintent/queries/`. Look up node names in the grammar's
`src/node-types.json` or its upstream `queries/tags.scm`:

```scheme
(function_definition name: (identifier) @name) @definition.function
(class_declaration   name: (identifier) @name) @definition.class
```

A `@definition.function` nested inside a `@definition.class` is reported as a
method (`Class.name`). Re-run `lintent scopes <file>` until the list holds
exactly the units a reviewer would judge. Do this before writing any rule for
that language.
