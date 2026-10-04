// What this app tells st it is, in the `x-st3-client` header, so `st clients` can list it:
// "smalltalk-ios 0.1.0", with the commit when the build was stamped (EXPO_PUBLIC_ST3_BUILD).
// st shows it as reported; it is never identity or authority.
export function clientName(version: string, build: string | undefined = process.env.EXPO_PUBLIC_ST3_BUILD): string {
  return `smalltalk-ios ${version}${build ? ` (${build})` : ''}`;
}
