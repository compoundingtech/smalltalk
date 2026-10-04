// What a client reports in the x-st3-client header. This is never identity or authority.
export function clientName(name: string, version: string, build?: string): string {
  return `${name} ${version}${build ? ` (${build})` : ''}`;
}
