/** Where a viewport-sized picture actually sits inside the element it is drawn in.
 *
 * The panel's picture is the browser's own pixels and it is drawn with `object-fit: contain`, which
 * means the element's box and the *drawn page* are not the same rectangle whenever the two aspects
 * differ: the difference is left as ground on two sides. Every piece of arithmetic that turns a
 * point in the picture into a point in the page has to go through the drawn rectangle — a click
 * scaled by the element box lands in the ground beside the page, on a page the person cannot see
 * any other way.
 *
 * One module holds it because the panel and the self-test board need the same mapping from opposite
 * ends (one maps a press to a page point, the other maps a page point to a press), and two copies
 * of that arithmetic is how one of them ends up clicking something the other never aimed at.
 */

export type FitBox = { left: number; top: number; width: number; height: number }
export type PageSize = { width: number; height: number }

/** The rectangle the page occupies inside `box`, drawn to fit. Null when nothing can be computed. */
export function drawnRect(box: FitBox, page: PageSize): FitBox | null {
  if (!page || !page.width || !page.height) return null
  if (box.width < 8 || box.height < 8) return null
  const scale = Math.min(box.width / page.width, box.height / page.height)
  if (!(scale > 0) || !Number.isFinite(scale)) return null
  const w = page.width * scale
  const h = page.height * scale
  return { left: box.left + (box.width - w) / 2, top: box.top + (box.height - h) / 2, width: w, height: h }
}

/** The page point a press at (`clientX`, `clientY`) inside the picture stands for. */
export function pagePoint(
  box: FitBox,
  page: PageSize,
  clientX: number,
  clientY: number,
): { x: number; y: number } | null {
  const d = drawnRect(box, page)
  if (!d) return null
  const x = Math.round(((clientX - d.left) / d.width) * page.width)
  const y = Math.round(((clientY - d.top) / d.height) * page.height)
  if (x < 0 || y < 0 || x >= page.width || y >= page.height) return null
  return { x, y }
}

/** The inverse: the point in the window that a press at a page point would have to arrive at. This
 *  is what a check that aims *at* something on the page needs. */
export function picturePoint(box: FitBox, page: PageSize, pageX: number, pageY: number): { x: number; y: number } | null {
  const d = drawnRect(box, page)
  if (!d) return null
  return { x: d.left + (pageX / page.width) * d.width, y: d.top + (pageY / page.height) * d.height }
}
