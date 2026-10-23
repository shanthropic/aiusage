import { UsageDataMessage } from '@/core/types.js'
import { writeFileForced } from '@/helpers/fs.js'
import { DateRange, loadUsageData as loadUsageDataFromRust } from '@/native.js'
import { isMessageIgnored } from './chart.js'
import { copyPngToClipboard } from './clipboard.js'
import { renderPng } from './image.js'
import { renderOverviewToSvg } from './overview-svg.js'
import { computeOverview } from './overview.js'
import {
  renderImageInTerminal,
  TerminalImageOptions,
} from './terminal-image.js'
import { DEFAULT_EXPORT_THEME } from './themes.js'
import { ExportFilterOptions, ExportFormat, ExportThemeId } from './types.js'

export async function loadUsageData(
  range?: DateRange
): Promise<UsageDataMessage[]> {
  return loadUsageDataFromRust(range)
}

function filterMessages(
  data: UsageDataMessage[],
  options: ExportFilterOptions
): UsageDataMessage[] {
  return data.filter((message) => !isMessageIgnored(message, options))
}

export async function buildOverviewSvg(
  options: ExportFilterOptions,
  themeId: ExportThemeId = DEFAULT_EXPORT_THEME,
  projects = 10
): Promise<string> {
  const data = await loadUsageData({
    from: options.dateStart ?? null,
    to: options.dateEnd ?? null,
  })
  const messages = filterMessages(data, options)
  const overview = await computeOverview(messages, {
    dateStart: options.dateStart,
    dateEnd: options.dateEnd,
    usageBy: options.usageBy,
    projects,
    groupModels: options.groupModels,
    autoGroupModels: options.autoGroupModels,
    refetchRemote: options.refetchRemote,
  })
  return renderOverviewToSvg(overview, themeId)
}

export async function exportReportToSvg(
  outputPath: string,
  options: ExportFilterOptions,
  fileOptions: {
    format?: ExportFormat
    theme?: ExportThemeId
    scale?: number
    projects?: number
  } = {}
): Promise<string> {
  const {
    format = 'svg',
    theme = DEFAULT_EXPORT_THEME,
    scale = 1,
  } = fileOptions
  const svg = await buildOverviewSvg(options, theme, fileOptions.projects)
  if (format === 'png') {
    await writeFileForced(outputPath, renderPng(svg, { scale }))
    return outputPath
  }
  await writeFileForced(outputPath, svg)
  return outputPath
}

export async function showReportImage(
  options: ExportFilterOptions,
  imageOptions: TerminalImageOptions & {
    copy?: boolean
    theme?: ExportThemeId
    projects?: number
  } = {}
): Promise<string> {
  const {
    copy = false,
    theme = DEFAULT_EXPORT_THEME,
    ...terminalImageOptions
  } = imageOptions
  const svg = await buildOverviewSvg(options, theme, imageOptions.projects)
  const png = await renderImageInTerminal(svg, terminalImageOptions)
  if (copy) {
    await copyPngToClipboard(png)
  }
  return svg
}
