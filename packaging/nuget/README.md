# lintent

[lintent](https://github.com/pietervp/lintent) checks code against lint rules
written in plain language, judged by a model and scoped with tree-sitter.

Install it as a local tool, so the version is pinned in your repository's tool
manifest (needs the .NET 10 SDK or later):

```bash
dotnet new tool-manifest    # once per repository
dotnet tool install lintent
dotnet lintent init
dotnet lintent check --changed
```

This package holds no .NET code: it points at the `lintent.<rid>` package for
your platform (macOS arm64/x64, Linux arm64/x64 with glibc, Windows x64), which
carries the native lintent binary.

The full reference is the [lintent README](https://github.com/pietervp/lintent#readme).
