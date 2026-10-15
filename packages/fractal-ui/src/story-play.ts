/** Shared Storybook play helpers for kit stories. */
import { expect, waitFor } from 'storybook/test'

/**
 * The anchor's tooltip, resolved through the anchor's own aria-describedby and asserted to have
 * role tooltip, exactly `name` as its accessible name, and to be visible. A document-wide ByRole
 * query re-runs on every mutation and its cost grows with the page: about 300ms per check at 6x
 * CPU on an 811-element tab strip page, where a few checks consumed the whole 1s budget and
 * missed a tooltip that was already mounted. The anchor lookup does not depend on page size.
 */
export async function tooltipFor(anchor: HTMLElement, name: string): Promise<HTMLElement> {
  return waitFor(() => {
    const tooltip = anchor.getAttribute('aria-describedby')?.split(/\s+/).map(id => anchor.ownerDocument.getElementById(id)).find(node => node?.getAttribute('role') === 'tooltip')
    if (tooltip === undefined || tooltip === null) throw new Error(`Missing tooltip for ${name}`)
    expect(tooltip).toHaveAccessibleName(name)
    expect(tooltip).toBeVisible()
    return tooltip
  })
}
