//
// SPDX-License-Identifier: MIT OR Apache-2.0
//

//! Emit `extern crate <plugin> as _;` for each enabled plugin crate so the
//! linker keeps its `inventory::submit!` registrations.

use std::env;
use std::fs;
use std::path::PathBuf;

use toml::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginSpec {
    crate_name: String,
    feature: Option<String>,
}

fn main() {
    println!("cargo:rerun-if-changed=Cargo.toml");

    let manifest = fs::read_to_string("Cargo.toml").expect("crate Cargo.toml should be readable");
    let links = plugin_links(&manifest).expect("plugin metadata should be valid TOML");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR should be set by cargo"));
    fs::write(out_dir.join("plugin_links.rs"), links).expect("generated plugin link file should be writable");
}

fn plugin_links(manifest: &str) -> Result<String, String> {
    let plugins = plugin_specs(manifest)?;
    let mut links = String::new();

    for plugin in plugins {
        if let Some(feature) = plugin.feature.as_deref() {
            let env_name = format!("CARGO_FEATURE_{}", feature.to_uppercase().replace('-', "_"));
            if env::var_os(&env_name).is_none() {
                continue;
            }
        }

        let crate_ident = plugin.crate_name.replace('-', "_");
        links.push_str(&format!("extern crate {crate_ident} as _;\n"));
    }

    Ok(links)
}

fn plugin_specs(manifest: &str) -> Result<Vec<PluginSpec>, String> {
    let document: Value = toml::from_str(manifest).map_err(|err| err.to_string())?;
    let Some(plugins) = document
        .get("package")
        .and_then(|value| value.get("metadata"))
        .and_then(|value| value.get("summit-usbgadget"))
        .and_then(|value| value.get("plugins"))
    else {
        return Ok(Vec::new());
    };

    let items = plugins
        .as_array()
        .ok_or_else(|| "package.metadata.summit-usbgadget.plugins must be an array".to_string())?;

    items
        .iter()
        .enumerate()
        .map(|(index, value)| parse_plugin_spec(value, index))
        .collect()
}

fn parse_plugin_spec(value: &Value, index: usize) -> Result<PluginSpec, String> {
    let table = value
        .as_table()
        .ok_or_else(|| format!("plugin entry #{index} must be a table"))?;

    let crate_name = table
        .get("crate")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("plugin entry #{index} is missing string field `crate`"))?
        .to_string();

    let feature = table
        .get("feature")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| format!("plugin entry #{index} field `feature` must be a string"))
                .map(ToOwned::to_owned)
        })
        .transpose()?;

    Ok(PluginSpec { crate_name, feature })
}