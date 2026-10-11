/** This boundary checks modules as well as emitted bytes, including lazy chunks and assets. */
export const privateModule = /(?:\/(?:webfractal|fractal-web)\/private\/|\/src\/vista\/|\/flakes\/vista\/|@vista\/blocks|@overeng\/agent-trace|\/shell\/AgentUsage(?:Data|Details|Facts|Styles)\.)/
export const privateFingerprints = ['wf.private.agent-usage', '/wf/usage/', 'wf.usage.readLens', 'Native OMP root mapping unavailable']

export const assertBundleBoundary = ({ modules, texts, privateBuild }) => {
  const privateModules = modules.filter((id) => privateModule.test(id.replaceAll('\\', '/')))
  if (!privateBuild && privateModules.length > 0)
    throw new Error('Public build acquired private modules: ' + privateModules.join(', '))
  const body = texts.join('\n')
  if (privateBuild) {
    for (const marker of privateFingerprints)
      if (!body.includes(marker)) throw new Error('Extension build lost plugin content: ' + marker)
    for (const pattern of [/\/src\/vista\/feature\.tsx/, /AgentUsageDetails\.tsx/, /AgentUsageFacts\.ts/])
      if (!privateModules.some((id) => pattern.test(id)))
        throw new Error('Extension build lost plugin module: ' + pattern)
  } else {
    for (const marker of privateFingerprints)
      if (body.includes(marker)) throw new Error('Public bundle contains private content: ' + marker)
  }
  return { privateBuild, privateModuleCount: privateModules.length }
}
