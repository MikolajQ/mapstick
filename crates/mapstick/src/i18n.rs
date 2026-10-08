// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! Tłumaczenie napisów budowanych po stronie Rusta.
//!
//! Napisy w kodzie są po angielsku i są zarazem kluczami (`msgid`) — tak samo
//! jak w plikach `.slint`. Tłumaczenia leżą w `lang/<język>/LC_MESSAGES/
//! mapstick.po`, wspólne dla obu stron; `build.rs` wkompilowuje je w binarkę.
//!
//! Język wybieramy raz, przy starcie, tą samą zasadą co Slint dla własnych
//! napisów: najpierw dokładne dopasowanie ustawień systemu (`pl-PL`), potem
//! sam język (`pl`), na końcu angielski, czyli oryginał.
//!
//! Wstawianie wartości: `{}` po kolei albo `{0}`, `{1}` z numerem, żeby
//! tłumacz mógł zmienić szyk. To ten sam zapis, którego używa Slint.

use std::{fmt::Display, sync::OnceLock};

include!(concat!(env!("OUT_DIR"), "/translations.rs"));

/// Numer wybranego języka w [`TRANSLATIONS`]; `None` to angielski.
static CHOSEN: OnceLock<Option<usize>> = OnceLock::new();

/// Języki, które mamy wbudowane, w kolejności z katalogu `lang/`.
///
/// Na razie nieużywane przez interfejs — wybór języka idzie przez
/// `MAPSTICK_LANG` i ustawienia systemu. Zostaje jako gotowa podstawa pod
/// przełącznik języka w oknie, gdy dojdzie na to kolej.
#[allow(dead_code)]
pub fn available() -> impl Iterator<Item = &'static str> {
    TRANSLATIONS.iter().map(|(language, _)| *language)
}

/// Ustala język na resztę życia procesu.
///
/// `preferred` to wybór użytkownika z ustawień, jeśli go zrobił; bez niego
/// pytamy system. Wywołane po raz drugi nic nie zmienia — Slint też wybiera
/// raz i oba wybory mają się zgadzać.
pub fn choose(preferred: Option<&str>) -> Option<&'static str> {
    let locale = preferred
        .map(str::to_owned)
        .or_else(sys_locale::get_locale)
        .unwrap_or_default();

    let index = match_language(&locale);
    let _ = CHOSEN.set(index);

    index.map(|i| TRANSLATIONS[i].0)
}

/// Dopasowanie jak w Slincie: cały kod, potem sam język przed `-`, `_`, `@`.
fn match_language(locale: &str) -> Option<usize> {
    fn base(l: &str) -> &str {
        l.find(['-', '_', '@', '.']).map_or(l, |i| &l[..i])
    }

    TRANSLATIONS
        .iter()
        .position(|(language, _)| *language == locale)
        .or_else(|| {
            let wanted = base(locale);
            TRANSLATIONS
                .iter()
                .position(|(language, _)| base(language) == wanted)
        })
}

/// Tłumaczy napis; bez tłumaczenia zostaje oryginał.
pub fn tr(msgid: &str) -> &str {
    let Some(Some(index)) = CHOSEN.get() else {
        return msgid;
    };

    TRANSLATIONS[*index]
        .1
        .iter()
        .find(|(id, _)| *id == msgid)
        .map_or(msgid, |(_, translated)| *translated)
}

/// Tłumaczy i wstawia wartości.
pub fn trf(msgid: &str, arguments: &[&dyn Display]) -> String {
    format(tr(msgid), arguments)
}

/// Wstawia wartości w `{}` (po kolei) i `{n}` (po numerze). `{{` i `}}` to
/// dosłowne nawiasy.
fn format(pattern: &str, arguments: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut next = 0;
    let mut chars = pattern.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut spec = String::new();
                for inner in chars.by_ref() {
                    if inner == '}' {
                        break;
                    }
                    spec.push(inner);
                }

                let index = if spec.is_empty() {
                    next += 1;
                    next - 1
                } else {
                    match spec.parse::<usize>() {
                        Ok(i) => i,
                        Err(_) => {
                            // Nieznany zapis — zostawiamy, jak było, żeby błąd
                            // tłumacza było widać, a nie żeby znikał po cichu.
                            out.push('{');
                            out.push_str(&spec);
                            out.push('}');
                            continue;
                        }
                    }
                };

                match arguments.get(index) {
                    Some(value) => out.push_str(&value.to_string()),
                    None => {
                        out.push('{');
                        out.push_str(&spec);
                        out.push('}');
                    }
                }
            }
            other => out.push(other),
        }
    }

    out
}

/// `tr!("text")` albo `tr!("{} of {}", a, b)`.
#[macro_export]
macro_rules! tr {
    ($msgid:literal) => {
        $crate::i18n::tr($msgid).to_owned()
    };
    ($msgid:literal, $($arg:expr),+ $(,)?) => {
        $crate::i18n::trf($msgid, &[$(&$arg as &dyn ::std::fmt::Display),+])
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_filled_in_order_or_by_number() {
        assert_eq!(format("{} of {}", &[&1, &2]), "1 of 2");
        assert_eq!(format("{1} of {0}", &[&1, &2]), "2 of 1");
        assert_eq!(format("{0}{0}", &[&"ab"]), "abab");
    }

    #[test]
    fn braces_can_be_written_literally() {
        assert_eq!(format("{{x}} {}", &[&5]), "{x} 5");
    }

    /// Błąd tłumacza ma być widoczny, nie połknięty.
    #[test]
    fn a_missing_argument_stays_visible() {
        assert_eq!(format("{} and {}", &[&1]), "1 and {}");
        assert_eq!(format("{name}", &[&1]), "{name}");
    }

    #[test]
    fn the_language_part_alone_is_enough_to_match() {
        // Tablica pochodzi z lang/; test nie zakłada, co w niej jest, tylko
        // że dopasowanie po samym języku działa tak samo jak po pełnym kodzie.
        for (language, _) in TRANSLATIONS {
            assert!(match_language(language).is_some());
            assert!(match_language(&format!("{language}-XX.UTF-8")).is_some());
        }
        assert_eq!(match_language("tlh"), None);
    }

    /// Napisy z `tr!("…")` i `i18n::tr("…")` w źródłach tej skrzynki.
    fn messages_in_sources() -> Vec<String> {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found = Vec::new();

        for entry in std::fs::read_dir(src).unwrap() {
            let path = entry.unwrap().path();
            // Ten plik tylko opisuje `tr!` — w dokumentacji i w tym teście.
            if path.extension().is_none_or(|e| e != "rs") || path.ends_with("i18n.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();

            for call in ["tr!(", "i18n::tr("] {
                for (start, _) in text.match_indices(call) {
                    let rest = text[start + call.len()..].trim_start();
                    if let Some(literal) = rest.strip_prefix('"') {
                        found.push(unescape(literal));
                    }
                }
            }
        }

        found
    }

    /// Treść literału napisu Rusta aż do zamykającego cudzysłowu — tyle
    /// sekwencji ucieczki, ile używają nasze napisy.
    fn unescape(literal: &str) -> String {
        let mut out = String::new();
        let mut chars = literal.chars();

        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    // Kontynuacja wiersza: znika razem z wcięciem następnego.
                    Some('\n') => {
                        let rest: String = chars.collect();
                        return out + &unescape(rest.trim_start());
                    }
                    Some(other) => out.push(other),
                    None => break,
                },
                _ => out.push(c),
            }
        }

        out
    }

    /// Każdy napis budowany w Rustcie ma tłumaczenie w każdym języku. Bez
    /// tego zapomniany `tr!` albo nowa etykieta wychodzą dopiero na żywym
    /// systemie — tak trafiły się trzy, gdy MapStick pierwszy raz ruszył
    /// na Windows.
    #[test]
    fn every_message_is_translated_in_every_language() {
        use crate::schedule::Kind;
        use mapstick_store::Interval;

        let mut messages = messages_in_sources();
        messages.extend(
            [
                Kind::Systemd,
                Kind::Cron,
                Kind::Autostart,
                Kind::TaskScheduler,
                Kind::Launchd,
            ]
            .map(|k| k.label().to_owned()),
        );
        messages.extend(Interval::ALL.iter().map(|i| i.label().to_owned()));
        messages.extend(crate::REGIONS.iter().map(|(_, label)| (*label).to_owned()));

        assert!(messages.len() > 50, "parser znalazł podejrzanie mało: {}", messages.len());

        let mut missing = Vec::new();
        for (language, table) in TRANSLATIONS {
            for message in &messages {
                if !table.iter().any(|(id, _)| id == message) {
                    missing.push(format!("{language}: {message:?}"));
                }
            }
        }
        missing.sort();
        missing.dedup();

        assert!(missing.is_empty(), "brak tłumaczeń:\n{}", missing.join("\n"));
    }
}
