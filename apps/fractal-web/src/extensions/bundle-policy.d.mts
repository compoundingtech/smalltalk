export const privateModule: RegExp
export const privateFingerprints: readonly string[]
export const assertBundleBoundary: (input: {
  readonly modules: readonly string[]; readonly texts: readonly string[]; readonly privateBuild: boolean
}) => { readonly privateBuild: boolean; readonly privateModuleCount: number }
