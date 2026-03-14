use std::path::Path;

const NEEDLE: &str = "process::exit(";

/// Library code must never call process::exit() — it kills the host process.
/// Only main.rs (the binary crate) may call it.
#[test]
fn no_process_exit_in_library_code() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    scan_dir(&src_dir, &mut violations);
    assert!(
        violations.is_empty(),
        "process::exit() found in library code (use anyhow::bail! instead):\n{}",
        violations.join("\n")
    );
}

fn scan_dir(dir: &Path, violations: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, violations);
        } else if path.extension().is_some_and(|e| e == "rs") {
            // Skip main.rs — it's the binary crate, not library code
            if path.file_name().unwrap() == "main.rs" {
                continue;
            }
            let content = std::fs::read_to_string(&path).unwrap();
            for (i, line) in content.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.contains(NEEDLE) && !trimmed.starts_with("//") {
                    violations.push(format!("{}:{}: {}", path.display(), i + 1, trimmed));
                }
            }
        }
    }
}
