# ADR 0001 — Web terminal bridge: hosting topology, transport, and gossip freshness

- Status: Accepted
- Date: 2026-06-15
- Issues: #131 (this work), #109 (parent), follow-ups #147–#151
- Decision owner: human; advised by a parallel review panel (herdr-protocol,
  fleet-topology, security, Rust-packaging) + a post-implementation review round.

This is herdr's first ADR; it seeds `docs/adr/` with sequential numbering.

## Context

We want a phone (browser) to view the herdr fleet via a web terminal served
over the tailnet, hosted on an always-on machine (e.g. sage) so it works even
when the laptop is off. An MVP existed out-of-tree (g-fleet `pkgs/herdr-web/`:
an axum + portable-pty WS bridge spawning a `herdr` client with
`HERDR_RENDER_ENCODING=terminal-ansi`). The decision was where this belongs and
how it should be shaped, given herdr's existing client/server/gossip design.

Key facts established during the spike (file:line in the issues):

- A headless `herdr server` participates **fully** in fleet gossip with no TUI
  and no client attached; the gossip loop lives in `App` (inside the server),
  not in the interface client. Clients never gossip.
- The interface client is a thin paint-only client; in `terminal-ansi` mode the
  server pre-diffs and the client is a stdout passthrough — exactly what xterm.js
  consumes.
- Fleet gossip is **pull, not push**: a 15s `peer-summary-tick` SSHes each
  `[[peers]]` for its summary (`PEER_POLL_INTERVAL_SECS = 15`). There is no
  push/broadcast between servers. A client's attached server streams that
  server's *own* state live (render frames); only *other* hosts' state is on the
  15s cadence.

## Decision

1. **Hosting topology — the headless node is the fleet member; the web bridge is
   just another client.** On an always-on host, run one persistent `herdr server`
   daemon (it owns the gossip). `herdr web` spawns a `herdr` client per WS that
   attaches to that daemon. A phone over `tailscale serve` therefore shares that
   node's live session *and* its fleet view, independently of the laptop. The TUI,
   if also run there, attaches to the same server as a peer client.

2. **Server-attach model — persistent, not ephemeral-per-WS.** Ephemeral
   per-connection servers fork the gossip loop (N tabs ⇒ N SSH poll storms), kill
   sessions on disconnect, and fake `FleetSnapshot` freshness. Rejected.

3. **Transport — PTY subprocess for v1.** The bridge spawns `herdr client` in a
   PTY (`terminal-ansi`) and pumps bytes. This inherits handshake, reconnect,
   fleet-snapshot carry, and server-switch logic for free. A native
   `herdr-client.sock` bincode bridge or a `--stdio` no-PTY mode is deferred
   (#150) — it is what unblocks structured features (#128/#129/#130) and removes
   the PTY-per-connection ceiling, but it reimplements non-trivial client logic.

4. **15s gossip cadence accepted for v1.** The phone's view of its *attached*
   node is instant (render stream); only cross-fleet rows (e.g. "is the laptop
   up") lag ≤15s + staleness, which is fine for a glance. A faster poll or
   push-on-change is a fleet-wide change with real blast radius and is explicitly
   out of scope; if needed, the smallest correct step is a client-initiated peer
   refresh on tab focus, not changing `PEER_POLL_INTERVAL_SECS`.

5. **Packaging — in-tree, behind a cargo feature.** A `herdr web` subcommand
   gated by `--features web` keeps axum/futures-util/rust-embed/anyhow and the
   tokio net/io-util drivers out of the default build; dispatch is routed through
   `cli::maybe_run` so a non-web build prints how to enable it (single cfg site).
   Frontend assets (index.html + vendored xterm) are embedded via rust-embed; no
   `--static-dir`.

6. **Security boundary — loopback bind + `tailscale serve`, with self-defence.**
   v1 binds loopback only and is fronted by `tailscale serve` (tailnet identity).
   Three guards back this up: refuse a non-loopback bind, refuse to start under
   active `tailscale funnel` (which would publish a public shell), and a
   same-origin check on the WS upgrade (CSWSH). The spawned client starts from a
   clean herdr environment (inherited `HERDR_*` stripped) so it isn't killed by
   the nested-launch guard and doesn't resume stale leg state. Tailscale-identity
   allow-listing (#147) and idle-timeout/session-cap (#148) are deferred P1s.

## Alternatives considered

- **Ephemeral per-WS server** — rejected (see decision 2).
- **Native socket client for v1** — rejected for v1; deferred to #150. Too much
  reimplemented client logic for the initial cut.
- **Faster poll / push-on-change gossip to make the phone real-time** — rejected
  for this work; fleet-wide blast radius, separate concern.
- **Keeping the bridge in g-fleet** — rejected; it is product, entirely coupled
  to herdr's `terminal-ansi` output, and belongs versioned with herdr. g-fleet
  keeps only `enable + tailscale serve` (#149).

## Consequences

- Any host can opt into `herdr web`; g-fleet just picks which and fronts it.
- The phone shares live sessions with the desktop (multi-client) and sees fleet
  state at the attached node's gossip cadence.
- The PTY-per-connection ceiling and the structured-frame features remain until
  the v2 transport (#150) lands.
- A persistent `herdr server` daemon must actually run on the host for the
  persistent-attach guarantee; otherwise the bridge's client auto-spawns one
  (#149 deploys the daemon).

## References

- Issues: #131 (spike addendum + acceptance), #109 (parent), #147 (identity
  allow-list), #148 (idle timeout/cap), #149 (g-fleet retirement + daemon),
  #150 (native transport v2), #151 (`--session` flag), #128/#129/#130 (web UI).
- Implementation: `src/web/mod.rs`, dispatch in `src/cli.rs` + `src/main.rs`,
  assets in `assets/web/`.
