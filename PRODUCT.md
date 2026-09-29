# Pravera

register: product

## What it is

A native, peer-to-peer, end-to-end encrypted remote desktop application. Written entirely in Rust,
host and client, for Windows and Linux. Fast enough to game on: the target is sub-20ms glass-to-glass
over a direct cable link.

## Users

One technical person managing their own machines. They run Tailscale. They own more than one
computer and physically move between them. They will plug a cable between two laptops and expect
software to notice. They read `ip addr` output for fun and they can tell the difference between 8ms
and 30ms by feel.

There is no second persona. This is not a fleet-management tool, there is no admin buying it for a
team, and nothing in the interface should be shaped by an imaginary enterprise buyer.

## The job each surface does

| Surface | The one question it answers |
|---|---|
| Devices | Which of my machines can I reach right now, and how good is the path? |
| Connect | Get me onto a machine that is not in the list. |
| Session | Nothing. Get out of the way of the pixels. |
| Users | Who may do what on this machine when they connect to it? |
| Transfers | Is the file moving, and how much longer? |
| Settings | What is this machine's identity and is the service actually running? |

## Tone

Instrument, not assistant. The interface reports; it does not reassure, congratulate, or apologise.

- State what is true. "No devices found" beats "Looks like there is nothing here yet."
- Never claim a measurement that has not been taken. An unknown latency renders as nothing, never
  as a plausible number.
- Never imply a security property the code does not enforce. Being on the tailnet is reachability,
  not authentication, and the interface must never blur that.
- Errors say what happened and what to do. They do not say sorry.
- Empty states are invitations with a real action attached.

## Anti-references

- **TeamViewer.** Toolbars stacked on toolbars, a commercial nag in every corner, chrome that
  competes with the remote screen.
- **Windows Remote Desktop.** Honest but inert. No sense of the link, no sense of quality, nothing
  that tells you why today feels worse than yesterday.
- **Generic SaaS dashboards.** Four bordered tiles with a big number and a small label across the
  top of every page. Pravera has real telemetry to show and should show it as instrumentation, not
  as a metrics grid.

## Strategic principles

1. **The path is the product.** Every other remote desktop tool hides how the connection is made.
   Pravera's differentiator is that it knows whether you are on a cable, a LAN, a hole-punched
   WireGuard tunnel, or a relay in Frankfurt, and it shows you. Route quality is the primary visual
   variable and the only place colour is spent.

2. **Chrome yields to content.** This window will be filled by a remote desktop stream. Every
   surface is designed to sit next to live video without glare, colour cast, or competing motion.

3. **Honest by construction.** Discovery establishes reachability, never identity. Permissions are
   enforced host-side. The interface never displays a device ID that has not been proven by key
   verification, and never shows a figure it did not measure.

4. **Dense over spacious.** The user is technical and wants the whole picture without scrolling.
   Information density is a feature, not a compromise.

5. **Motion means something.** Animation communicates state change and spatial relationship. It is
   never decorative and never blocks input.
