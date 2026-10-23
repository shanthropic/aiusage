import { execFile as execFileCallback } from 'node:child_process'
import { mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { promisify } from 'node:util'

const execFile = promisify(execFileCallback)

export async function copyPngToClipboard(png: Uint8Array) {
  if (process.platform !== 'darwin') {
    throw new Error('Copying images is currently supported on macOS only.')
  }

  const directory = await mkdtemp(path.join(tmpdir(), 'mytokens-'))
  const imagePath = path.join(directory, 'overview.png')

  try {
    await writeFile(imagePath, png)
    await execFile('osascript', [
      '-e',
      'on run argv\nset the clipboard to (read (POSIX file (item 1 of argv)) as «class PNGf»)\nend run',
      imagePath,
    ])
  } finally {
    await rm(directory, { force: true, recursive: true })
  }
}
