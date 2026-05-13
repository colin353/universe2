# universe2 build guide

This workspace is built with CBS, a small experimental build system. CBS is intentionally not Cargo, Bazel, or Make: targets are declared in `BUILD.ccl`, workspace settings live in `WORKSPACE.ccl`, and Rust code is compiled by invoking `rustc` directly through CBS plugins.

## Quick start

From the workspace root:

```sh
cd /Users/colinwm/Documents/code/universe2
cbs build //util/cbs:cbs
cbs test //util/...
cbs run //util/bus:busfmt -- --help
```

CBS discovers the workspace by walking up from the current directory until it finds `WORKSPACE.ccl`.

## Required plugins

CBS loads build rules from dynamic plugins:

- The Rust plugin is implicit and is loaded from `/tmp/rust.cdylib`.
- The Bus plugin is configured in `WORKSPACE.ccl` and is loaded from `/tmp/bus.cdylib`.

If a plugin is stale, rebuild and copy it into place:

```sh
cd /Users/colinwm/Documents/code/universe2

cp "$(cbs build //util/cbs_rust_plugin:cbs_rust_plugin | tail -1)" /tmp/rust.cdylib
cp "$(cbs build //util/bus:bus_plugin | tail -1)" /tmp/bus.cdylib
```

These refresh commands require an already-working CBS/plugin pair. A fresh checkout needs bootstrap plugin artifacts supplied externally before CBS can load the workspace.

The installed `cbs` binary can also rebuild itself:

```sh
cbs install //util/cbs:cbs
```

`install` writes the built executable to `~/bin`, replacing an existing file with the same name. CBS warns if `~/bin` is not on `PATH`.

## Common commands

```sh
cbs build <target-or-pattern>...
cbs test <target-or-pattern>...
cbs run <target> [-- args...]
cbs install <target>
```

`build` accepts one or more targets or recursive patterns. It expands all requested targets first, constructs one combined build graph, builds it, and prints output paths on success.

`test` also accepts targets or recursive patterns, but it only builds targets marked as tests by plugins. For Rust, that means targets with `_type = "rust_test"`. Regular libraries and binaries matched by a pattern are ignored. Each test target is compiled as an executable with Rust's `--test` mode and passes when the executable exits with status 0. Failing test logs are printed.

`run` builds one target and executes its first output. Arguments after `--` are passed to the executable.

`install` builds one executable target and copies it to `~/bin/<output-name>`.

## Target labels and patterns

CBS labels look like:

```text
//package/path:target_name
:target_in_current_package
//package/path/...
//...
```

Examples:

```sh
cbs build //util/flags:flags
cbs test //util/bus/...
cbs build //util/cbs:cbs //util/bus:busfmt
```

The recursive `...` form scans packages below that directory for `BUILD.ccl` files and expands to matching target kinds.

## Workspace configuration

`WORKSPACE.ccl` configures the cache, toolchain, plugins, and target platform:

```ccl
workspace = {
    cache_dir = ".cbs/cache"
}

toolchain = {
    rust = {
        rustc = "rustc"
    }
}

plugins = [
    {
        name = "bus"
        path = "/tmp/bus.cdylib"
    },
]

target = {
    family = "unix"
    os = "macos"
    env = ""
    arch = "aarch64"
    vendor = "apple"
    endian = "little"
}
```

`cache_dir` is where CBS stores resolved external dependencies and build outputs. The Rust plugin path is currently implicit as `/tmp/rust.cdylib`; additional plugins are listed in `plugins`.

## BUILD.ccl files

Each package may contain a `BUILD.ccl`. Targets are top-level CCL bindings whose values are rule prototype expansions. Import the shared CBS rule prototypes from `//util/cbs:build_defs.ccl`.

Source paths are package-relative. Do not use absolute paths or `..` in source paths.

### Rust libraries

```ccl
import { rust_library } from "//util/cbs:build_defs.ccl"

flags = rust_library {
    srcs = [
        "lib.rs",
        "parse.rs",
    ]
}
```

By default a library uses `lib.rs` or `<name>.rs` as the crate root. Use `root_source` when the root is different.

Optional fields:

- `crate_name`: override the Rust crate name.
- `crate_type`: override the crate type, for example `"rlib"`, `"cdylib"`, or `"proc-macro"`.
- `deps`: CBS target dependencies.
- `cargo_deps`: external Cargo dependencies.

### Rust binaries

```ccl
import { rust_binary } from "//util/cbs:build_defs.ccl"

busfmt = rust_binary {
    srcs = ["busfmt.rs"]
    deps = [
        ":parser",
        ":fmt",
        "//util/flags:flags",
    ]
}
```

By default a binary uses `main.rs` or `<name>.rs` as the root source. `root_source` can override this.

### Rust tests

```ccl
import { rust_test } from "//util/cbs:build_defs.ccl"

parser_test = rust_test {
    root_source = "parser.rs"
    srcs = [
        "parser.rs",
        "ast.rs",
    ]
    deps = ["//util/ggen:ggen"]
}
```

Rust tests are compiled with `rustc --test`, so normal `#[test]` functions are discovered and run by the produced executable.

### Cargo dependencies

Use `cargo_deps` for crates from crates.io:

```ccl
cargo_deps = [
    {
        package = "tokio"
        version = "=1.48.0"
        default_features = false
        features = ["macros", "rt-multi-thread"]
    },
    {
        package = "serde_json"
        version = "=1.0.117"
    },
]
```

Fields:

- `package`: Cargo package name.
- `version`: version requirement. Current examples generally pin exact versions with `=`.
- `default_features` or `default-features`: defaults to `true`.
- `features`: Cargo features to enable.
- `target`: optional explicit target label. By default CBS uses `cargo://<package>`.

CBS plans Cargo dependencies for the whole build invocation, then resolves `cargo://...` targets through the Rust plugin. Cargo `build.rs` scripts are not executed unless the Rust plugin has a hermetic recipe for that crate.

### Bus libraries

The Bus plugin adds `rust_bus_library` for `.bus` schemas:

```ccl
import { rust_bus_library } from "//util/cbs:build_defs.ccl"

fortune_bus = rust_bus_library {
    srcs = ["fortune.bus"]
}
```

This rule expects exactly one `.bus` source. It runs the Bus code generator and produces a Rust library. By default it depends on `//util/bus:bus` and `//util/bus/codegen:codegen`; these can be overridden with `bus_runtime` and `codegen` label fields when needed.

## Examples in this workspace

- `//util/flags:flags`: small Rust library.
- `//util/flags:flags_test`: Rust test target.
- `//util/bus:busfmt`: Rust binary.
- `//util/bus:bus_plugin`: Bus CBS plugin built as a Rust `cdylib`.
- `//util/cbs_rust_plugin:cbs_rust_plugin`: Rust CBS plugin built as a Rust `cdylib`.
- `//util/cbs:cbs`: CBS built by CBS.

## Notes and limitations

CBS is still evolving. Important current behaviors:

- Build outputs are content-addressed under `.cbs/cache/build`.
- External Cargo crates are resolved under `.cbs/cache/resolve`.
- Rust compilation is direct `rustc` invocation, not `cargo build`.
- Plugin ABI support is part of the build system: plugins can define rule kinds, test rule kinds, dependency planners, and target resolvers.
- Production CBS requires dynamic plugins; test-only in-process fallbacks are only for the CBS test suite.
