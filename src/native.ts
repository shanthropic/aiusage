import { createRequire } from 'node:module'
import type { UsageDataMessage } from '@/core/types.js'

export type DateRange = {
  from: Date | null
  to: Date | null
}

export type RawUsageMessage = {
  source: string
  agent: string
  type: string
  dateMs: number
  modelId: string
  provider: string
  input: number
  output: number
  reasoning: number
  cacheInput: number
  cacheOutput: number
  projectName?: string
  projectPath?: string
}

export type FileMessagesCacheEntry = {
  path: string
  messages: number
  bytes: number
}

export type FileMessagesCacheInfo = {
  existsOnDisk: boolean
  fileBytes: number
  entries: number
  messages: number
}

interface NativeBinding {
  loadUsageDataNative(
    fromMs?: number | null,
    toMs?: number | null,
    fresh?: boolean | null
  ): Promise<RawUsageMessage[]>
  disableFileMessagesCache(): void
  clearFileMessagesCache(): void
  listFileMessagesCache(): FileMessagesCacheEntry[]
  getFileMessagesCacheInfo(): FileMessagesCacheInfo
}

const require = createRequire(import.meta.url)
const binding = require('../native/binding.cjs') as NativeBinding

export async function loadUsageData(
  range?: DateRange
): Promise<UsageDataMessage[]> {
  const raw = await binding.loadUsageDataNative(
    range?.from ? range.from.getTime() : null,
    range?.to ? range.to.getTime() : null,
    null
  )

  return raw.map((m) => ({
    source: m.source,
    agent: m.agent,
    type: (m.type as UsageDataMessage['type']) || 'assistant',
    date: new Date(m.dateMs),
    model: {
      id: m.modelId,
      provider: m.provider,
    },
    tokens: {
      input: m.input,
      output: m.output,
      reasoning: m.reasoning,
      cacheInput: m.cacheInput,
      cacheOutput: m.cacheOutput,
    },
    project:
      m.projectName || m.projectPath
        ? {
            name: m.projectName,
            path: m.projectPath,
          }
        : undefined,
  }))
}

export function disableFileMessagesCache(): void {
  binding.disableFileMessagesCache()
}

export function clearFileMessagesCache(): void {
  binding.clearFileMessagesCache()
}

export function listFileMessagesCache(): FileMessagesCacheEntry[] {
  return binding.listFileMessagesCache()
}

export function getFileMessagesCacheInfo(): FileMessagesCacheInfo {
  return binding.getFileMessagesCacheInfo()
}
