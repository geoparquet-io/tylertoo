use std::{env, fs, path::PathBuf};
use tylertoo_core::overview::config::keywords::{command_defaults, CONVERT_POSITIONAL};

fn literal(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "None".into(),
        serde_json::Value::Bool(true) => "True".into(),
        serde_json::Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

fn main() {
    let mut output = String::new();
    for name in [
        "convert",
        "overview",
        "export_pmtiles",
        "pyramid",
        "decode",
        "validate",
    ] {
        let mut defaults = command_defaults(name);
        let positional = match name {
            "validate" => "file",
            "pyramid" => "bands, output",
            _ => "input, output",
        };
        let mut signature = format!("({positional}");
        if name == "convert" {
            for key in CONVERT_POSITIONAL {
                let value = defaults
                    .remove(*key)
                    .expect("legacy parameter is a core option");
                signature.push_str(&format!(", {key}={}", literal(&value)));
            }
        }
        if !defaults.is_empty() {
            signature.push_str(", *");
            for (key, value) in defaults {
                signature.push_str(&format!(", {key}={}", literal(&value)));
            }
        }
        signature.push(')');
        let args = if name == "validate" {
            "file"
        } else if name == "convert" {
            "input, output, *args, **kwargs"
        } else if name == "pyramid" {
            "bands, output, **kwargs"
        } else {
            "input, output, **kwargs"
        };
        let doc_path = format!("src/docs/{name}.txt");
        println!("cargo:rerun-if-changed={doc_path}");
        let docs = fs::read_to_string(doc_path).expect("Python API docstring");
        output.push_str(&format!("macro_rules! signature_{name} {{ ($function:item) => {{ #[expect(clippy::doc_markdown, reason = \"#779: public Python docstrings use Python identifiers and Google-style sections\")] #[pyfunction] #[pyo3(signature = ({args}), text_signature = {signature:?})] #[doc = {docs:?}] $function }}; }}\n"));
    }
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR")).join("signatures.rs"),
        output,
    )
    .expect("write generated signatures");
    println!("cargo:rerun-if-changed=build.rs");
}
