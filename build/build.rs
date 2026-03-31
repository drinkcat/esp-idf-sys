use std::collections::HashMap;
use std::iter::once;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::*;
use common::*;
use embuild::bindgen::types::callbacks::{IntKind, ParseCallbacks, Token, TokenKind};
use embuild::bindgen::BindgenExt;
use embuild::utils::OsStrExt;
use embuild::{bindgen as bindgen_utils, build, cargo, kconfig, path_buf};

mod common;
mod config;

// Features `native` and `pio` control whether the build is performed using the "native" ESP IDF CMake-based build,
// or via the PlatformIO `espressif32` module. They work as follows:
// - If neither the `native` nor the `pio` feature is specified, native build would be used
// - If boththe  `native` and `pio` features are specified, native build would be used as well
// - Otherwise, either native or PlatformIO build would be used, depending on which feature is specified
//
// The sole reason why the `native` feature exists in the first place is so that if somebody uses `cargo check --all-features`
// (might happen due to VSCode Rust Analyzer default settings) native build to still be used in that case.
#[cfg(any(feature = "native", not(feature = "pio")))]
mod native;
#[cfg(all(not(feature = "native"), feature = "pio"))]
mod pio;

#[cfg(any(feature = "native", not(feature = "pio")))]
use native as build_driver;
#[cfg(all(not(feature = "native"), feature = "pio"))]
use pio as build_driver;

#[derive(Debug, Default)]
struct BindgenCallbacks {
    // Used to track macro "types", see modify_macro below.
    macro_types: Mutex<HashMap<String, &'static str>>,
}

// C types allowed to appear as casts in PSA_* macros; their casts will be stripped so bindgen
// can evaluate the macro as a plain integer constant.
const ALLOWED_PSA_TYPES: &[&str] = &[
    "psa_algorithm_t",
    "psa_crypto_local_input_t",
    "psa_crypto_local_output_t",
    "psa_crypto_transaction_type_t",
    "psa_dh_family_t",
    "psa_driver_get_entropy_flags_t",
    "psa_ecc_family_t",
    "psa_handle_t",
    // Only used once to define PSA_KEY_BITS_TOO_LARGE = -1, which breaks logic below.
    //"psa_key_bits_t",
    "psa_key_derivation_step_t",
    "psa_key_id_t",
    "psa_key_lifetime_t",
    "psa_key_location_t",
    "psa_key_persistence_t",
    "psa_key_type_t",
    "psa_key_usage_t",
    "psa_pake_primitive_type_t",
    "psa_pake_role_t",
    "psa_pake_step_t",
    "psa_status_t",
];

impl ParseCallbacks for BindgenCallbacks {
    fn modify_macro(&self, name: &str, tokens: &mut Vec<Token>) {
        // Many PSA_ macros are defined as ((psa_type_t)-1234), which bindgen can't evaluate.
        // See https://github.com/rust-lang/rust-bindgen/issues/316 for context.
        // Strip the cast if the type is in ALLOWED_PSA_TYPES, leaving just the numeric literal.
        // This should be reasonably safe as we allowlist types, and if removing (type_t) still
        // doesn't result in a valid integer, bindgen will just skip the macro as usual.
        if name.starts_with("PSA_") {
            // Token stream: '(' '(' '<type>' ')' <literal> ')'
            // Remove the inner cast '(' '<type>' ')', keeping outer parens and value.
            if let Some(i) = tokens.windows(3).position(|w| {
                w[0] == Token::from((TokenKind::Punctuation, b"(" as &[u8]))
                    && w[1].kind == TokenKind::Identifier
                    && w[2] == Token::from((TokenKind::Punctuation, b")" as &[u8]))
            }) {
                if let Some(type_name) = ALLOWED_PSA_TYPES
                    .iter()
                    .find(|&&t| t.as_bytes() == &*tokens[i + 1].raw)
                {
                    self.macro_types
                        .lock()
                        .unwrap()
                        .insert(name.to_string(), *type_name);
                    tokens.drain(i..i + 3);
                }
            }
        }
    }

    fn int_macro(&self, name: &str, _value: i64) -> Option<IntKind> {
        if name.starts_with("PSA_") {
            // Look for the type in the hashmap, we only need it once so we can remove it.
            if let Some(type_name) = self.macro_types.lock().unwrap().remove(name) {
                return Some(IntKind::Custom {
                    name: type_name,
                    is_signed: true,
                });
            }
        }

        // Make sure the ESP_ERR_*, ESP_OK and ESP_FAIL macros are all i32.
        const PREFIX: &str = "ESP_";
        const SUFFIX: &str = "ERR_";
        const SUFFIX_SPECIAL: [&str; 2] = ["OK", "FAIL"];

        let name = name.strip_prefix(PREFIX)?;
        if name.starts_with(SUFFIX) || SUFFIX_SPECIAL.contains(&name) {
            Some(IntKind::I32)
        } else {
            None
        }
    }
}

fn main() -> anyhow::Result<()> {
    let build_output = build_driver::build()?;

    // We need to restrict the kconfig parameters which are turned into rustc cfg items
    // because otherwise we would be hitting rustc command line restrictions on Windows
    //
    // For now, we take all tristate parameters which are set to true, as well as a few
    // selected string ones, as per below
    //
    // This might change in future
    let kconfig_str_allow = regex::Regex::new(r"IDF_TARGET")?;

    let cfg_args = build::CfgArgs {
        args: build_output
            .kconfig_args
            .filter(|(key, value)| {
                matches!(value, kconfig::Value::Tristate(kconfig::Tristate::True))
                    || kconfig_str_allow.is_match(key)
            })
            .filter_map(|(key, value)| value.to_rustc_cfg("esp_idf", key))
            .collect(),
    };

    let mcu = cfg_args
        .get("esp_idf_idf_target")
        .ok_or_else(|| {
            anyhow!(
                "Failed to get IDF_TARGET from kconfig. cfgs:\n{:?}",
                cfg_args.args
            )
        })?
        .to_lowercase();

    // We need the IDF version to configure bindgen blocklist, but normally
    // the version is parsed from the bindgen themselves, so extract it from
    // the headers manually here.
    // For now, only major version is needed.
    let idf_version_header = path_buf![
        &build_output.esp_idf,
        "components",
        "esp_common",
        "include",
        "esp_idf_version.h"
    ];
    let idf_version_major: u32 = std::fs::read_to_string(&idf_version_header)
        .ok()
        .and_then(|s| {
            regex::Regex::new(r"#define\s+ESP_IDF_VERSION_MAJOR\s+(\d+)")
                .ok()?
                .captures(&s)?
                .get(1)?
                .as_str()
                .parse()
                .ok()
        })
        .ok_or_else(|| {
            anyhow!(
                "Failed to parse ESP_IDF_VERSION_MAJOR from '{}'",
                idf_version_header.display()
            )
        })?;

    let manifest_dir = manifest_dir()?;

    let header_file = path_buf![
        &manifest_dir,
        "src",
        "include",
        if mcu == "esp8266" {
            "esp-8266-rtos-sdk"
        } else {
            "esp-idf"
        },
        "bindings.h"
    ];

    cargo::track_file(&header_file);

    // CONFIG_LIBC_PICOLIBC=y is normally only supported with GCC toolchain. This is a problem
    // as we use clang/bindgen to generate the bindings (even if a GCC toolchain is selected).
    // clang/bindgen doesn't understand GCC's -specs=picolibc.specs and falls back to the
    // newlib sysroot headers, causing errors like 'unknown type name __FILE'.
    // Detect the picolibc include dir (relative to the GCC sysroot) and inject it via the
    // Factory's clang args so it is searched before the sysroot -I path added by embuild.
    // gcc_sysroot is e.g. <toolchain>/riscv32-esp-elf/riscv32-esp-elf/;
    // picolibc is at <toolchain>/riscv32-esp-elf/picolibc/include (one level up from sysroot).
    let picolibc_include: Option<PathBuf> = if cfg_args.get("esp_idf_libc_picolibc").is_some() {
        let sysroot = build_output
            .gcc_sysroot
            .as_deref()
            .ok_or_else(|| anyhow!("CONFIG_LIBC_PICOLIBC=y but GCC sysroot could not be found"))?;
        let picolibc = sysroot.parent().unwrap().join("picolibc").join("include");
        if !picolibc.exists() {
            bail!("picolibc include dir not found at '{}'", picolibc.display());
        }
        Some(picolibc)
    } else {
        None
    };

    // Because we have multiple bindgen invocations and we can't clone a bindgen::Builder,
    // we have to set the options every time.
    let configure_bindgen = |bindgen: embuild::bindgen::types::Builder| {
        let bindgen = bindgen
            .parse_callbacks(Box::new(BindgenCallbacks::default()))
            .use_core()
            .enable_function_attribute_detection()
            .clang_arg("-DESP_PLATFORM")
            .blocklist_function("strtold")
            .blocklist_function("_strtold_r")
            .blocklist_function("v.*printf")
            .blocklist_function("v.*scanf")
            .blocklist_function("_v.*printf_r")
            .blocklist_function("_v.*scanf_r")
            .blocklist_function("esp_log_writev");
        // In ESP-IDF < v6.0, pcnt_unit_t exists as both a struct and an enum; blocklist the
        // type so we can provide the enum definition manually in src/pcnt.rs. In v6.0+ the
        // legacy enum is gone, so bindgen can handle the struct fine on its own.
        let bindgen = if idf_version_major < 6 {
            bindgen.blocklist_type("pcnt_unit_t")
        } else {
            bindgen
        };
        // If picolibc is active, inject its include path before the sysroot headers so
        // bindgen picks up the right stdlib headers (clang ignores -specs=picolibc.specs).
        let bindgen = if let Some(ref picolibc) = picolibc_include {
            bindgen.clang_arg(format!("-I{}", picolibc.display()))
        } else {
            bindgen
        };
        let bindgen = bindgen
            .clang_args(build_output.components.clang_args())
            .clang_args(vec![
                "-target",
                if mcu != "esp32" && mcu != "esp32s2" && mcu != "esp32s3" {
                    // Necessary to pass explicitly, because of https://github.com/rust-lang/rust-bindgen/issues/1555
                    "riscv32"
                } else {
                    // We don't really have a similar issue with Xtensa, but we pass it explicitly as well just in case
                    "xtensa"
                },
            ]);
        Ok(bindgen)
    };

    let bindings_file = bindgen_utils::default_bindings_file()?;
    let bindgen_err = || {
        anyhow!(
            "failed to generate bindings in file '{}'",
            bindings_file.display()
        )
    };

    #[allow(unused_mut)]
    let mut headers = vec![header_file];

    #[cfg(any(feature = "native", not(feature = "pio")))]
    // Add additional headers from extra components.
    headers.extend(
        build_output
            .config
            .native
            .combined_bindings_headers()?
            .into_iter()
            .inspect(|h| cargo::track_file(h)),
    );

    configure_bindgen(build_output.bindgen.clone().builder()?)?
        .path_headers(headers)?
        .generate()
        .with_context(bindgen_err)?
        .write_to_file(&bindings_file)
        .with_context(bindgen_err)?;

    // Generate bindings separately for each unique module name.
    #[cfg(any(feature = "native", not(feature = "pio")))]
    (|| {
        use std::fs;
        use std::io::{BufWriter, Write};

        let mut output_file =
            BufWriter::new(fs::File::options().append(true).open(&bindings_file)?);

        for (module_name, headers) in build_output.config.native.module_bindings_headers()? {
            let bindings = configure_bindgen(build_output.bindgen.clone().builder()?)?
                .path_headers(headers.into_iter().inspect(|h| cargo::track_file(h)))?
                .generate()?;

            writeln!(
                &mut output_file,
                "pub mod {module_name} {{\
                     {bindings}\
                 }}"
            )?;
        }
        Ok(())
    })()
    .with_context(bindgen_err)?;

    // Cargo fmt generated bindings.
    bindgen_utils::cargo_fmt_file(&bindings_file);

    let cfg_args = build::CfgArgs {
        args: cfg_args
            .args
            .into_iter()
            .chain(EspIdfVersion::parse(bindings_file)?.cfg_args())
            .chain(build_output.components.cfg_args())
            .chain(once(mcu))
            .collect(),
    };
    cfg_args.propagate();
    cfg_args.output();

    // In case other crates need to have access to the ESP-IDF C headers
    build_output.cincl_args.propagate();

    // In case other crates need to have access to the ESP-IDF toolchains
    if let Some(env_path) = build_output.env_path {
        cargo::set_metadata(embuild::build::ENV_PATH_VAR, env_path);
    }

    // In case other crates need access to the ESP-IDF SDK
    cargo::set_metadata(
        embuild::build::ESP_IDF_PATH_VAR,
        build_output.esp_idf.try_to_str()?,
    );

    if let Some(link_args) = build_output.link_args {
        link_args.propagate();

        // Only necessary for building the examples
        link_args.output();
    }

    Ok(())
}
