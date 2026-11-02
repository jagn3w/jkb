//! The non-intersection checks lifecycle-layout.test.mjs and lifecycle-smoke.test.mjs share: the
//! one over the layout's own boxes, the other over the rendered elements' boxes.

/** Whether two boxes (`{x, y, w, h}`, top-left) share area; touching edges do not. */
export function intersects(a, b) {
  const e = 0.01;
  return a.x + e < b.x + b.w && b.x + e < a.x + a.w && a.y + e < b.y + b.h && b.y + e < a.y + a.h;
}

/** Every pair of distinct boxes from `as` and `bs` (each with an `id`) that intersect, by id. */
export function overlaps(as, bs) {
  const out = [];
  for (const a of as) {
    for (const b of bs) {
      if (a === b || (as === bs && out.some(([x, y]) => x === b.id && y === a.id))) continue;
      if (intersects(a, b)) out.push([a.id, b.id]);
    }
  }
  return out;
}
