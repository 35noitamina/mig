// Syntax highlighting for the editor. The syntax set is bat's (via two-face), which covers many
// more languages than syntect's built-in set: TypeScript, TOML, Kotlin, Swift, Dockerfile and so on.
use std::path::Path;
use std::sync::OnceLock;

use eframe::egui::{self, text::LayoutJob};
use egui_extras::syntax_highlighting::{self, CodeTheme, SyntectSettings};

fn settings() -> &'static SyntectSettings {
  static SETTINGS: OnceLock<SyntectSettings> = OnceLock::new();
  SETTINGS.get_or_init(|| {
    let mut s = SyntectSettings::default();
    s.ps = two_face::syntax::extra_newlines();
    s
  })
}

// File names and extensions that two-face does not know, mapped to one it does.
const ALIASES: &[(&str, &str)] = &[
  ("jsx", "js"),
  ("mjs", "js"),
  ("cjs", "js"),
  ("mts", "ts"),
  ("cts", "ts"),
  ("jsonc", "json"),
  ("json5", "json"),
  ("lock", "toml"),
  ("pyi", "py"),
  ("zsh", "sh"),
  ("bash", "sh"),
  ("h", "c"),
  ("hpp", "cpp"),
  ("hh", "cpp"),
  ("cc", "cpp"),
];

// The name of the syntax to use for `path`, judged by file name, extension, then the first line.
pub fn syntax_name(path: &str, first_line: &str) -> Option<String> {
  let ps = &settings().ps;
  let p = Path::new(path);
  let file = p.file_name()?.to_string_lossy();
  let ext = p.extension().map(|e| e.to_string_lossy().into_owned());
  let lower = ext.as_deref().map(str::to_lowercase);
  let found = ps
    .find_syntax_by_extension(&file)
    .or_else(|| ext.as_deref().and_then(|e| ps.find_syntax_by_extension(e)))
    .or_else(|| lower.as_deref().and_then(|e| ps.find_syntax_by_extension(e)))
    .or_else(|| {
      let alias = ALIASES.iter().find(|(from, _)| Some(*from) == lower.as_deref())?.1;
      ps.find_syntax_by_extension(alias)
    })
    .or_else(|| ps.find_syntax_by_first_line(first_line))?;
  Some(found.name.clone())
}

pub fn layout(ui: &egui::Ui, theme: &CodeTheme, code: &str, syntax: Option<&str>) -> LayoutJob {
  match syntax {
    Some(name) => syntax_highlighting::highlight_with(ui.ctx(), ui.style(), theme, code, name, settings()),
    None => syntax_highlighting::highlight_with(ui.ctx(), ui.style(), theme, code, "txt", settings()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn many_languages_are_known() {
    let known: Vec<&str> = [
      "a.rs",
      "a.py",
      "a.js",
      "a.ts",
      "a.tsx",
      "a.jsx",
      "a.go",
      "a.java",
      "a.kt",
      "a.swift",
      "a.c",
      "a.cpp",
      "a.h",
      "a.cs",
      "a.rb",
      "a.php",
      "a.lua",
      "a.sh",
      "a.sql",
      "a.toml",
      "a.yaml",
      "a.json",
      "a.md",
      "a.html",
      "a.css",
      "a.scss",
      "a.xml",
      "Dockerfile",
      "Makefile",
      "a.dart",
      "a.zig",
      "a.vue",
      "a.svelte",
      "a.hs",
      "a.ex",
      "a.scala",
      "a.r",
      "a.nix",
      "a.tf",
      "a.proto",
      "a.graphql",
      "a.diff",
      "a.ini",
      "a.bat",
      "Cargo.lock",
      "a.mjs",
    ]
    .into_iter()
    .filter(|p| syntax_name(p, "").is_some())
    .collect();
    eprintln!("known: {known:?}");
    let unknown: Vec<&str> = ["a.rs", "a.ts", "a.toml", "a.kt", "a.swift", "Dockerfile"]
      .into_iter()
      .filter(|p| syntax_name(p, "").is_none())
      .collect();
    assert!(unknown.is_empty(), "no syntax for {unknown:?}");
  }

  #[test]
  fn shebang_decides_when_there_is_no_extension() {
    assert!(syntax_name("script", "#!/usr/bin/env python3").is_some());
    assert!(syntax_name("README", "plain words").is_none());
  }
}
