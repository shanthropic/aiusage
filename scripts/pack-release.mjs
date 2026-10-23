import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { appendFileSync, copyFileSync, cpSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs'
import { resolve } from 'node:path'

const pkg = JSON.parse(readFileSync('package.json', 'utf8'))
assert.equal(pkg.name, 'aiusage')
assert.match(pkg.version, /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/)
assert.equal(pkg.napi.packageName, '@shanthropic/aiusage')
assert.equal(pkg.napi.binaryName, 'aiusage-core')
assert.equal(pkg.repository.url, 'git+https://github.com/shanthropic/aiusage.git')
const stage = resolve('release-stage')
const output = resolve('tarballs')
mkdirSync(stage)
mkdirSync(output)
cpSync('dist', `${stage}/dist`, { recursive: true })
mkdirSync(`${stage}/native`)
// The loader is platform-independent; take the Linux x64 build's verified copy.
for (const name of ['binding.cjs', 'binding.d.cts']) {
  copyFileSync(`artifacts/native-x86_64-unknown-linux-gnu/${name}`, `${stage}/native/${name}`)
}
copyFileSync('README.md', `${stage}/README.md`)
delete pkg.devDependencies
delete pkg.scripts
delete pkg.packageManager
writeFileSync(`${stage}/package.json`, JSON.stringify(pkg, null, 2) + '\n')
for (const args of [
  ['create-npm-dirs'],
  ['artifacts', '--output-dir', '../artifacts'],
  ['pre-publish', '--skip-optional-publish', '--no-gh-release'],
]) {
  execFileSync('pnpm', ['exec', 'napi', ...args, '--cwd', stage], { stdio: 'inherit' })
}
const staged = JSON.parse(readFileSync(`${stage}/package.json`, 'utf8'))
const dirs = readdirSync(`${stage}/npm`).sort()
assert.equal(dirs.length, 5)
assert.equal(Object.keys(staged.optionalDependencies).length, 5)
const packages = []
for (const dir of [...dirs.map(dir => `${stage}/npm/${dir}`), stage]) {
  const manifest = JSON.parse(readFileSync(`${dir}/package.json`, 'utf8'))
  if (dir !== stage) {
    assert.equal(manifest.name, `@shanthropic/aiusage-${dir.split('/').at(-1)}`)
    assert.equal(manifest.version, pkg.version)
    assert.equal(staged.optionalDependencies[manifest.name], pkg.version)
    assert.equal(manifest.cpu.length, 1)
    assert.equal(manifest.os.length, 1)
    if (manifest.os[0] === 'linux') assert.deepEqual(manifest.libc, ['glibc'])
  }
  const [packed] = JSON.parse(execFileSync('npm', ['pack', '--json', '--ignore-scripts', '--pack-destination', output], { cwd: dir, encoding: 'utf8' }))
  const binaries = packed.files.filter(file => file.path.endsWith('.node'))
  assert.equal(binaries.length, dir === stage ? 0 : 1)
  if (dir === stage) {
    for (const name of ['dist/index.js', 'native/binding.cjs']) {
      assert(packed.files.some(file => file.path === name), `Missing ${name}`)
    }
    assert(packed.files.every(file => /^(dist\/|native\/binding\.|package.json$|README.md$)/.test(file.path)))
  } else {
    assert.equal(binaries[0].path, manifest.main)
  }
  packages.push({ name: manifest.name, version: manifest.version, filename: packed.filename, integrity: packed.integrity, size: packed.size, unpackedSize: packed.unpackedSize })
}
writeFileSync(`${output}/manifest.json`, JSON.stringify({ version: pkg.version, packages }, null, 2) + '\n')
if (process.env.GITHUB_STEP_SUMMARY) {
  appendFileSync(process.env.GITHUB_STEP_SUMMARY, '| Package | Download bytes | Unpacked bytes |\n|---|---:|---:|\n' + packages.map(p => `| ${p.name} | ${p.size} | ${p.unpackedSize} |`).join('\n') + '\n')
}
