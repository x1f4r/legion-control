# How the interface is drawn

Legion's interface is one set of HTML, CSS, and inline SVG shared across the phone app, tablet app,
desktop app, and browser door. Everything visual follows a small set of rules that can be checked
by measurement, not taste alone.

## Icons

All icons live as `<symbol id="i-...">` definitions in `apps/ui/icons.js` on a `24 × 24` grid.

### Optical centring

Icons are centred by what the eye sees, not by their geometric bounding box:

- The measure is the **area-weighted centroid of the drawn ink** rendered at device pixel ratio 3
  inside a `24 × 24` box in a real `.btn.icon` hit target.
- Every icon sits within **0.35 px** of the target centre. A right-pointing triangle (`play`) has
  its weight toward its base, so its bounding box sits right of centre while its ink centroid sits
  on the cross; asymmetric strokes (`back`, `actions`, `music`, `doc`, `copy`, `volume`) are placed
  by their ink centroid the same way.
- **Ring-shaped icons** (`power`, `refresh`, `sync`, `skip-back-10`, `skip-fwd-10`) are the
  exception: the ring's own centre sits on the cross (within 0.35 px), and the `"10"` inside both
  skip icons sits on that same ring centre (within 0.5 px at device pixel ratios 1, 2, and 3).

### Optical size and keylines

Every icon keeps at least **2 px of padding** on every side of the `24 × 24` box and follows one
shared set of outer ink keylines so neighbours read as one family:

| Shape | Outer ink size on the 24 grid | Examples |
| --- | --- | --- |
| Square | 18 × 18 | `machines`, `grid`, `copy`, `fullscreen`, `exit-fullscreen` |
| Circle / ring | 20 across | `gear`, `power`, `refresh`, `sync`, `skip-back-10`, `skip-fwd-10`, `star` |
| Tall rectangle | 16 × 20 | `doc`, `actions`, `bin`, `music` |
| Wide rectangle | 20 × 16 | `video`, `image`, `files`, `archive`, `overview`, `activity`, `send`, `volume` |
| Chevrons | 18 along the long axis | `back`, `down` |

`more` is three filled dots of **2 px radius**, spaced **7 px centre to centre** along the
horizontal centre line (`cx = 5, 12, 19`, `cy = 12`), so it stays legible at a glance on every
screen.

### Weight and kinds

There is one stroke weight and two kinds of glyph:

- **Outline icons:** `2 px` stroke with round caps and round joins everywhere (navigation bar,
  toolbar icon buttons, list and grid thumbnails, action bars, and fallback cards).
- **Solid transport glyphs:** filled (`fill: currentColor`, no outline) everywhere they appear —
  video player and music screen alike:
  - `play`: a filled triangle centred by ink;
  - `pause`: two filled bars, `4 px` wide and `14 px` tall, with a `4 px` gap;
  - `prev` and `next`: a filled triangle paired with a `2.5 px` bar;
  - `fast-forward`: two filled triangles.

### Player centre row

In the video player's centre transport row, icons are drawn larger so they read at arm's length:
skip-back and skip-forward sit at **28 px** inside **56 px** circles with bold `"10"` digits; play
and pause sit at **32 px** inside their main circle.

### Completeness

Every icon name requested by a view or helper must exist in `apps/ui/icons.js`. Unused symbols in
`apps/ui/icons.js` are tracked explicitly by tests so stale or mistyped names never go unnoticed.

## Buttons

### Press feedback

Every tappable control answers **within one frame** of the finger or pointer landing:

- **Motion:** scales to **0.97** immediately on `:active` (0 ms press latency) and returns smoothly
  over **120 ms** on release.
- **Surface step:** changes by a clear, theme-appropriate step while pressed:
  - primary and danger buttons darken;
  - outline and quiet buttons fill with the raised surface step;
  - icon buttons show a **40 px round highlight** centred behind the icon;
  - chips, segmented controls, tabs, filter pills, action bar buttons, swipe actions, and player
    buttons answer with the same scale and surface step.
- **Touch latency:** tappable controls carry `touch-action: manipulation` where custom pointer
  gestures do not already take `touch-action: none`.
- **Reduced motion:** under `prefers-reduced-motion: reduce`, the surface colour step stays and the
  scale transform is dropped.

### Focus rings

Keyboard focus rings (`:focus-visible`) are **2 px** in the accent colour, follow each control's
own border radius, and are never clipped by an ancestor scroll container:

- Horizontally scrolling strips (`.tabs`, `.filters`) keep enough vertical and horizontal padding
  so the full `2 px` ring and offset fit inside the strip's clipping box.
- Pill buttons (such as the resume chip's `"Start over"`) and circular icon buttons (including
  header icons and the update-note dismiss button) carry matching rounded or circular rings.
- Adjacent swipe actions draw an inset focus ring with their own radius so the ring is never cut to
  a hairline.

### Unavailable controls

A control that cannot act is either **absent** or looks unmistakably **unavailable** — never
identical to a working control:

- **Always absent when unavailable (never rendered `disabled`):** all player controls, `.chip-btn`,
  `.seg button`, `.tabs button`, `.filters button`, `.swipe-action`, and `.update-note-dismiss`.
  Tests fail if a view renders any of these with `disabled`.
- **Disabled when temporarily unavailable:** standard `.btn` variants and `.bar4 button` (including
  `.viewer-bar .bar4 button`, where `"Open in…"` disables while staging). Every disabled control
  shares the common unavailable look: **45 percent opacity**, `cursor: default`, and no press
  response.

## Checks

- `node scripts/ui-contact-sheet.mjs --check` renders every icon at 24 px inside `.btn.icon` across
  device pixel ratios 1, 2, and 3 and every button kind in resting, pressed (`:active`), focused
  (`:focus-visible`), and disabled/absent states in light and dark themes, printing bounding-box
  and DPR-3 ink-centroid offsets and failing if any icon exceeds `0.35 px`.
- Unit and browser tests verify that every icon name used in views exists in `apps/ui/icons.js`,
  that never-disabled controls are never given `disabled`, that every button kind changes computed
  style within one frame of `pointerdown`, and that focus rings on `.tabs` and `.filters` stay
  fully inside their scroll containers.
