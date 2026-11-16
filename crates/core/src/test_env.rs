//! Re-run an environment-dependent test alone in a child process, so a test
//! that sets variables never touches the shared test process's environment.

/// Runs the test `module::test` (`module` is the caller's `module_path!()`)
/// alone in a child process with `vars` set. Returns `true` inside that
/// child, where the caller runs its body, and `false` in the parent once the
/// child has passed. The child sees only `vars`, never the shell's
/// `TYLERTOO_*` or store credential variables.
pub(crate) fn run_in_child(module: &str, test: &str, vars: &[(&str, &str)]) -> bool {
    const CHILD: &str = "TYLERTOO_ENV_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let (_, module) = module.split_once("::").expect("crate::module path");
    let name = format!("{module}::{test}");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary path"));
    for (k, _) in std::env::vars_os().filter(|(k, _)| {
        let k = k.to_string_lossy();
        k == "SERVICE_ACCOUNT"
            || ["TYLERTOO_", "AWS_", "GOOGLE_"]
                .iter()
                .any(|p| k.starts_with(p))
    }) {
        child.env_remove(k);
    }
    let out = child
        .args([name.as_str(), "--exact", "--test-threads=1", "--nocapture"])
        .env(CHILD, "1")
        .envs(vars.iter().copied())
        .output()
        .expect("re-run the test binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "{name} failed in its own process:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    false
}
