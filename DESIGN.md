# Pravera Design System

Source of truth is `crates/pravera-ui/src/theme/tokens.rs`. This document explains the reasoning;
the code holds the values. No literal colour, spacing, or size appears anywhere else in the UI crate.

## Theme: dark, and why

> Someone at their desk after midnight with a cable running between two laptops, glancing at this
> window to confirm the link came up, seconds before the same window fills with a 4K remote desktop
> stream.

Dark is forced by the second half of that sentence, not by the category. This chrome surrounds live
video. A light frame around a dark game render is a glare sandwich, and the eye adaptation cost is
paid on every glance away from the stream. There is no light theme and there should not be one.

## Colour strategy: restrained

Pure Tailwind `neutral`, per the brief. The greys are deliberately untinted: this interface sits
beside arbitrary remote content, and a colour cast in the chrome would misrepresent the colours in
the stream next to it.

Neutrals carry roughly 95% of the surface. Colour is spent in exactly one place: **path quality.**

| Role | Token | Meaning |
|---|---|---|
| Direct | `ROUTE_DIRECT` `#16a34a` | Cable, LAN, or hole-punched. The bits go straight there. |
| Relayed | `ROUTE_RELAY` `#d97706` | Riding a DERP or iroh relay. Still encrypted, measurably slower. |
| Offline | `ROUTE_OFFLINE` `NEUTRAL_600` | Known but unreachable. |
| Destructive | `DESTRUCTIVE` `#dc2626` | Close, disconnect, revoke. Nothing else. |

Green and amber are never decorative and never appear on anything that is not a path. If a future
surface wants a colour for emphasis, the answer is weight or size, not hue.

## Typography

One face: **JetBrains Mono**, embedded in the binary (`FONT_BYTES`) so the interface looks the same
on a machine that has never heard of it. It is the default font, so a widget that forgets to set one
still lands on it.

The tokens say what a string *is* rather than which file it comes from:

- `FONT_UI`, `FONT_UI_MEDIUM`, `FONT_UI_STRONG` (Regular 400, Medium 500, Semibold 600) for prose,
  names, labels and anything a human wrote.
- `FONT_MONO`, `FONT_MONO_STRONG` for everything the machine knows: addresses, device IDs,
  interface names, figures, latencies, and every uppercase eyebrow label. They resolve to the same
  face as the UI weights; they are separate tokens so a call site still declares its intent, and so
  the machine's voice can be split off again without touching the screens.

Making the whole interface mono is the point, not a compromise. This is a product about network
paths, and its native vernacular is fixed-width technical strings. When the chrome speaks in the
machine's own voice, an address, a figure and a page title sit on one grid, and nothing about the
type could be lifted onto a different product. Hierarchy comes from size and weight, never from
switching families.

Eyebrow labels are uppercase (`tokens::tracked`, which only upper-cases: a monospaced face already
sets capitals evenly), at `TEXT_2XS`. They are structural: they name a region, and they never
restate a heading.

Scale: 10 / 11 / 13 / 14 / 16 / 20 / 26. Page titles are `TEXT_LG` in the header and section titles
inside a page are the same; `TEXT_XL` and above are for the rare hero.

### How weights are registered

The font file is variable, so it registers once, at its default weight of 400. cosmic-text only
takes a requested family's own face when the registered weight is exactly the one asked for;
otherwise it tries its fallback list first, and on Windows that list starts with Segoe UI, which
has no Medium. Nothing logs a warning: labels just quietly render in a different family (once, a
serif). So `tokens::font_at` registers the same bytes again with the OS/2 weight class rewritten to
500 and 600, which gives each weight an exact match in its own family while the glyphs still come
from the `wght` axis at render time. `Weight::Bold` is not registered and must not be requested;
`tokens::tests::every_weight_the_interface_uses_has_a_face_that_claims_it` fails the build if a
token asks for a weight with no face behind it.

## Elevation

Border and background only. No drop shadows anywhere.

Depth is expressed as a step up the neutral scale: `BACKGROUND` (950) → `CARD` (900) →
`SECONDARY` (800). A single hairline in `BORDER` (800) separates regions. Shadows would read as
lighting, and there is no light source in this interface.

## Motion

`crates/pravera-ui/src/motion.rs` is the source of truth for every number below; this section says
what they are for. Keeping the set small is what makes the app feel like one object rather than a
pile of separately-tuned widgets.

| Token | Duration | Easing | Used for |
|---|---|---|---|
| `MICRO` | 120ms | ease-out-quart | hover, press, focus, a page's contents leaving, a dialog or menu leaving |
| `STANDARD` | 200ms | ease-out-cubic | a value moving between two resting states: switches, selection, the rail's highlight, a segmented control's thumb, a nav entry |
| `ENTRANCE` | 320ms | ease-out-quint | anything arriving: a page's header and blocks, a dialog and its scrim |
| `MENU_IN` | 220ms | ease-out-quint | a context menu arriving (a glance, so shorter than a dialog) |

Leaving uses the exit curve, ease-in-cubic, and is never longer than `MICRO`: nobody is waiting to
watch something go. Arrivals and changes decelerate, so a thing responding to input starts at once
and settles. Nothing bounces and nothing overshoots.

**Page changes.** The sheet is chrome, so it never moves or fades; only what is on it changes. The
old contents fade out in place over `MICRO` with no travel. The new header and body then rise 6px
(`PAGE_RISE`) and fade in over `ENTRANCE`, the header first and each block of the body 22ms after
the one above it (`STAGGER_STEP`), the rows of a list on the same beat. The stagger is capped at
220ms (`STAGGER_MAX`), so a long list is still one arrival and not a queue.

**The rail** has one highlight that slides to the active entry over `STANDARD`, at a steady pace
however far the entry is. Entries grow into pills under the pointer as before.

**Segmented controls** have one tile that slides under the chosen cell over `STANDARD`; the label
of each cell is lit by how much of the tile is under it.

**Dialogs** arrive over `ENTRANCE` from 97% scale and transparent, with the scrim fading in beside
them, and leave over `MICRO` the same way back.

**Buttons** ease between their resting and hovered looks over `MICRO` (`widget::glide`), which
keeps its own hover in the widget tree. A press answers at once.

iced 0.14 renders reactively, so animation requires holding a `window::frames()` subscription while
something is in flight. Each screen owns its animations and answers `is_animating(now)`; the
application subscribes only while some answer is yes. A button's own hover asks for redraws only
for the 120ms it takes. Nothing animates by accident and nothing spins the GPU at rest.

## The page

Every page is one sheet: a single bevelled panel (`RADIUS_LG`, `CARD`, `BEVEL_CARD`) filling the
window to the right of the rail, which sits on the window floor. The header is the top region of
the sheet, 56px tall, with the title at `TEXT_LG` in `FONT_UI_STRONG`, what the page is showing at a
glance beside it and the page's actions on the right. A hairline splits it from the body, and the
body scrolls inside the sheet. The sheet and that hairline belong to the shell, not to the page:
changing page changes what is on the sheet and never the sheet.

Anything else a page needs is another region of the same sheet, split by a hairline: a second pane
is left, vrule, right (`page_split`); a status strip or a ledger is a footer under its own hairline
(`page_footed`). A page never sets a card inside its sheet, so the header, the panes and the footer
are separated by rules and space, not by boxes.

## Icons

Hand-authored SVG on a 16x16 grid. 1.5px stroke, round caps, round joins, no fills, no two-tone.
Window controls are the exception at 10x10 with a 1px stroke, matching platform convention that
chrome glyphs are hairline.

Unicode glyphs are banned. They vary by installed font, refuse to align on a shared baseline, and
render at whatever weight the fallback font happens to have.

## The discovery panel

Below the device list, a four-row readout of every discovery source and what it found. It answers
the question the list above cannot: why a machine you expected is not in it. Three of the four
sources can be off, unconfigured, or unwritten, and without this a missing peer is
indistinguishable from a broken app.

It is also where the interface admits what is unbuilt. A source arriving in a later phase names the
phase rather than sitting greyed out and unexplained.

## Signature: the route meter

Each device row carries a small drawn diagram of how that peer is actually reached. Two endpoint
nodes and the path between them, drawn differently per route class: a short heavy bar for a cable,
a plain line for LAN, a line with a midpoint node for a hole-punched tunnel, a line detouring up
through a third node for a relay, a broken line for offline.

It is drawn from real discovery data (`CurAddr` versus `Relay` in Tailscale's status, interface
classification for direct links), never invented. It is the one element of this interface that
exists nowhere else, and it exists because the path is the product.

## Rules

- No drop shadows.
- No unicode glyphs as icons.
- No colour outside the route palette and `DESTRUCTIVE`.
- No bordered metric tiles. Figures go in a hairline-separated ledger, not in boxes.
- No nested containers. If something is already on `CARD`, its children do not get their own card.
- No em dashes in interface copy.
- Every interactive surface defines hover, press, and disabled. Instant state changes are a bug.
