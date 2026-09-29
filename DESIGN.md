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

Two faces, with a deliberate split that comes from the subject matter.

- **UI sans** (`FONT_UI`, `FONT_UI_STRONG`) for prose, names, and anything a human wrote.
- **Mono** (`FONT_MONO`, `FONT_MONO_STRONG`) for everything the machine knows: addresses, device IDs, interface names,
  figures, latencies, and every uppercase eyebrow label.

The second half of that rule is the point. This is a product about network paths, and its native
vernacular is fixed-width technical strings. Promoting mono from "the font for code" to "the voice
of the machine" gives the interface a character that costs no extra font loading and could not be
lifted onto a different product.

Eyebrow labels are uppercase mono, letterspaced with hair spaces (iced has no tracking property),
at `TEXT_2XS`. They are structural: they name a region, and they never restate a heading.

Scale: 11 / 12 / 14 / 16 / 20 / 24 / 32. Steps are at least 1.2x apart.

### Only Normal, Semibold and Bold

`Weight::Medium` is banned, and the reason is a trap worth writing down.

Windows ships Segoe UI in Light, Semilight, Regular, Semibold, Bold and Black. There is no Medium.
When cosmic-text cannot satisfy a weight request it falls back to a different **family**, not to a
neighbouring weight of the same one, so asking for a medium-weight sans on Windows silently returns
a **serif**. Nothing logs a warning; labels just quietly render in the wrong typeface. It was in the
build for an hour before a magnified screenshot caught it.

Only weights every platform reliably carries are used, and
`tokens::tests::no_token_asks_for_a_weight_that_may_not_exist` fails the build if that slips.

Bundling Inter and selecting it by name would remove the platform dependency entirely and give
Windows and Linux identical typography. That is the right permanent fix and is not done yet.

## Elevation

Border and background only. No drop shadows anywhere.

Depth is expressed as a step up the neutral scale: `BACKGROUND` (950) → `CARD` (900) →
`SECONDARY` (800). A single hairline in `BORDER` (800) separates regions. Shadows would read as
lighting, and there is no light source in this interface.

## Motion

Every animation is one of three tiers. Keeping the set this small is what makes the app feel like
one object rather than a pile of separately-tuned widgets.

| Tier | Duration | Easing | Used for |
|---|---|---|---|
| micro | 120ms | `EaseOutQuart` | hover, press, focus |
| standard | 220ms | `EaseOutQuint` | state change, selection, badge |
| entrance | 340ms | `EaseOutExpo` | screen, list item, banner |

All three decelerate. Nothing bounces, nothing overshoots, nothing eases in: an interface element
responding to a pointer should start immediately and settle, because the user's input already
supplied the acceleration.

List entrances stagger by 22ms per item, clamped at 220ms total, so a long list still reads as one
arrival rather than a queue.

iced 0.14 renders reactively, so animation requires holding a `window::frames()` subscription while
something is in flight. Each screen owns its animations and answers `is_animating(now)`; the
application subscribes only while some answer is yes. Nothing animates by accident and nothing
spins the GPU at rest.

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
