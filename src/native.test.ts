import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import {
  clearFileMessagesCache,
  getFileMessagesCacheInfo,
  listFileMessagesCache,
  loadUsageData,
} from './native.js'

let originalUserProfile: string | undefined
let originalHome: string | undefined
let tempHome: string

function setupTempHome(): string {
  tempHome = mkdtempSync(join(tmpdir(), 'home-'))
  originalUserProfile = process.env.USERPROFILE
  originalHome = process.env.HOME
  process.env.USERPROFILE = tempHome
  process.env.HOME = tempHome
  return tempHome
}

function restoreHome() {
  if (originalUserProfile !== undefined) {
    process.env.USERPROFILE = originalUserProfile
  } else {
    delete process.env.USERPROFILE
  }
  if (originalHome !== undefined) {
    process.env.HOME = originalHome
  } else {
    delete process.env.HOME
  }
}

describe('native Rust NAPI bridge', () => {
  beforeEach(() => {
    setupTempHome()
    clearFileMessagesCache()
  })

  afterEach(() => {
    restoreHome()
  })

  it('returns empty array when no usage files exist', async () => {
    const result = await loadUsageData()
    expect(result).toEqual([])
  })

  it('parses usage across sources, populates SQLite parse cache, and filters by date range', async () => {
    const claudeDir = join(tempHome, '.claude')
    mkdirSync(claudeDir, { recursive: true })
    writeFileSync(
      join(claudeDir, 'usage.jsonl'),
      `{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}`
    )

    const result = await loadUsageData()
    expect(result).toHaveLength(1)
    expect(result[0].source).toBe('claude')
    expect(result[0].model.id).toBe('claude-3-5-sonnet')
    expect(result[0].tokens.input).toBe(100)
    expect(result[0].tokens.output).toBe(50)
    expect(result[0].date).toBeInstanceOf(Date)

    const cacheInfo = getFileMessagesCacheInfo()
    expect(cacheInfo.existsOnDisk).toBe(true)
    expect(cacheInfo.entries).toBe(1)
    expect(cacheInfo.messages).toBe(1)
    expect(listFileMessagesCache()).toHaveLength(1)

    const outOfRange = await loadUsageData({
      from: new Date('2025-01-01T00:00:00.000Z'),
      to: null,
    })
    expect(outOfRange).toEqual([])
  })
})
