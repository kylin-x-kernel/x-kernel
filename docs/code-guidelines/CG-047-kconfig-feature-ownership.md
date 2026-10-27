---
id: CG-047-kconfig-feature-ownership
level: MUST
paths:
  - "Cargo.toml"
  - "**/Cargo.toml"
  - "Kconfig"
  - "Kconfig.*"
  - "**/Kconfig"
  - "**/Kconfig.*"
  - "platforms/**/*defconfig"
  - "Makefile"
  - ".cargo/*.toml"
  - "xtask/xconfig/**"
  - "xtask/xkmake/**"
  - "scripts/**"
  - "Jenkinsfile"
checks:
  - backend: ai
---

# CG-047-kconfig-feature-ownership: Keep system feature policy in Kconfig and assembly in kfeat

- **Rule**: Configurable kernel capabilities MUST take their selection,
  prerequisites, exclusions and default policy from Kconfig. `api/kfeat/Cargo.toml`
  MUST assemble the participating subsystem and platform Cargo features for each
  system capability. Ordinary crates may forward features required by their own
  implementation, but MUST NOT own system-wide selection of peer capabilities.
  Cargo defaults, dependency feature lists and build arguments MUST preserve the
  resolved Kconfig selection rather than enable a disabled system capability.
- **Scope**: Added or changed kernel feature declarations, forwarding edges,
  defaults, optional dependencies, Kconfig relationships, platform selections
  and build/configuration wiring. Changes to existing features count too.
  This rule deliberately has no keyword triggers: an edited array element or
  removed prerequisite may carry no feature-related keyword. Inspect applicable
  changed files, including deletions, before deciding whether feature wiring
  changed. [CG-035](CG-035-declared-cfgs.md) owns cfg name/value validity;
  this rule owns configuration policy and assembly boundaries.
- **Why**: Scattered system feature edges create a second configuration policy:
  a selected combination can miss required participants, or Cargo can enable a
  capability that the resolved kernel configuration disables.
- **How to check**:
  1. Classify each changed feature edge as system capability assembly, a local
     implementation dependency, or one of the exceptions below. Read the
     feature's consumers and dependencies; a `crate/feature` string alone does
     not establish a violation.
  2. Trace system capabilities from their Kconfig definitions and applicable
     defconfig through the configuration generator to `kfeat` and the consuming
     crate features. Verify that Kconfig expresses the prerequisite, exclusion
     and platform constraints. Cargo implication alone is not a substitute.
  3. Verify that `kfeat` selects the required participants together. Keep local
     implementation edges with their owner: for example, a task crate enabling
     its shared-lock dependency's SMP support is local wiring; a runtime crate
     selecting the task, RCU and platform SMP capabilities is system assembly.
  4. Trace default features, dependency `features`, optional dependency activation
     and build arguments for alternate enablement paths. Check both enabled and
     disabled configurations and relevant platform restrictions. When validating
     builds, use the configured Makefile/XKMake flow; a successful default build
     does not prove the disabled configuration is respected.
  5. Report only relationships introduced or changed by the PR. Identify the
     relevant Kconfig symbol (or its absence), `kfeat` mapping and concrete Cargo
     edge/build argument. Explain the missing participant, configuration bypass
     or misplaced system policy and where it belongs. Do not report unrelated
     historical wiring or demand that every feature get a Kconfig symbol.
- **Exceptions**: Crate-local implementation features, optional dependency
  activation and third-party library options may stay in their owning manifest
  when they do not choose an independently configurable kernel capability.
  Explicit test entry features such as `entry/unittest`, host tools, standalone
  workspaces and boot-helper implementation options need no matching Kconfig
  symbol; they must still preserve kernel configuration when building a kernel.
  Kconfig-only dependency gates such as `KFEAT_FS`, numeric settings and
  architecture/platform mapping need not have one-to-one Cargo feature names.
- **Counter-example**:

In a hypothetical runtime crate, `net` is independently configurable but this
feature selects its peer subsystem without a Kconfig prerequisite or kfeat
assembly:

```toml
[features]
packet_trace = ["knet/net", "ktracing/packet-trace"]
```

The runtime owns system policy, and Cargo can enable networking despite the
resolved configuration disabling it.

- **Example**:

Declare the prerequisite in Kconfig:

```kconfig
config KFEAT_PACKET_TRACE
    bool "Packet tracing"
    depends on KFEAT_NET
```

Assemble the participants in `api/kfeat/Cargo.toml`:

```toml
[features]
packet_trace = ["knet/packet-trace", "ktracing/packet-trace"]
```

Each participating crate declares its local implementation feature. The existing
`KFEAT_NET` selection supplies the networking assembly through `kfeat/net`.
