// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! The scripts in static/ load as classic <script> tags and share the global
//! scope: two declarations with the same name in different files make the last
//! one silently replace the first. This test fails if a top-level name is
//! declared in more than one file.

use std::collections::BTreeMap;

#[test]
fn no_duplicate_globals_across_scripts() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("static");
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).collect();
    files.sort();
    for f in files {
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".js") || name.ends_with(".min.js") {
            continue;
        }
        for line in std::fs::read_to_string(&f).unwrap().lines() {
            let rest = line
                .strip_prefix("async function ")
                .or_else(|| line.strip_prefix("function "))
                .or_else(|| line.strip_prefix("const "))
                .or_else(|| line.strip_prefix("let "))
                .or_else(|| line.strip_prefix("var "))
                .or_else(|| line.strip_prefix("class "));
            let Some(rest) = rest else { continue };
            let ident: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect();
            if ident.is_empty() {
                continue; // destructuring: const { a, b } = …
            }
            let files = seen.entry(ident).or_default();
            if !files.contains(&name) {
                files.push(name.clone());
            }
        }
    }
    let dups: Vec<String> = seen
        .into_iter()
        .filter(|(_, f)| f.len() > 1)
        .map(|(n, f)| format!("{n}: {}", f.join(", ")))
        .collect();
    assert!(dups.is_empty(), "top-level names declared in more than one script:\n{}", dups.join("\n"));
}
