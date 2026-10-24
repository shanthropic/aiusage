#!/usr/bin/env node
// Head-to-head benchmark: `npx tokenstat` (Rust parsers) vs `npx mytokens`
// (TypeScript parsers) on this machine's real history.
//
// Local only. Every timed run is a real npx invocation with --fresh (parse
// cache off) and npm offline (warm npm cache, no network). Wall time and
// memory are measured by bench/measure.ps1 so both variants share one method.
import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { cpus, homedir, tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'

const BENCH_DIR = fileURLToPath(new URL('.', import.meta.url))
const RESULTS_DIR = join(BENCH_DIR, 'results')

const VARIANTS = [
  { id: 'tokenstat', spec: 'tokenstat@0.1.1' },
  { id: 'mytokens', spec: 'mytokens@1.3.4' },
]

// The default screen (models-by-tokens) is already the heaviest report, so
// `all` and an explicit screen would measure the same work.
const WORKLOADS = {
  help: ['--help'],
  default: ['--fresh'],
  all: ['--fresh', '--all'],
}

const SOURCE_ROOTS = [
  '.claude',
  '.codex',
  '.config/claude',
  '.config/tokscale',
  '.config/manicode',
  '.config/manicode-dev',
  '.config/manicode-staging',
  '.config/Code/User/globalStorage/kilocode.kilo-code',
  '.config/Code/User/globalStorage/rooveterinaryinc.roo-cline',
  '.vscode-server/data/User/globalStorage/kilocode.kilo-code',
  '.vscode-server/data/User/globalStorage/rooveterinaryinc.roo-cline',
  '.copilot',
  '.factory',
  '.gemini',
  '.hermes',
  '.kimi',
  '.local/share/amp',
  '.local/share/goose',
  '.local/share/Block/goose',
  '.local/share/crush',
  '.local/share/kilo',
  '.local/share/octofriend',
  '.local/share/opencode',
  '.mux',
  '.openclaw',
  '.pi',
  '.qwen',
]

const argv = process.argv.slice(2)
const argValue = (name, fallback) => {
  const i = argv.indexOf(`--${name}`)
  return i >= 0 ? argv[i + 1] : fallback
}

const rounds = Number(argValue('rounds', 7))
const warmups = Number(argValue('warmups', 1))
const workloads = (argValue('workloads', Object.keys(WORKLOADS).join(',')) || '')
  .split(',')
  .filter(Boolean)
const selfCheck = argv.includes('--self-check')

if (process.platform !== 'win32') {
  throw new Error('bench/measure.ps1 is Windows-only; time-only runs were not built.')
}
if (!Number.isInteger(rounds) || rounds < 1) throw new Error('--rounds must be >= 1')
if (!Number.isInteger(warmups) || warmups < 0) throw new Error('--warmups must be >= 0')
for (const name of workloads) {
  if (!WORKLOADS[name]) throw new Error(`Unknown workload "${name}"; use ${Object.keys(WORKLOADS).join(', ')}`)
}

const env = {
  ...process.env,
  NO_COLOR: '1',
  FORCE_COLOR: '0',
  COLUMNS: '120',
  TZ: 'UTC',
  npm_config_offline: 'true',
  npm_config_update_notifier: 'false',
  npm_config_fund: 'false',
  npm_config_audit: 'false',
  npm_config_progress: 'false',
}

const tmp = mkdtempSync(join(tmpdir(), 'tokenstat-bench-'))
mkdirSync(RESULTS_DIR, { recursive: true })
let runCounter = 0

function runMeasured({ spec, args, online }) {
  const id = runCounter++
  const out = join(tmp, `${id}.out`)
  const err = join(tmp, `${id}.err`)
  const result = join(tmp, `${id}.json`)

  const parts = ['npx', '--yes', online ? null : '--offline', spec, ...args].filter(Boolean)
  for (const a of parts) {
    if (/\s/.test(a)) throw new Error(`Unquoted space in benchmark argument: ${a}`)
  }
  const command = parts.join(' ')
  const runEnv = { ...env, npm_config_offline: online ? 'false' : 'true' }

  const ps = spawnSync(
    'powershell.exe',
    [
      '-NoProfile',
      '-NonInteractive',
      '-ExecutionPolicy',
      'Bypass',
      '-File',
      join(BENCH_DIR, 'measure.ps1'),
      '-RunCommand',
      command,
      '-OutFile',
      out,
      '-ErrFile',
      err,
      '-ResultFile',
      result,
      '-HarnessPid',
      String(process.pid),
      '-WorkingDirectory',
      tmp,
    ],
    { env: runEnv, windowsHide: true, encoding: 'utf8', timeout: 900_000, maxBuffer: 16 * 1024 * 1024 }
  )
  if (ps.status !== 0 || !existsSync(result)) {
    throw new Error(
      `measure.ps1 failed (status ${ps.status}) for "${command}"\n${ps.stderr || ''}`
    )
  }

  const measured = JSON.parse(readFileSync(result, 'utf8'))
  measured.stdoutBytes = statSync(out).size
  measured.stdoutPath = out
  measured.stderrPath = err
  return measured
}

function runChecked(variant, workload, online) {
  const measured = runMeasured({ spec: variant.spec, args: WORKLOADS[workload], online })
  if (measured.timedOut || measured.exitCode !== 0) {
    const stderr = readFileSync(measured.stderrPath, 'utf8').slice(-2000)
    throw new Error(
      `${variant.spec} ${WORKLOADS[workload].join(' ')} exited ${measured.exitCode}\n${stderr}`
    )
  }
  if (measured.stdoutBytes === 0) {
    throw new Error(`${variant.spec} ${workload}: empty stdout`)
  }
  return measured
}

function readOutput(measured) {
  return readFileSync(measured.stdoutPath, 'utf8')
}

function normalize(text) {
  return text
    .replace(/\u001b\[[0-9;]*[A-Za-z]/g, '')
    .replace(/\r\n/g, '\n')
    // Humanized token magnitudes differ in display precision between the Rust
    // port and the TypeScript original (26.9M vs 27M); compare 2 significant
    // digits so a real data difference still shows up.
    .replace(/(\d+(?:\.\d+)?)([KMBT])\b/g, (_, n, unit) => `${twoSig(Number(n))}${unit}`)
    .split('\n')
    .map((line) =>
      // Brand names differ; both CLIs print the same report otherwise.
      line.replace(/\bmytokens\b/gi, 'CLI').replace(/\btokenstat\b/gi, 'CLI')
    )
    .map((line) => line.replace(/[ \t]+/g, ' ').trim())
    .filter(Boolean)
    .join('\n')
}

function twoSig(value) {
  if (!Number.isFinite(value) || value === 0) return String(value)
  const factor = 10 ** Math.max(0, 2 - (Math.floor(Math.log10(Math.abs(value))) + 1))
  return String(Number((Math.round(value * factor) / factor).toFixed(3)))
}

function stat(values) {
  const sorted = [...values].sort((a, b) => a - b)
  const mid = Math.floor(sorted.length / 2)
  const median =
    sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2
  return { median, min: sorted[0], max: sorted[sorted.length - 1] }
}

// Fingerprint of the history files both CLIs read: path + size + mtime.
// Content is not hashed (multi-GB); identical file lists are enough to know
// two result sets saw the same data.
function fingerprint() {
  const roots = []
  const lines = []
  const home = homedir()
  for (const root of SOURCE_ROOTS) {
    const abs = join(home, root)
    if (!existsSync(abs)) continue
    let files = 0
    let bytes = 0
    for (const entry of readdirSync(abs, { recursive: true, withFileTypes: true })) {
      if (!entry.isFile()) continue
      const full = join(entry.parentPath ?? entry.path, entry.name)
      const rel = full.slice(join(home).length + 1)
      const st = statSync(full)
      files++
      bytes += st.size
      lines.push(`${rel}\t${st.size}\t${Math.trunc(st.mtimeMs)}`)
    }
    roots.push({ root, files, bytes })
  }
  lines.sort()
  const hash = createHash('sha256').update(lines.join('\n')).digest('hex')
  const summary = roots.reduce(
    (acc, r) => ({ files: acc.files + r.files, bytes: acc.bytes + r.bytes }),
    { files: 0, bytes: 0 }
  )
  return { hash, ...summary, roots: roots.filter((r) => r.files > 0) }
}

const mb = (bytes) => `${(bytes / 1024 / 1024).toFixed(0)} MB`

const isFailure = (measured) => measured.timedOut || measured.exitCode !== 0

function failReason(measured) {
  if (measured.timedOut) return 'timed out'
  let stderr = ''
  try {
    stderr = readFileSync(measured.stderrPath, 'utf8')
  } catch {
    // Stderr is best effort; the exit code is the fact.
  }
  const hit = stderr.match(/.*(?:FATAL ERROR|Error):.*/)
  return `exit ${measured.exitCode}${hit ? ` (${hit[0].trim()})` : ''}`
}

function gateError(left, right) {
  const stamp = new Date().toISOString().replace(/[:.]/g, '-')
  writeFileSync(join(RESULTS_DIR, `gate-${stamp}-tokenstat.txt`), left)
  writeFileSync(join(RESULTS_DIR, `gate-${stamp}-mytokens.txt`), right)
  return new Error(
    `Correctness gate failed: reports differ. Normalized output written to ${RESULTS_DIR}.\n${diffLines(left, right)}`
  )
}

function markdownTable(header, rows) {
  const sep = header.map(() => '---')
  return [header, sep, ...rows].map((row) => `| ${row.join(' | ')} |`).join('\n')
}

function diffLines(a, b) {
  const left = a.split('\n')
  const right = b.split('\n')
  const diffs = []
  for (let i = 0; i < Math.max(left.length, right.length); i++) {
    if (left[i] !== right[i]) {
      diffs.push(`line ${i + 1}\n  tokenstat: ${left[i] ?? '<missing>'}\n  mytokens:  ${right[i] ?? '<missing>'}`)
      if (diffs.length === 20) break
    }
  }
  return diffs.join('\n')
}

function npmVersion() {
  const ps = spawnSync('npm --version', { env, encoding: 'utf8', windowsHide: true, shell: true })
  return (ps.stdout || '').trim() || 'unknown'
}

async function main() {
  const before = fingerprint()
  console.log(
    `history: ${before.files} files, ${mb(before.bytes)}, fingerprint ${before.hash.slice(0, 12)}`
  )
  console.log(
    `workloads: ${workloads.join(', ')} | rounds: ${rounds} (+${warmups} warmup) | node ${process.version} | npm ${npmVersion()}`
  )

  // Prewarm: one online run per variant downloads the package and fills the
  // shared remote price/model cache. Timed runs are offline afterwards.
  console.log('prewarming (npm download + remote data cache)...')
  for (const variant of VARIANTS) {
    runChecked(variant, 'help', true)
    runChecked(variant, 'default', true)
  }
  for (const variant of VARIANTS) runChecked(variant, 'help', false)

  const samples = {}
  const failures = {}
  const workloadsToRun = selfCheck ? ['default'] : workloads

  for (const workload of workloadsToRun) {
    samples[workload] = Object.fromEntries(VARIANTS.map((v) => [v.id, []]))
    failures[workload] = {}

    // The first offline run per variant doubles as the correctness gate and as
    // the "can this variant finish this workload at all" check.
    const first = {}
    for (const variant of VARIANTS) {
      first[variant.id] = runMeasured({ spec: variant.spec, args: WORKLOADS[workload], online: false })
      if (isFailure(first[variant.id])) failures[workload][variant.id] = failReason(first[variant.id])
    }

    const runnable = VARIANTS.filter((variant) => !failures[workload][variant.id])
    if (runnable.length === VARIANTS.length && WORKLOADS[workload].includes('--fresh')) {
      const left = normalize(readOutput(first.tokenstat))
      const right = normalize(readOutput(first.mytokens))
      if (left !== right) throw gateError(left, right)
      console.log(`${workload}: correctness gate passed`)
    }
    for (const [variant, reason] of Object.entries(failures[workload])) {
      console.log(`${workload}: ${variant} FAILED (${reason})`)
    }
    if (selfCheck || runnable.length === 0) continue

    for (let i = 0; i < warmups; i++) {
      for (const variant of runnable) runChecked(variant, workload, false)
    }
    for (let roundIndex = 0; roundIndex < rounds; roundIndex++) {
      const order = roundIndex % 2 === 0 ? runnable : [...runnable].reverse()
      for (const variant of order) {
        samples[workload][variant.id].push(runChecked(variant, workload, false))
      }
      process.stdout.write(
        `  ${workload}: round ${roundIndex + 1}/${rounds} (${order.map((v) => v.id).join(' then ')})\n`
      )
    }
  }

  if (selfCheck) {
    const failed = Object.entries(failures).flatMap(([workload, byVariant]) =>
      Object.entries(byVariant).map(([variant, reason]) => `${workload}/${variant}: ${reason}`)
    )
    if (failed.length) throw new Error(`self-check failed: ${failed.join('; ')}`)
    console.log('\nself-check OK: both variants resolve and render the same default report offline')
    return
  }

  const after = fingerprint()
  const stable = before.hash === after.hash
  if (!stable) console.warn('\nWARNING: history changed during the run; results are not comparable.')

  const time = {}
  const memory = {}
  for (const workload of workloadsToRun) {
    time[workload] = {}
    memory[workload] = {}
    for (const variant of VARIANTS) {
      const runs = samples[workload][variant.id]
      if (!runs.length) continue
      time[workload][variant.id] = stat(runs.map((r) => r.wallMs))
      memory[workload][variant.id] = stat(runs.map((r) => r.nodeSumPeakMB))
    }
  }

  const failureLabel = (workload, variant) =>
    failures[workload]?.[variant] ? `failed (${failures[workload][variant]})` : 'not run'

  const timeRows = workloadsToRun.map((workload) => {
    const t = time[workload].tokenstat
    const m = time[workload].mytokens
    return [
      workload,
      t ? `${t.min.toFixed(0)} / ${t.median.toFixed(0)} / ${t.max.toFixed(0)}` : failureLabel(workload, 'tokenstat'),
      m ? `${m.min.toFixed(0)} / ${m.median.toFixed(0)} / ${m.max.toFixed(0)}` : failureLabel(workload, 'mytokens'),
      t && m ? `${(m.median / t.median).toFixed(2)}x` : 'n/a',
      t && m ? `${(m.median - t.median).toFixed(0)} ms` : 'n/a',
    ]
  })
  const memoryRows = workloadsToRun.map((workload) => {
    const t = memory[workload].tokenstat?.median
    const m = memory[workload].mytokens?.median
    return [
      workload,
      t === undefined ? failureLabel(workload, 'tokenstat') : t.toFixed(1),
      m === undefined ? failureLabel(workload, 'mytokens') : m.toFixed(1),
      t !== undefined && m !== undefined ? `${(m / t).toFixed(2)}x` : 'n/a',
    ]
  })

  const timeTable = markdownTable(
    ['workload', 'tokenstat ms (min/median/max)', 'mytokens ms (min/median/max)', 'mytokens/tokenstat', 'median delta'],
    timeRows
  )
  const memoryTable = markdownTable(
    ['workload', 'tokenstat peak node MB', 'mytokens peak node MB', 'mytokens/tokenstat'],
    memoryRows
  )

  console.log(`\n## Wall time (npx included, ${rounds} rounds)\n\n${timeTable}\n`)
  console.log(`\n## Peak memory, all node processes in the run tree\n\n${memoryTable}\n`)
  console.log(
    `\nfingerprint ${before.hash} | ${stable ? 'history stable during run' : 'HISTORY CHANGED'}\n`
  )

  const previous = previousComparable(before.hash)
  if (previous) {
    const deltas = workloadsToRun
      .filter(
        (w) =>
          time[w]?.tokenstat &&
          time[w]?.mytokens &&
          previous.summary?.time?.[w]?.tokenstat &&
          previous.summary.time[w].mytokens
      )
      .map((w) => {
        const oldT = previous.summary.time[w].tokenstat.median
        const oldM = previous.summary.time[w].mytokens.median
        return `${w}: tokenstat ${(time[w].tokenstat.median - oldT).toFixed(0)} ms, mytokens ${(time[w].mytokens.median - oldM).toFixed(0)} ms`
      })
    if (deltas.length) {
      console.log(`vs previous run (${previous.generatedAt}, same fingerprint): ${deltas.join('; ')}`)
    }
  }

  for (const workload of workloadsToRun) {
    for (const variant of VARIANTS) {
      for (const sample of samples[workload][variant.id]) {
        delete sample.stdoutPath
        delete sample.stderrPath
      }
    }
  }

  const stamp = new Date().toISOString().replace(/[:.]/g, '-')
  const report = {
    generatedAt: new Date().toISOString(),
    stable,
    platform: process.platform,
    arch: process.arch,
    cpu: cpus()[0]?.model,
    node: process.version,
    npm: npmVersion(),
    variants: VARIANTS,
    workloads: workloadsToRun,
    rounds,
    warmups,
    fingerprint: before,
    fingerprintAfter: after,
    failures,
    summary: { time, memory },
    samples,
  }
  writeFileSync(join(RESULTS_DIR, `${stamp}.json`), JSON.stringify(report, null, 2))
  writeFileSync(
    join(RESULTS_DIR, `${stamp}.md`),
    [
      `# tokenstat vs mytokens benchmark`,
      ``,
      `Generated: ${report.generatedAt}`,
      `History: ${before.files} files, ${mb(before.bytes)}, fingerprint \`${before.hash}\` (${stable ? 'stable' : 'CHANGED'})`,
      `Machine: ${report.cpu} | node ${report.node} | npm ${report.npm}`,
      ``,
      `## Wall time`,
      ``,
      timeTable,
      ``,
      `## Peak memory`,
      ``,
      memoryTable,
      ``,
      `Runs: ${rounds} measured + ${warmups} discarded warmup per workload, A/B order alternated.`,
      `Every run: \`npx --yes --offline <package> <args>\`, redirected stdout, no TTY.`,
      `\`help\` is startup only; \`default\` and \`all\` add \`--fresh\` (parse cache bypassed).`,
      ``,
    ].join('\n')
  )
  console.log(`\nwrote ${RESULTS_DIR}\\${stamp}.json and .md`)
}

function previousComparable(hash) {
  if (!existsSync(RESULTS_DIR)) return null
  const files = readdirSync(RESULTS_DIR)
    .filter((f) => f.endsWith('.json'))
    .sort()
    .reverse()
  for (const file of files) {
    try {
      const report = JSON.parse(readFileSync(join(RESULTS_DIR, file), 'utf8'))
      if (report.stable && report.fingerprint?.hash === hash) return report
    } catch {
      // Ignore unreadable result files.
    }
  }
  return null
}

try {
  await main()
} finally {
  rmSync(tmp, { recursive: true, force: true })
}
