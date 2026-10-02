#!/usr/bin/env node
// Runs the lintent binary from the platform package npm installed next to
// this one. Each `@lintent/cli-<platform>` package declares `os`, `cpu` (and
// `libc` on Linux), so npm installs exactly one of them.
'use strict'

const { spawnSync } = require('node:child_process')
const fs = require('node:fs')

const PACKAGES = {
  'darwin-arm64': '@lintent/cli-darwin-arm64',
  'darwin-x64': '@lintent/cli-darwin-x64',
  'linux-arm64': '@lintent/cli-linux-arm64-gnu',
  'linux-x64': '@lintent/cli-linux-x64-gnu',
  'win32-x64': '@lintent/cli-win32-x64',
}

function isMusl() {
  if (process.platform !== 'linux') return false
  const report = process.report && process.report.getReport()
  return !(report && report.header && report.header.glibcVersionRuntime)
}

function binaryPath() {
  // An explicit binary (a local build, a distro package) wins.
  if (process.env.LINTENT_BINARY) return process.env.LINTENT_BINARY

  const key = `${process.platform}-${process.arch}`
  const pkg = PACKAGES[key]
  if (!pkg || isMusl()) {
    fail(
      `lintent has no prebuilt binary for ${key}${isMusl() ? ' (musl)' : ''}.\n` +
        'Build it with `cargo install --git https://github.com/pietervp/lintent`\n' +
        'and point LINTENT_BINARY at the result.',
    )
  }
  const exe = process.platform === 'win32' ? 'lintent.exe' : 'lintent'
  try {
    return require.resolve(`${pkg}/bin/${exe}`)
  } catch {
    fail(
      `The lintent binary package ${pkg} is not installed.\n` +
        'It is an optional dependency of @lintent/cli: reinstall without\n' +
        '--no-optional / --omit=optional, and make sure your lockfile was not\n' +
        'generated on a different platform with optional packages pruned.',
    )
  }
}

function fail(message) {
  process.stderr.write(`lintent: ${message}\n`)
  process.exit(2)
}

const bin = binaryPath()
if (process.platform !== 'win32') {
  // Some package managers drop the executable bit when unpacking.
  try {
    fs.accessSync(bin, fs.constants.X_OK)
  } catch {
    try {
      fs.chmodSync(bin, 0o755)
    } catch {}
  }
}

const result = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' })
if (result.error) fail(`could not run ${bin}: ${result.error.message}`)
if (result.signal) process.kill(process.pid, result.signal)
process.exit(result.status ?? 2)
