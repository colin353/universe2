# Hermetic CBS Tool Execution Plan

## Goal

CBS should not accidentally depend on ambient host state. Build actions, resolver actions, and plugin actions should execute tools that were explicitly declared, resolved, and included in the build graph/hash. The execution environment should be narrow enough that accidental uses of `/usr/bin`, inherited `PATH`, local Cargo/Rustup state, credentials, or arbitrary environment variables fail loudly.

This plan is intentionally incremental: it should move CBS toward hermeticity without requiring a full Linux-style sandbox or chroot on every platform.

## Design principles

1. **Tools are build inputs.** If an action executes `rustc`, `cargo`, `cc`, `ar`, `tar`, `curl`, or any other executable, that executable must be represented as an explicit dependency with version/digest metadata.
2. **Plugins own their toolchains.** The Rust plugin should be responsible for resolving/downloading the Rust toolchain and related tools it needs, rather than CBS core hard-coding `rustc`, `cargo`, or host command lookup.
3. **CBS core should not search host paths.** No implicit probing of `/usr/bin`, `/bin`, `/usr/local/bin`, or inherited `PATH`.
4. **Action environments are hygienic by default.** Commands should run with `env_clear()`, action-private temp/home directories, and a synthetic `PATH` containing only declared tools.
5. **Escape hatches are explicit and noisy.** During migration, host tools can be allowed only through explicit declarations or an opt-in compatibility mode that changes action hashes and emits warnings.

## Proposed build model

### Declared tools

Add first-class tool declarations in `WORKSPACE.ccl`. Initially these can support host tools as a migration bridge:

```ccl
tools = {
    cargo = host_tool {
        path = "/path/to/cargo"
        sha256 = "..."
    }

    tar = host_tool {
        path = "/usr/bin/tar"
        sha256 = "..."
    }
}
```

Then add downloaded tools:

```ccl
tools = {
    rust = downloaded_toolchain {
        plugin = "rust"
        version = "1.XX.Y"
        target = "aarch64-apple-darwin"
        components = ["rustc", "cargo", "rust-std"]
        sha256 = "..."
    }
}
```

Long-term, tool declarations should be plugin-extensible so the Rust plugin can define Rust-specific fields and decide how to fetch/extract/verify the toolchain.

### Tools as dependencies

Extend `Config` or action metadata so actions can declare required tools separately from ordinary target deps:

```rust
pub struct Config {
    pub dependencies: Vec<String>,
    pub build_dependencies: Vec<String>,
    pub tools: Vec<String>,
    ...
}
```

CBS should include each tool descriptor and resolved digest in action hashes. If a tool changes, dependent actions rebuild.

### Running declared tools

Replace generic command execution like:

```rust
context.run_process("cargo", &args)
```

with tool-aware execution:

```rust
context.run_tool("cargo", &args)
```

The runner should:

1. Look up `cargo` in the action's declared tool map.
2. Materialize an action-local `bin/` directory of symlinks or wrapper scripts.
3. Run with `PATH` set only to that directory.
4. Reject undeclared bare command names.
5. Reject absolute paths unless they are declared tool paths or declared build outputs.

## Hygienic execution environment

For every CBS-managed action:

1. Call `Command::env_clear()`.
2. Set only known-safe variables:
   - `PATH=<action-bin-dir>`
   - `TMPDIR=<action-private-tmp>`
   - `HOME=<action-private-home>`
   - `CBS_WORKSPACE_ROOT`, if needed
   - target-specific variables explicitly provided by CBS/toolchain plugins
3. Set tool-specific state locations explicitly:
   - `CARGO_HOME=<action/toolchain cache path>`
   - `RUSTUP_HOME` only if rustup remains in use, preferably not
   - certificate variables only if a downloaded tool requires them
4. Avoid inheriting user config:
   - no ambient `.cargo/config.toml`
   - no ambient credentials
   - no ambient compiler/linker variables
   - no ambient proxy variables unless explicitly declared

On macOS this will not prevent arbitrary filesystem reads, but it prevents the most common accidental non-hermetic behavior.

## Rust plugin migration

The Rust implementation should eventually move fully behind a cdylib plugin boundary.

Current direction:

1. Keep the existing bootstrap path working.
2. Make Rust plugin own:
   - `rustc`
   - `cargo` or Cargo metadata equivalent
   - `cc`
   - `ar`
   - crate download/extract support
3. Prefer Rust libraries over host tools where reasonable:
   - HTTP downloads instead of `curl`
   - gzip/tar extraction libraries instead of `/usr/bin/tar`
   - archive writing library instead of `ar`, if feasible
4. If a native tool remains necessary, declare it as a plugin tool dependency.
5. Move built-in Rust plugin code out of CBS core into a loaded `cdylib`, while keeping a bootstrap-compatible path for building/installing CBS itself.

## Incremental implementation phases

### Phase 1: Inventory and diagnostics

- Inventory every `Command::new`, `run_process`, and path lookup in CBS, plugins, and build actions.
- Add warnings when CBS executes a bare command or searches host paths.
- Reject undeclared command execution.
- Do not provide compatibility escape hatches for non-hermetic tool use.

Status: implemented diagnostics for CBS action execution, plugin SDK command resolution, and Rust plugin process execution. Non-hermetic tool use is rejected unconditionally; the earlier `--strict-tools`, `--allow-non-hermetic-tools`, and environment-variable escape hatch were removed.

### Phase 2: Tool declarations and resolution

- Add `tools` parsing to `WORKSPACE.ccl`.
- Add `host_tool` declarations with path and optional/required SHA-256.
- Add CBS-side tool resolution APIs.
- Add tool fingerprints to context/action hashes.
- Convert `rustc` from `toolchain.rust.rustc = "rustc"` into a declared tool reference.

Status: implemented `tools` parsing, SHA-256 verification/fingerprinting, context hash inclusion, and `toolchain.rust.rustc` lookup through the declared tool map. `WORKSPACE.ccl` now imports shared CBS workspace prototypes from `//util/cbs:cbs.ccl`; generic host program lookup has been removed, and source tool declarations use explicit `rust_toolchain` / `xcode_tool` types instead of `host_tool`.

### Phase 3: Tool-aware process execution

- Add `run_tool` APIs in CBS core and plugin SDK.
- Replace `resolve_command` with declared tool lookup.
- Create synthetic per-action `bin/` directories.
- Use `env_clear()` and minimal environment for all CBS-owned process execution.
- Keep a temporary compatibility mode for undeclared host tools, but make it explicit and hash-affecting.

Status: implemented declared tool maps in CBS contexts, plugin planning/resolution contexts, and plugin build requests. CBS actions and plugin SDK calls now have `run_tool`; declared tools execute through action-local synthetic tool directories with cleared environments, private `TMPDIR`/`HOME`, and `PATH` limited to declared tools. Rust/Cargo planning plus native recipes use declared tool paths, and plugin bare-command resolution now rejects undeclared host tools rather than searching `/usr/bin`, `/bin`, or `/usr/local/bin`.

macOS compromise: `WORKSPACE.ccl` declares `platform_requirements.macos.xcode = xcode_tool { tool = "clang"; sdk = "macos" }` and platform-scoped `cc`/`ar` Xcode tools. CBS applies that requirement only on macOS, resolves Xcode through `xcrun`, detects the current Xcode developer directory and macOS SDK if paths are not specified, validates Xcode clang and `TargetConditionals.h` at workspace load time, and includes that requirement in the context hash. Missing Xcode/CLT now fails early instead of surfacing as an accidental `cc` failure later. The `target` section has been removed from `WORKSPACE.ccl`; CBS defaults target configuration from the current host platform unless a workspace explicitly overrides it.

### Phase 4: Rust/Cargo toolchain ownership

- Teach the Rust plugin to resolve/download a specified Rust toolchain.
- Move `cargo metadata` execution behind a declared `cargo` tool or replace it with plugin-owned metadata generation.
- Replace host `curl` and `tar` in crate fetching/extraction with Rust library implementations where practical.
- Make native build recipes declare `cc`/`ar` or replace `ar` with a Rust archive writer.

Status: partially implemented. Cargo crate sources are now populated through declared `cargo fetch` into CBS-owned `CARGO_HOME`, so crate fetching no longer uses host `curl` or `tar`. `WORKSPACE.ccl` now declares Rust as `rust_toolchain { version = "1.91.1"; dist = ... }`, with pinned Rust dist URLs/hashes for common macOS/Linux hosts; CBS expands that into declared `rustc` and `cargo` tools, validates the active Rust version, and fingerprints the toolchain metadata. The current provider is still a bootstrap `rustup`/current-rustc sysroot provider; actual archive download/extraction remains future work.

### Phase 5: Strict-by-default execution

- Flip the default so undeclared tools are errors.
- Make action environments hygienic by default.
- Remove host tool escape hatches.
- Run `cbs build //...` and `cbs test //...` with strict default behavior.
- Fix all remaining ambient dependencies.

Status: implemented. Undeclared/bare host tools and known host tool paths are rejected unconditionally in CBS actions, plugin SDK execution, and both Rust plugin implementations. CBS no longer has a strict-mode flag, compatibility flag, or environment-variable escape hatch. The full repository builds and tests with default strict behavior.

Ordering note: do this before the full Phase 6 plugin split, but avoid cementing Rust-specific tool schemas in CBS core while doing it. Phase 5 should focus on enforcement and escape-hatch semantics for whatever tools are already declared; Rust-specific dynamic tool definition belongs in Phase 6.

### Phase 6: Rust plugin as cdylib

- Split the Rust plugin out so CBS core does not embed Rust-specific resolver/builder logic.
- Add plugin parameters in `WORKSPACE.ccl`, e.g. `plugins = [{ name = "rust"; path = "..."; parameters = { rust_version = "1.91.2" } }]`.
- Add a plugin initialization API that receives parameters plus host platform/cache context and returns tool requirements to CBS.
- Let the Rust plugin own Rust-specific toolchain logic: mapping Rust version + current platform to dist URLs/hashes, choosing archive layout, and exposing declared `rustc`/`cargo` tools.
- Keep CBS core responsible for generic mechanics only: parameter transport, tool requirement validation, download/cache/extract primitives, hashing/fingerprinting, and exposing tools to actions.
- Move the temporary `rust_toolchain` concept out of `//util/cbs:cbs.ccl`; any Rust-specific declaration should either be internal to the Rust plugin or derived from Rust plugin parameters.
- Keep bootstrap sequencing documented:
  1. build CBS with previous compatible CBS,
  2. build Rust plugin,
  3. install CBS/plugin pair,
  4. rebuild under strict tool mode.
- Ensure CBS can still build itself without relying on undeclared host tools.

## Open questions

1. What is the bootstrap trust root for downloading toolchains: built-in HTTP client, declared host downloader, or pre-seeded cache?
2. What archive/decompression implementation should CBS use for Rust dist tarballs without reintroducing host `tar`?
3. Should plugin-provided tools be target labels, external requirements, or a third dependency class?
4. Should plugin parameters be untyped CCL values passed through to plugins, or should CBS ask each plugin for a schema and validate parameters before initialization?
5. Do we want remote/cache portability guarantees, or only local reproducibility at first?
6. Should strict mode reject all absolute paths, or allow declared immutable prefixes?
7. What Linux-native `cc`/`ar` replacement should pair with the macOS Xcode compromise?

## Immediate next steps

1. Make native recipes use exact declared `cc`/`ar` requirements per platform; on macOS this currently means the declared Xcode/SDK prerequisite, while Linux should move toward a downloaded LLVM/Zig-style toolchain.
2. Start Phase 6 by adding plugin parameters and a plugin initialization/tool-requirement API, then move Rust-specific toolchain declarations out of CBS core.
