import { APP_SCREENS_MAP, AppScreenType } from './constants/screen.js'
import { loadUsageData } from './native.js'
import { RenderScreenOptions } from './render/types.js'

export async function runApp(
  options: RenderScreenOptions & { screen: AppScreenType }
) {
  const data = await loadUsageData({
    from: options.dateStart ?? null,
    to: options.dateEnd ?? null,
  })

  const ScreenConstructor = APP_SCREENS_MAP[options.screen]
  if (!ScreenConstructor) {
    throw new Error(`Screen "${options.screen}" not found`)
  }

  const screen = new ScreenConstructor(data, options)
  await screen.setup()
  await screen.render()
}
