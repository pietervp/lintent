#!/usr/bin/env bash
# One-time setup for npm trusted publishing.
#
# npm configures a trusted publisher per package, and only for a package that
# exists. This publishes a 0.0.0 placeholder of each lintent package so that
# their settings pages exist; the release workflow publishes real versions
# from then on. Run it once, logged in (`npm login`) as an owner of the
# @lintent org.
set -euo pipefail

packages=(
  @lintent/cli
  @lintent/cli-darwin-arm64
  @lintent/cli-darwin-x64
  @lintent/cli-linux-arm64-gnu
  @lintent/cli-linux-x64-gnu
  @lintent/cli-win32-x64
)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

for name in "${packages[@]}"; do
  if npm view "$name" version >/dev/null 2>&1; then
    echo "$name exists already"
    continue
  fi
  dir="$work/${name#@lintent/}"
  mkdir -p "$dir"
  cat > "$dir/package.json" <<JSON
{
  "name": "$name",
  "version": "0.0.0",
  "description": "Placeholder: the first real release of lintent replaces it",
  "repository": { "type": "git", "url": "git+https://github.com/pietervp/lintent.git" }
}
JSON
  echo "This version is a placeholder. See https://github.com/pietervp/lintent" > "$dir/README.md"
  (cd "$dir" && npm publish --access public)
done

cat <<'TEXT'

Now, for each package, open https://www.npmjs.com/package/<name>/access,
add a trusted publisher (GitHub Actions; user pietervp, repository lintent,
workflow release.yml), and under "Publishing access" choose to disallow tokens.
TEXT
