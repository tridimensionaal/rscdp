mod emit;
mod schema;

use std::{env, fs, path::PathBuf};

#[cfg_attr(test, allow(dead_code))]
pub(super) fn generate() -> Result<(), Box<dyn std::error::Error>> {
    let mut inputs = Vec::new();
    for path in [
        "protocol/browser_protocol.json",
        "protocol/js_protocol.json",
    ] {
        println!("cargo:rerun-if-changed={path}");
        inputs.push((path, fs::read(path)?));
    }
    let inputs: Vec<_> = inputs
        .iter()
        .map(|(path, bytes)| (*path, bytes.as_slice()))
        .collect();
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?).join("cdp.rs"),
        render(&inputs)?,
    )?;
    Ok(())
}

pub(super) fn render(inputs: &[(&str, &[u8])]) -> std::io::Result<String> {
    let mut domains = Vec::new();
    for (source, bytes) in inputs {
        domains.extend(schema::parse(bytes, source)?.domains);
    }
    schema::validate(&domains)?;
    Ok(format!(
        "// Generated from protocol/*.json. Do not edit.\n{}",
        emit::generate(&domains)
    ))
}
