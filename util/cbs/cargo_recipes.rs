use cbs_plugin_sdk::*;

use super::cargo::{CargoBuildRecipe, CargoNativeStaticLib};

pub fn build_recipe(
    context: &PluginContext,
    package: &str,
    version: &str,
) -> Option<CargoBuildRecipe> {
    match (package, version) {
        ("flatbuffers", "25.12.19")
        | ("generic-array", "0.14.7")
        | ("httparse", "1.8.0")
        | ("httparse", "1.10.1")
        | ("libc", "0.2.151")
        | ("libc", "0.2.186")
        | ("proc-macro2", "1.0.71")
        | ("proc-macro2", "1.0.106")
        | ("quote", "1.0.45")
        | ("rustls", "0.23.31")
        | ("serde", "1.0.193")
        | ("serde", "1.0.228")
        | ("serde_json", "1.0.117")
        | ("serde_core", "1.0.228")
        | ("slab", "0.4.9")
        | ("slab", "0.4.12")
        | ("syn", "1.0.109")
        | ("zerocopy", "0.8.48") => Some(CargoBuildRecipe::default()),
        ("indexmap", "1.9.3") => Some(CargoBuildRecipe {
            rustc_cfgs: vec!["has_std".to_string()],
            ..Default::default()
        }),
        ("ring", "0.17.14") if is_aarch64_apple(context) => Some(CargoBuildRecipe {
            native_static_libs: vec![ring_aarch64_apple_native_static_lib()],
            ..Default::default()
        }),
        _ => None,
    }
}

fn is_aarch64_apple(context: &PluginContext) -> bool {
    context.get_config(build_config_key::TARGET_FAMILY) == Some("unix")
        && context.get_config(build_config_key::TARGET_ARCH) == Some("aarch64")
        && context.get_config(build_config_key::TARGET_VENDOR) == Some("apple")
        && context.get_config(build_config_key::TARGET_ENDIAN) == Some("little")
        && matches!(
            context.get_config(build_config_key::TARGET_OS),
            Some("ios" | "macos" | "tvos" | "visionos" | "watchos")
        )
}

fn ring_aarch64_apple_native_static_lib() -> CargoNativeStaticLib {
    CargoNativeStaticLib {
        name: "ring_core_0_17_14_".to_string(),
        sources: vec![
            "crypto/curve25519/curve25519.c",
            "crypto/fipsmodule/aes/aes_nohw.c",
            "crypto/fipsmodule/bn/montgomery.c",
            "crypto/fipsmodule/bn/montgomery_inv.c",
            "crypto/fipsmodule/ec/ecp_nistz.c",
            "crypto/fipsmodule/ec/gfp_p256.c",
            "crypto/fipsmodule/ec/gfp_p384.c",
            "crypto/fipsmodule/ec/p256.c",
            "crypto/limbs/limbs.c",
            "crypto/mem.c",
            "crypto/poly1305/poly1305.c",
            "crypto/fipsmodule/ec/p256-nistz.c",
            "pregenerated/aesv8-armx-ios64.S",
            "pregenerated/aesv8-gcm-armv8-ios64.S",
            "pregenerated/ghash-neon-armv8-ios64.S",
            "pregenerated/ghashv8-armx-ios64.S",
            "pregenerated/p256-armv8-asm-ios64.S",
            "pregenerated/sha256-armv8-ios64.S",
            "pregenerated/sha512-armv8-ios64.S",
            "pregenerated/chacha-armv8-ios64.S",
            "pregenerated/chacha20_poly1305_armv8-ios64.S",
            "pregenerated/armv8-mont-ios64.S",
            "pregenerated/vpaes-armv8-ios64.S",
        ]
        .into_iter()
        .map(|source| source.to_string())
        .collect(),
        include_dirs: vec!["include".to_string(), "pregenerated".to_string()],
        flags: vec![
            "-fvisibility=hidden",
            "-std=c1x",
            "-Wall",
            "-Wbad-function-cast",
            "-Wcast-align",
            "-Wcast-qual",
            "-Wconversion",
            "-Wmissing-field-initializers",
            "-Wmissing-include-dirs",
            "-Wnested-externs",
            "-Wredundant-decls",
            "-Wshadow",
            "-Wsign-compare",
            "-Wsign-conversion",
            "-Wstrict-prototypes",
            "-Wundef",
            "-Wuninitialized",
            "-gfull",
            "-DNDEBUG",
        ]
        .into_iter()
        .map(|flag| flag.to_string())
        .collect(),
    }
}
