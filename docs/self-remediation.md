# Plex self-remediation: diagnose → suggest → confirm → apply

Status: **design** (not yet implemented). Captures the abstraction the Plex
plugin uses to detect transcode-health problems and drive its *own runtime* to
fix them — without the plugin ever hard-coding a hypervisor command.

## Principle

The Plex plugin knows *what* is wrong (transcode temp saturated, orphaned
transcoders, undersized runtime RAM). It must **not** know *how* to change the
runtime (that would bake `pct set` / `qm set` into a service plugin). Per
"orca defines what, plugins define how" and "core abstractions, plugins
concrete", the plugin emits a **typed suggested action that references another
capability**, and core routes it to whichever provider owns the runtime.

## The three seams

### 1. Topology link — service → its runtime

A registered Plex endpoint must resolve to the **managed unit** that runs it,
e.g. `runtime = "<runtime-provider>:<kind>:<id>"`, discovered via the guest
services topology (`node → guest → service + endpoint`). The plugin learns its
runtime handle abstractly; it never learns "this is an LXC on hypervisor X".

This depends on modeling services inside non-peer guests in topology (today a
gap: inventory sees hosts + peers only).

### 2. `set_resources` verb on the universal-lifecycle surface

The lifecycle/managed-unit surface already carries
`start / stop / restart / update / backup / restore`, with runtime providers
registering each VM/LXC/container as a managed unit. Add one generic verb:

```
lifecycle.set_resources {
  unit:   ManagedUnitId,      // the runtime handle from seam 1
  memory: Option<u64>,        // MiB
  cores:  Option<u32>,
  disk:   Option<DiskResize>, // grow only; provider-validated
}
```

- **Core** defines the verb + typed args.
- **Each runtime provider** implements it concretely and validates against host
  capacity. The hypervisor provider maps memory→balloon set (live where the
  platform supports it), cores→vcpu (respecting the vcpus ≤ sockets×cores
  constraint), disk→grow. Always least-privilege: the scoped runtime token, not
  root.
- **Admin-gated**: mutating a runtime's resources is an admin/elevated action,
  never a default data-plane call.

### 3. `SuggestedAction` — the generic remediation contract (new, in core)

A diagnose finding may carry zero or more suggested actions. A suggested action
*references a capability call* instead of performing it:

```
SuggestedAction {
  id, title, rationale, risk,          // human-facing "why"
  target_tool: String,                 // e.g. "lifecycle.set_resources"
  target_unit: Option<ManagedUnitId>,  // what it acts on
  args: <typed>,                       // e.g. { memory: 8192 }
  requires_confirmation: bool,         // resource changes: always true
  requires_admin: bool,                // resource changes: always true
}
```

Core owns the flow so every plugin gets it for free:

```
diagnose → [SuggestedAction...] → present to user
         → user confirms (requires_confirmation)
         → authorize (requires_admin)
         → dispatch target_tool(target_unit, args) to the owning provider
         → re-run diagnose to confirm resolution
```

The plugin never dispatches the runtime change itself; it only *proposes*.

## How Plex uses it

`plex.diagnose` (extends today's `plex.transcode_health`) emits findings, each
with the right remediation tier:

| Condition | Detection | Remediation | Tier |
|---|---|---|---|
| Transcode temp dir near-full | statvfs the resolved `TranscoderTempDirectory` filesystem vs. a workload headroom threshold | remove **stale** session scratch (never a live `sessionKey`) | auto-apply (safe) |
| Orphaned/stuck transcoders | enumerate `Plex Transcoder` procs, correlate to live `sessionKey` via `-progressurl`; flag ones with no live session (don't trust process uptime in a rebuilt container) | kill orphans, reclaim their scratch | auto-apply (safe) |
| Runtime RAM too small for the transcode profile (temp dir is a RAM-backed tmpfs capped below what N concurrent 4K sessions need) | temp fs is tmpfs AND its cap is a large fraction of runtime RAM AND saturation recurs | **`SuggestedAction` → `lifecycle.set_resources { memory: +N }`** on the plugin's own runtime unit | **confirm + admin** |
| Direct-play delivery-bandwidth risk | session is `directplay`, high source bitrate, LAN client | advise client-side quality cap / wired client (no server action) | advisory only |

Note the tmpfs constraint the RAM-grow suggestion must respect: a RAM-backed
temp dir counts against runtime RAM, so the suggested new tmpfs cap must stay
comfortably below the (post-resize) runtime memory — the provider validates
against host capacity before applying.

## Build order

1. Core: `SuggestedAction` contract + confirm/authorize/dispatch flow.
2. Core: `lifecycle.set_resources` verb on the managed-unit surface.
3. Runtime provider (hypervisor plugin): implement `set_resources`
   (memory/cores/disk), admin-gated, host-capacity-validated, live where
   supported.
4. Topology: link a Plex endpoint to its managed unit (guest-services model).
5. Plex: `plex.diagnose` emits findings + tiered remediations; safe repairs
   auto-apply, RAM grow is a `SuggestedAction`.
6. Schedule `plex.diagnose` (auto-repair safe tiers) via an orca routine.
</content>
