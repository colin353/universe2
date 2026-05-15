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
- Add a `--strict-tools` flag that fails on undeclared command execution.
- Keep default behavior compatible at first.

Status: implemented diagnostics for CBS action execution, plugin SDK command resolution, and Rust plugin process execution. `--strict-tools` is available as a global CBS flag and currently fails on undeclared bare host tools/path searches.

### Phase 2: Tool declarations and resolution

- Add `tools` parsing to `WORKSPACE.ccl`.
- Add `host_tool` declarations with path and optional/required SHA-256.
- Add CBS-side tool resolution APIs.
- Add tool fingerprints to context/action hashes.
- Convert `rustc` from `toolchain.rust.rustc = "rustc"` into a declared tool reference.

Status: implemented `tools` parsing for `host_tool` entries, SHA-256 verification/fingerprinting, context hash inclusion, and `toolchain.rust.rustc` lookup through the declared tool map. `WORKSPACE.ccl` now declares the current host `rustc` as the first transitional tool.

### Phase 3: Tool-aware process execution

- Add `run_tool` APIs in CBS core and plugin SDK.
- Replace `resolve_command` with declared tool lookup.
- Create synthetic per-action `bin/` directories.
- Use `env_clear()` and minimal environment for all CBS-owned process execution.
- Keep a temporary compatibility mode for undeclared host tools, but make it explicit and hash-affecting.

Status: implemented declared tool maps in CBS contexts, plugin planning/resolution contexts, and plugin build requests. CBS actions and plugin SDK calls now have `run_tool`; declared tools execute through action-local synthetic tool directories with cleared environments, private `TMPDIR`/`HOME`, and `PATH` limited to declared tools. `WORKSPACE.ccl` now declares the currently needed transitional host tools (`rustc`, `cargo`, `curl`, `tar`, `cc`, and `ar`) with SHA-256 fingerprints, and Rust/Cargo planning plus native recipes use the declared tool path. Compatibility fallback still exists for undeclared process execution until strict-by-default migration is complete.

### Phase 4: Rust/Cargo toolchain ownership

- Teach the Rust plugin to resolve/download a specified Rust toolchain.
- Move `cargo metadata` execution behind a declared `cargo` tool or replace it with plugin-owned metadata generation.
- Replace host `curl` and `tar` in crate fetching/extraction with Rust library implementations where practical.
- Make native build recipes declare `cc`/`ar` or replace `ar` with a Rust archive writer.

### Phase 5: Strict-by-default execution

- Flip the default so undeclared tools are errors.
- Make action environments hygienic by default.
- Require explicit opt-in for host tool escape hatches.
- Run `cbs build //...` and `cbs test //...` under strict mode.
- Fix all remaining ambient dependencies.

### Phase 6: Rust plugin as cdylib

- Split the Rust plugin out so CBS core does not embed Rust-specific resolver/builder logic.
- Keep bootstrap sequencing documented:
  1. build CBS with previous compatible CBS,
  2. build Rust plugin,
  3. install CBS/plugin pair,
  4. rebuild under strict tool mode.
- Ensure CBS can still build itself without relying on undeclared host tools.

## Open questions

1. What is the bootstrap trust root for downloading toolchains: built-in HTTP client, declared host downloader, or pre-seeded cache?
2. Should `host_tool` require SHA-256 immediately, or allow a warning-only mode during migration?
3. Should tools be target labels, external requirements, or a third dependency class?
4. How should plugin-defined tool schemas be validated from `WORKSPACE.ccl`?
5. Do we want remote/cache portability guarantees, or only local reproducibility at first?
6. Should strict mode reject all absolute paths, or allow declared immutable prefixes?
7. How should platform-specific tools be selected for macOS vs Linux?

## Immediate next steps

1. Move the transitional `host_tool` Rust entries into a Rust-owned downloaded toolchain.
2. Replace host `curl`/`tar` with Rust library implementations or Rust-plugin-owned downloaded tools.
3. Make native recipes declare exact `cc`/`ar` requirements per platform, or replace `ar` with a Rust archive writer.
4. Expand strict-mode coverage from smoke targets to the full repository after remaining bootstrap/toolchain ownership work.
