use std::{env, fs, path::PathBuf};
use tylertoo_core::internal::overview::config::keywords::{
    command_defaults, defaults, doc_constant, CONVERT_POSITIONAL,
};

fn literal(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "None".into(),
        serde_json::Value::Bool(true) => "True".into(),
        serde_json::Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

fn expand_docs(source: &str, name: &str) -> String {
    use tylertoo_core::internal::overview::{convert::ConvertOptions, export::ExportOptions};
    let mut values = defaults(ExportOptions::default());
    values.extend(defaults(ConvertOptions::default()));
    for (key, value) in command_defaults(name) {
        if !value.is_null() {
            values.insert(key, value);
        }
    }
    let mut output = source.to_owned();
    for kind in ["default", "const"] {
        let prefix = format!("{{{kind}:");
        while let Some(start) = output.find(&prefix) {
            let end = start
                + output[start..]
                    .find('}')
                    .expect("unclosed docstring placeholder");
            let key = &output[start + prefix.len()..end];
            let value = if kind == "default" {
                literal(
                    values
                        .get(key)
                        .unwrap_or_else(|| panic!("unknown docstring default: {key}")),
                )
            } else {
                doc_constant(key).unwrap_or_else(|| panic!("unknown docstring constant: {key}"))
            };
            output.replace_range(start..=end, &value);
        }
    }
    output
}

/// Docstrings never type a default: a digit after "default" or "`None` for"
/// must come from a `{default:KEY}` or `{const:NAME}` placeholder. This runs
/// on every build of the extension, so CI cannot miss it.
fn assert_no_typed_defaults(source: &str, path: &str) {
    let lower = source
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    for marker in ["`none` for", "default"] {
        for after in lower.split(marker).skip(1) {
            let value = after.trim_start_matches(|c: char| {
                c.is_whitespace() || matches!(c, '`' | ':' | '(' | '"')
            });
            assert!(
                !value.starts_with(|c: char| c.is_ascii_digit()),
                "{path}: typed default after {marker:?}: {}; use a {{default:KEY}} or {{const:NAME}} placeholder",
                value.chars().take(60).collect::<String>()
            );
        }
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
        "stats",
        "serve",
    ] {
        let mut defaults = command_defaults(name);
        let positional = match name {
            "validate" => "file",
            "stats" | "serve" => "archive",
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
        } else if name == "stats" || name == "serve" {
            "archive, **kwargs"
        } else if name == "pyramid" {
            "bands, output, **kwargs"
        } else {
            "input, output, **kwargs"
        };
        let doc_path = format!("src/docs/{name}.txt");
        println!("cargo:rerun-if-changed={doc_path}");
        let source = fs::read_to_string(&doc_path).expect("Python API docstring");
        assert_no_typed_defaults(&source, &doc_path);
        let docs = expand_docs(&source, name);
        output.push_str(&format!("macro_rules! signature_{name} {{ ($function:item) => {{ #[expect(clippy::doc_markdown, reason = \"#779: public Python docstrings use Python identifiers and Google-style sections\")] #[pyfunction] #[pyo3(signature = ({args}), text_signature = {signature:?})] #[doc = {docs:?}] $function }}; }}\n"));
    }
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR")).join("signatures.rs"),
        output,
    )
    .expect("write generated signatures");
    println!("cargo:rerun-if-changed=build.rs");
}
