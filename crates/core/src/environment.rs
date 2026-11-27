//! Environment settings shared by the frontends and reference generator.

/// Environment variables supported by tylertoo.
pub const ENVIRONMENT: &[(&str, &str)] = &[
    ("RAYON_NUM_THREADS", "Worker threads used when the workers option is unset."),
    ("TYLERTOO_AUTO_MEM_LIMIT_BYTES", "Available-memory override in bytes; the memory-limit option takes precedence."),
    (crate::overview::convert::SKIP_MEMORY_PREFLIGHT_ENV, "Skip the memory preflight refusal; allocation can still fail."),
    ("TYLERTOO_HTTP_<KEY>", "Remote HTTP client option named by KEY, converted to lowercase; unsupported or invalid options produce a warning."),
    (crate::input::CONCURRENCY_VAR, "Experimental, not stable API: maximum concurrent staging requests."),
    (crate::input::MEM_BUDGET_VAR, "Experimental, not stable API: staging memory budget in bytes."),
    (crate::input::COALESCE_VAR, "Experimental, not stable API: staging request coalescing window in bytes."),
    ("TYLERTOO_PROFILE_JSON", "Path for structured pipeline profiling output."),
    (crate::tile_trace::ENV, "Path for the per-tile export trace."),
];

#[cfg(test)]
mod tests {
    use super::ENVIRONMENT;

    #[test]
    fn every_env_var_is_documented() {
        const DEV_ONLY: &[&str] = &["TYLERTOO_DHAT", "TYLERTOO_BYTE_ARRAY_LIMIT_BYTES"];
        fn scan(path: &std::path::Path, missing: &mut std::collections::BTreeSet<String>) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    scan(&path, missing);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).unwrap();
                for quoted in source
                    .split("\"TYLERTOO_")
                    .skip(1)
                    .filter_map(|tail| tail.split('"').next())
                    .map(|tail| format!("TYLERTOO_{tail}"))
                {
                    if !quoted.starts_with("TYLERTOO_")
                        || (quoted == "TYLERTOO_" || quoted == "TYLERTOO_HTTP_")
                        || !quoted
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                    {
                        continue;
                    }
                    let documented = ENVIRONMENT.iter().any(|(name, _)| {
                        *name == quoted.as_str()
                            || (*name == "TYLERTOO_HTTP_<KEY>"
                                && quoted.starts_with("TYLERTOO_HTTP_"))
                    });
                    if !documented
                        && !DEV_ONLY.contains(&quoted.as_str())
                        && !quoted.contains("_TEST_")
                    {
                        missing.insert(format!("{}: {quoted}", path.display()));
                    }
                }
            }
        }
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let mut missing = std::collections::BTreeSet::new();
        for entry in std::fs::read_dir(crates).unwrap() {
            let src = entry.unwrap().path().join("src");
            if src.is_dir() {
                scan(&src, &mut missing);
            }
        }
        assert!(
            missing.is_empty(),
            "undocumented environment variables: {missing:#?}"
        );
    }
}
