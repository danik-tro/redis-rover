//! Themes work only if every colour comes from the palette, so UI code must
//! not name a colour of its own.

use std::{fs, path::Path};

/// The files that define or convert colours.
const ALLOWED: &[&str] = &["src/config.rs", "src/color_depth.rs", "src/theme.rs"];

const COLOR_LITERALS: &[&str] = &[
    "Color::",
    "Rgb(",
    "Indexed(",
    ".black()",
    ".red()",
    ".green()",
    ".yellow()",
    ".blue()",
    ".magenta()",
    ".cyan()",
    ".gray()",
    ".dark_gray()",
    ".white()",
    ".on_black()",
    ".on_red()",
    ".on_green()",
    ".on_yellow()",
    ".on_blue()",
    ".on_magenta()",
    ".on_cyan()",
    ".on_gray()",
    ".on_white()",
];

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn ui_code_takes_every_colour_from_the_palette() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);

    let mut found = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWED.contains(&relative.as_str()) {
            continue;
        }
        let text = fs::read_to_string(&file).unwrap();
        // Test modules may compare against a concrete colour.
        let code = text.split("#[cfg(test)]").next().unwrap_or_default();
        for (number, line) in code.lines().enumerate() {
            let line = line.trim_start();
            if line.starts_with("//") {
                continue;
            }
            if COLOR_LITERALS.iter().any(|literal| line.contains(literal)) {
                found.push(format!("{relative}:{}: {line}", number + 1));
            }
        }
    }
    assert!(
        found.is_empty(),
        "use a palette token:\n{}",
        found.join("\n")
    );
}
