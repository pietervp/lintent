#!/usr/bin/env bash
# Prints the Homebrew formula for a lintent release.
#
#   packaging/homebrew/formula.sh <version> <sha256sums.txt>
#
# <sha256sums.txt> is the release's checksum file (`sha256sum` output for the
# lintent-<target>.tar.gz archives). The release workflow writes the result to
# Formula/lintent.rb in pietervp/homebrew-tap.
set -euo pipefail

version=$1
sums=$2
base="https://github.com/pietervp/lintent/releases/download/v$version"

sha() {
  local sum
  sum=$(awk -v f="lintent-$1.tar.gz" '$2 == f || $2 == "*" f { print $1 }' "$sums")
  [ -n "$sum" ] || { echo "no checksum for lintent-$1.tar.gz in $sums" >&2; exit 1; }
  echo "$sum"
}

cat <<RUBY
class Lintent < Formula
  desc "Plain-language lint rules judged by a model, scoped with tree-sitter"
  homepage "https://github.com/pietervp/lintent"
  license "MIT"

  on_macos do
    on_arm do
      url "$base/lintent-aarch64-apple-darwin.tar.gz"
      sha256 "$(sha aarch64-apple-darwin)"
    end
    on_intel do
      url "$base/lintent-x86_64-apple-darwin.tar.gz"
      sha256 "$(sha x86_64-apple-darwin)"
    end
  end

  on_linux do
    on_arm do
      url "$base/lintent-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "$(sha aarch64-unknown-linux-gnu)"
    end
    on_intel do
      url "$base/lintent-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "$(sha x86_64-unknown-linux-gnu)"
    end
  end

  def install
    bin.install "lintent"
    pkgshare.install "skills"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/lintent --version")
    system bin/"lintent", "init"
    assert_path_exists testpath/"lintent.toml"
  end
end
RUBY
