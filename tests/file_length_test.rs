use std::fs;
use std::path::Path;

#[test]
fn test_max_file_lines() {
    let max_lines = 1000;
    let mut violations = Vec::new();

    check_directory(Path::new("src"), max_lines, &mut violations);

    if !violations.is_empty() {
        panic!(
            "File line limit (max {} lines) exceeded:\n{}",
            max_lines,
            violations.join("\n")
        );
    }
}

fn check_directory(dir: &Path, max_lines: usize, violations: &mut Vec<String>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                check_directory(&path, max_lines, violations);
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                if let Ok(content) = fs::read_to_string(&path) {
                    let line_count = content.lines().count();
                    if line_count > max_lines {
                        violations.push(format!("{}: {} lines", path.display(), line_count));
                    }
                }
            }
        }
    }
}
