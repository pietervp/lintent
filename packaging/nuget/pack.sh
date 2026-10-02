#!/usr/bin/env bash
# Packs the NuGet packages from the release binaries.
#
#   packaging/nuget/pack.sh <artifacts-dir> <out-dir>
#
# <artifacts-dir>/<rust-target>/lintent[.exe] are the binaries the release
# workflow built. Writes lintent.<rid>.<version>.nupkg for each platform and
# the pointer package lintent.<version>.nupkg, at the version in Cargo.toml.
set -euo pipefail

artifacts=$(cd "$1" && pwd)
mkdir -p "$2"
out=$(cd "$2" && pwd)
here=$(cd "$(dirname "$0")" && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$here/../../Cargo.toml" | head -1)

pack() {
  dotnet pack "$here/lintent.pack.csproj" --nologo -v quiet -o "$out" -p:Version="$version" "$@"
}

while read -r target rid exe; do
  pack -p:ToolRid="$rid" -p:Binary="$artifacts/$target/lintent$exe"
done <<'TARGETS'
aarch64-apple-darwin osx-arm64
x86_64-apple-darwin osx-x64
aarch64-unknown-linux-gnu linux-arm64
x86_64-unknown-linux-gnu linux-x64
x86_64-pc-windows-msvc win-x64 .exe
TARGETS
pack

ls "$out"
