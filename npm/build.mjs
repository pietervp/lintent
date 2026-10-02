#!/usr/bin/env node
// Assembles the npm packages from the release binaries.
//
//   node npm/build.mjs <artifacts-dir> <out-dir>
//
// <artifacts-dir>/<rust-target>/lintent[.exe] are the binaries the release
// workflow built. Writes <out-dir>/<package-dir>/ for each platform package
// and for @lintent/cli, all at the version in Cargo.toml. Under a tag push
// the tag must name that same version.
import { chmodSync, copyFileSync, cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const [artifacts, out] = process.argv.slice(2)
if (!artifacts || !out) {
  console.error('usage: node npm/build.mjs <artifacts-dir> <out-dir>')
  process.exit(2)
}

const PLATFORMS = [
  { target: 'aarch64-apple-darwin', suffix: 'darwin-arm64', os: 'darwin', cpu: 'arm64' },
  { target: 'x86_64-apple-darwin', suffix: 'darwin-x64', os: 'darwin', cpu: 'x64' },
  { target: 'aarch64-unknown-linux-gnu', suffix: 'linux-arm64-gnu', os: 'linux', cpu: 'arm64', libc: 'glibc' },
  { target: 'x86_64-unknown-linux-gnu', suffix: 'linux-x64-gnu', os: 'linux', cpu: 'x64', libc: 'glibc' },
  { target: 'x86_64-pc-windows-msvc', suffix: 'win32-x64', os: 'win32', cpu: 'x64', exe: '.exe' },
]

const cargo = readFileSync(join(root, 'Cargo.toml'), 'utf8')
const version = cargo.match(/^version\s*=\s*"([^"]+)"/m)?.[1]
if (!version) throw new Error('no version in Cargo.toml')
if (process.env.GITHUB_REF_TYPE === 'tag' && process.env.GITHUB_REF_NAME !== `v${version}`) {
  console.error(`tag ${process.env.GITHUB_REF_NAME} does not match Cargo.toml version ${version}`)
  process.exit(1)
}

const wrapper = JSON.parse(readFileSync(join(root, 'npm/cli/package.json'), 'utf8'))
const shared = {
  version,
  description: wrapper.description,
  license: wrapper.license,
  homepage: wrapper.homepage,
  repository: wrapper.repository,
}

rmSync(out, { recursive: true, force: true })
const optional = {}
for (const p of PLATFORMS) {
  const binary = join(artifacts, p.target, `lintent${p.exe ?? ''}`)
  if (!existsSync(binary)) {
    console.error(`missing ${binary}`)
    process.exit(1)
  }
  const name = `@lintent/cli-${p.suffix}`
  const dir = join(out, `cli-${p.suffix}`)
  mkdirSync(join(dir, 'bin'), { recursive: true })
  const dest = join(dir, 'bin', `lintent${p.exe ?? ''}`)
  copyFileSync(binary, dest)
  copyFileSync(join(root, 'LICENSE'), join(dir, 'LICENSE'))
  // Artifact uploads drop the executable bit.
  chmodSync(dest, 0o755)
  const pkg = {
    name,
    ...shared,
    description: `The ${p.suffix} binary of @lintent/cli`,
    os: [p.os],
    cpu: [p.cpu],
    ...(p.libc ? { libc: [p.libc] } : {}),
    files: ['bin'],
    preferUnplugged: true,
  }
  writeFileSync(join(dir, 'package.json'), `${JSON.stringify(pkg, null, 2)}\n`)
  optional[name] = version
}

const cliDir = join(out, 'cli')
cpSync(join(root, 'npm/cli'), cliDir, { recursive: true })
cpSync(join(root, 'skills/lintent'), join(cliDir, 'skills/lintent'), { recursive: true })
copyFileSync(join(root, 'LICENSE'), join(cliDir, 'LICENSE'))
chmodSync(join(cliDir, 'bin/lintent.js'), 0o755)
writeFileSync(
  join(cliDir, 'package.json'),
  `${JSON.stringify({ ...wrapper, version, optionalDependencies: optional }, null, 2)}\n`,
)

console.log(`npm packages for ${version} in ${out}: ${PLATFORMS.map(p => `cli-${p.suffix}`).join(', ')}, cli`)
