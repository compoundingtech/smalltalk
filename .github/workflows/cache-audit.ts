/** Every executable job declares its cache coverage, including jobs with no build inputs. */
export const noBuildCache = (reason: string) => ({
  name: 'Report job cache coverage',
  if: 'always()',
  shell: 'bash',
  env: { CACHE_REASON: reason },
  run: `printf 'Cache coverage: **not applicable** — %s\\n' "$CACHE_REASON" >> "$GITHUB_STEP_SUMMARY"`,
})

export const auditCaches = <T extends { jobs: Record<string, any> }>(workflow: T, noBuild: Record<string, string> = {}): T => {
  const jobs = Object.fromEntries(Object.entries(workflow.jobs).map(([name, job]) => {
    const entries: Record<string, unknown>[] = []
    const steps = job.steps.map((step: any, index: number) => {
      if (step.id !== 'build-snapshot' && !/^(actions\/cache(?:\/restore)?@|Swatinem\/rust-cache@)/.test(step.uses ?? '')) return step
      const id = step.id ?? `cache-${index}`
      entries.push({
        name: step.name ?? ((step.uses ?? '').startsWith('Swatinem/') ? 'Cargo release' : id),
        outcome: `\${{ steps.${id}.outcome }}`,
        hit: `\${{ steps.${id}.outputs.cache-hit }}`,
        // rust-cache reports exact match only; Actions cache also exposes its fallback key.
        matched: (step.uses ?? '').startsWith('actions/cache/restore@') ? `\${{ steps.${id}.outputs.cache-matched-key }}` : '',
        opaqueFallback: !(step.uses ?? '').startsWith('actions/cache/restore@'),
        sourceOnly: step.id === 'build-snapshot',
        avoidable: step.id === 'build-snapshot' ? `\${{ steps.${id}.outputs.avoidable-miss }}` : '',
        lookup: step.with?.['lookup-only'] === true,
      })
      return { ...step, id }
    })
    if (entries.length) {
      steps.push({ name: 'Report job cache coverage', if: 'always()', shell: 'bash',
        env: { CACHE_ENTRIES: JSON.stringify(entries) }, run: 'python3 "${CI_CACHE_AUDIT_SCRIPT:-scripts/ci-cache-audit}"' })
    } else if (noBuild[name]) {
      steps.push(noBuildCache(noBuild[name]))
    } else {
      throw new Error(`Cache audit missing a policy for job ${name}`)
    }
    return [name, { ...job, steps }]
  }))
  return { ...workflow, jobs } as T
}
