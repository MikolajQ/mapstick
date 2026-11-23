// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! Kompilacja interfejsu i wbudowanie tłumaczeń.
//!
//! Jeden plik `.po` na język, dwóch odbiorców: Slint wkompilowuje z niego
//! napisy z `.slint` sam, a napisy z Rusta wyciąga poniższy parser i zapisuje
//! jako tablice w `$OUT_DIR/translations.rs`. Bez `libintl`, bez plików
//! doinstalowywanych obok binarki — jedno `cargo build` daje gotowy program
//! na każdym systemie.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// Katalog z tłumaczeniami: `lang/<język>/LC_MESSAGES/mapstick.po`.
const LANG_DIR: &str = "lang";

fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .with_bundled_translations(LANG_DIR)
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);

    slint_build::compile_with_config("ui/app.slint", config).expect("Failed to compile Slint UI");

    // Zmienna z chwili uruchomienia, nie `env!` z chwili kompilacji skryptu:
    // skompilowany skrypt bywa użyty ponownie z innego miejsca (wspólny
    // katalog target dla dwóch kopii repozytorium) i szukałby tłumaczeń tam,
    // gdzie ich już nie ma — po cichu, z pustą tablicą.
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let lang_dir = PathBuf::from(manifest_dir).join(LANG_DIR);
    println!("cargo:rerun-if-changed={}", lang_dir.display());

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    fs::write(out.join("translations.rs"), generate(&lang_dir)).expect("write translations.rs");
}

/// Buduje kod z tablicami `msgid → msgstr` dla każdego znalezionego języka.
fn generate(lang_dir: &Path) -> String {
    let mut languages: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();

    if let Ok(entries) = fs::read_dir(lang_dir) {
        for entry in entries.flatten() {
            let po = entry.path().join("LC_MESSAGES").join("mapstick.po");
            let Ok(text) = fs::read_to_string(&po) else {
                continue;
            };

            println!("cargo:rerun-if-changed={}", po.display());

            let language = entry.file_name().to_string_lossy().into_owned();
            languages.insert(language, parse_po(&text));
        }
    }

    // Bez tłumaczeń program i tak by się zbudował — mówiąc wszędzie po
    // angielsku. Lepiej, żeby build padł od razu.
    assert!(
        !languages.is_empty(),
        "Brak tłumaczeń w {} — build bez nich dałby program tylko po angielsku",
        lang_dir.display()
    );

    let mut code = String::from(
        "/// Tłumaczenia wbudowane z `lang/`. Wygenerowane przez build.rs.\n\
         pub static TRANSLATIONS: &[(&str, &[(&str, &str)])] = &[\n",
    );

    for (language, entries) in &languages {
        code.push_str(&format!("    ({language:?}, &[\n"));
        for (msgid, msgstr) in entries {
            if msgid.is_empty() || msgstr.is_empty() {
                continue;
            }
            code.push_str(&format!("        ({msgid:?}, {msgstr:?}),\n"));
        }
        code.push_str("    ]),\n");
    }

    code.push_str("];\n");
    code
}

/// Minimalny parser gettext: `msgid`/`msgstr` z kontynuacjami w kolejnych
/// wierszach. Konteksty (`msgctxt`) i liczba mnoga są pomijane — po stronie
/// Rusta nie używamy ani jednego, ani drugiego.
fn parse_po(text: &str) -> BTreeMap<String, String> {
    let mut entries = BTreeMap::new();
    let (mut msgid, mut msgstr) = (String::new(), String::new());
    let mut current: Option<&mut String> = None;
    let mut skip = false;

    let mut flush = |msgid: &mut String, msgstr: &mut String, skip: &mut bool| {
        if !*skip && !msgid.is_empty() {
            entries.insert(std::mem::take(msgid), std::mem::take(msgstr));
        } else {
            msgid.clear();
            msgstr.clear();
        }
        *skip = false;
    };

    for line in text.lines() {
        let line = line.trim();

        if line.starts_with("#,") && line.contains("fuzzy") {
            skip = true;
        }

        if let Some(rest) = line.strip_prefix("msgctxt ") {
            // Wpis z kontekstem należy do Slinta, nie do nas.
            let _ = rest;
            skip = true;
            current = None;
        } else if let Some(rest) = line.strip_prefix("msgid ") {
            flush(&mut msgid, &mut msgstr, &mut skip);
            msgid.push_str(&unquote(rest));
            current = Some(&mut msgid);
        } else if let Some(rest) = line.strip_prefix("msgid_plural ") {
            let _ = rest;
            skip = true;
            current = None;
        } else if let Some(rest) = line.strip_prefix("msgstr ") {
            msgstr.push_str(&unquote(rest));
            current = Some(&mut msgstr);
        } else if line.starts_with("msgstr[") {
            skip = true;
            current = None;
        } else if line.starts_with('"') {
            if let Some(target) = current.as_deref_mut() {
                target.push_str(&unquote(line));
            }
        } else if line.is_empty() {
            current = None;
        }
    }

    flush(&mut msgid, &mut msgstr, &mut skip);
    entries
}

/// Zdejmuje cudzysłowy i rozwija ucieczki znane z plików `.po`.
fn unquote(raw: &str) -> String {
    let inner = raw.trim().trim_start_matches('"').trim_end_matches('"');
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();

    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }

        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }

    out
}
