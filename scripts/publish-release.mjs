import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'

const release = JSON.parse(readFileSync('tarballs/manifest.json', 'utf8'))
assert.equal(process.env.GITHUB_REPOSITORY, 'shanthropic/aiusage')
assert.equal(process.env.GITHUB_REF, `refs/tags/aiusage-v${release.version}`)
assert.match(release.version, /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/)
const names = ['darwin-arm64', 'darwin-x64', 'linux-arm64-gnu', 'linux-x64-gnu', 'win32-x64-msvc'].map(p => `@shanthropic/aiusage-${p}`)
assert.deepEqual(release.packages.map(p => p.name), [...names, 'aiusage'])

async function metadata(spec) {
  const response = await fetch(`https://registry.npmjs.org/${spec}`)
  if (response.status === 404) return null
  assert(response.ok, `Registry returned ${response.status} for ${spec}`)
  return response.json()
}

// Verify every tarball before publishing even the first platform package.
for (const pkg of release.packages) {
  assert.equal(pkg.version, release.version)
  assert.match(pkg.filename, /^[a-z0-9.-]+\.tgz$/)
  const integrity = 'sha512-' + createHash('sha512').update(readFileSync(`tarballs/${pkg.filename}`)).digest('base64')
  assert.equal(integrity, pkg.integrity)
  const existing = await metadata(`${encodeURIComponent(pkg.name)}/${pkg.version}`)
  if (existing) assert.equal(existing.dist.integrity, pkg.integrity, `Published bits differ for ${pkg.name}`)
}
const latest = await metadata('aiusage/latest')
if (latest) {
  const current = latest.version.split('.').map(Number)
  const next = release.version.split('.').map(Number)
  const firstDifference = next.findIndex((n, i) => n !== current[i])
  assert(firstDifference === -1 || next[firstDifference] > current[firstDifference], 'Refusing to move latest backward')
}
for (const pkg of release.packages) {
  const spec = `${encodeURIComponent(pkg.name)}/${pkg.version}`
  if (!(await metadata(spec))) {
    execFileSync('npm', ['publish', `tarballs/${pkg.filename}`, '--access', 'public', '--ignore-scripts'], { stdio: 'inherit' })
  }
  const published = await metadata(spec)
  assert.equal(published?.dist.integrity, pkg.integrity, `Registry verification failed for ${pkg.name}; rerun this publishing job`)
}
