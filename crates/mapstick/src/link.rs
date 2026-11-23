// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! Rozpoznawanie odnośników, którymi strona producenta uruchamia aplikację.
//!
//! Przycisk „Download" na `update.hyundai.com` (i bliźniaczych) nie prowadzi do
//! pliku, tylko ustawia `window.location` na własny schemat URI:
//!
//! ```text
//! com.hyundai.update.nau://?vRebqiGactPF9PtZK4g1CJ0sZq//lDH6c9P8ZCCiwbM…
//! ```
//!
//! Cała treść to jeden zaszyfrowany blok — nie ma w nim czytelnej marki,
//! modelu ani regionu, a klucz siedzi w oficjalnej aplikacji. Rozszyfrowywać go
//! jednak nie trzeba: serwis **publikuje ten sam ciąg otwartym tekstem** obok
//! nazwy modelu i wersji, w statycznych plikach `latInfo`. Wystarczy zebrać je
//! raz i mieć tablicę ciąg → pojazd.
//!
//! Ciąg zmienia się razem z wersją oprogramowania, więc tablica buduje się na
//! bieżąco, w tle. Gotowa lista dołączona do wydania starzałaby się dokładnie
//! tam, gdzie boli: przy modelu, któremu właśnie wyszła aktualizacja — a to
//! jedyny moment, w którym ktoś ten przycisk naciska.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::tr;

/// Schematy URI przypisane markom, w postaci używanej w pliku ulubionych.
const SCHEMES: &[(&str, &str)] = &[
    ("com.hyundai.update.nau", "hyundai"),
    ("com.kia.update.nau", "kia"),
    ("com.genesis.update.nau", "genesis"),
];

/// Adres strony każdej z marek. Serwis odpowiada tak samo niezależnie od tego,
/// przez który host się pyta — marka idzie w treści zapytania — ale pytamy tam,
/// gdzie pytałaby przeglądarka.
fn host_for(brand: &str) -> &'static str {
    match brand {
        "kia" => "https://update.kia.com",
        "genesis" => "https://update.genesis.com",
        _ => "https://update.hyundai.com",
    }
}

/// Marka, do której należy odnośnik — po samym schemacie, bez zaglądania
/// w zaszyfrowaną treść.
pub fn brand_of(url: &str) -> Option<&'static str> {
    let scheme = url.split(':').next()?;

    SCHEMES
        .iter()
        .find(|(s, _)| s.eq_ignore_ascii_case(scheme))
        .map(|(_, brand)| *brand)
}

pub fn is_link(url: &str) -> bool {
    brand_of(url).is_some()
}

/// Pojazd kryjący się pod odnośnikiem.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Target {
    pub brand: String,
    pub region: String,
    /// `mcd` w katalogu strony, `mcode` w nudl — to samo pole.
    pub mcode: String,
    pub name: String,
    pub version: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Table {
    #[serde(default)]
    pub links: HashMap<String, Target>,
    /// Kiedy ostatnio zebrano odnośniki marki w regionie (`hyundai/EU`).
    #[serde(default)]
    pub refreshed: HashMap<String, String>,
}

/// Jak długo zebrane odnośniki uchodzą za świeże przy starcie okna. Wydania
/// map wychodzą co kilka miesięcy, a nieznany odnośnik i tak wywołuje
/// zbieranie na żądanie — codzienne sto zapytań do cudzego serwisu przy
/// każdym uruchomieniu nic by nie dodało.
const FRESH_HOURS: i64 = 24;

fn combo_key(brand: &str, region: &str) -> String {
    format!("{brand}/{region}")
}

impl Table {
    pub fn path() -> std::path::PathBuf {
        let path = cache_base().join("links.json");
        mapstick_store::adopt_legacy(&cache_base().join("odnosniki.json"), &path);
        path
    }

    pub fn load() -> Self {
        let Ok(text) = std::fs::read_to_string(Self::path()) else {
            return Self::default();
        };

        serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!("Tablica odnośników nie do odczytania: {e}");
            Self::default()
        })
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&temp, &path)?;

        Ok(())
    }

    pub fn find(&self, url: &str) -> Option<&Target> {
        self.links.get(url.trim())
    }

    /// Dopisuje wynik zbierania, zastępując wpisy o tych samych odnośnikach.
    /// Czy odnośniki tej marki w tym regionie zebrano niedawno.
    pub fn is_fresh(&self, brand: &str, region: &str) -> bool {
        self.refreshed
            .get(&combo_key(brand, region))
            .and_then(|t| t.parse::<jiff::Timestamp>().ok())
            .is_some_and(|t| {
                t + jiff::SignedDuration::from_hours(FRESH_HOURS) > jiff::Timestamp::now()
            })
    }

    /// Zapisuje, że marka w regionie została właśnie zebrana.
    pub fn mark_refreshed(&mut self, brand: &str, region: &str) {
        self.refreshed
            .insert(combo_key(brand, region), jiff::Timestamp::now().to_string());
    }

    pub fn merge(&mut self, found: Vec<(String, Target)>) -> usize {
        let before = self.links.len();

        for (url, target) in found {
            self.links.insert(url, target);
        }

        self.links.len() - before
    }
}

fn cache_base() -> std::path::PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("mapstick")
}

/// Jeden wpis oprogramowania w pliku `latInfo`.
#[derive(Debug, Deserialize)]
struct SwInfo {
    mcd: serde_json::Value,
    #[serde(default)]
    region: Option<String>,
    #[serde(default, rename = "swVer")]
    versions: Vec<String>,
    #[serde(default, rename = "downInfo")]
    links: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct LatestInfo {
    #[serde(rename = "genNm")]
    name: Option<String>,
    #[serde(default, rename = "swInfo")]
    software: Vec<SwInfo>,
}

/// Ile plików `latInfo` ciągniemy naraz. Są malutkie (setki bajtów), ale jest
/// ich koło stu na markę i region — bez ograniczenia byłoby to sto połączeń
/// naraz do cudzego serwisu.
const CONCURRENCY: usize = 8;

/// Zbiera odnośniki dla jednej marki i regionu.
///
/// `brand` w postaci z pliku ulubionych (`hyundai`), `code` w postaci serwisu
/// (`HM`).
pub async fn collect(brand: &str, code: &str, region: &str) -> Result<Vec<(String, Target)>> {
    let client = reqwest::Client::builder()
        .user_agent("MapStick")
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .with_context(|| tr!("Could not create the HTTP client"))?;

    let url = format!("{}/api/car/model", host_for(brand));
    let body = serde_json::json!({
        "commonCode": { "brand": code, "reg": region, "lang": "EN" }
    });

    let catalogue: serde_json::Value = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| tr!("Could not query {}", url))?
        .json()
        .await
        .with_context(|| tr!("Unexpected response from {}", url))?;

    let files = file_urls(&catalogue);
    tracing::info!("{brand}/{region}: {} plików z wersjami", files.len());

    let mut found = Vec::new();

    // Okno przesuwne zamiast paczek: paczka czeka na swój najwolniejszy
    // plik, a tu każdy zakończony od razu robi miejsce następnemu.
    let mut pending = files.into_iter();
    let mut set = tokio::task::JoinSet::new();
    let spawn = |set: &mut tokio::task::JoinSet<_>, file: String| {
        let client = client.clone();
        set.spawn(async move {
            let text = client.get(&file).send().await?.text().await?;
            Ok::<_, reqwest::Error>(text)
        });
    };

    for file in pending.by_ref().take(CONCURRENCY) {
        spawn(&mut set, file);
    }

    while let Some(joined) = set.join_next().await {
        if let Some(file) = pending.next() {
            spawn(&mut set, file);
        }

        let Ok(Ok(text)) = joined else { continue };

        let Ok(info) = serde_json::from_str::<LatestInfo>(&text) else {
            continue;
        };

        let name = info.name.unwrap_or_default();

        for software in info.software {
            let Some(links) = software.links else {
                continue;
            };

            let target = Target {
                brand: brand.to_owned(),
                region: software.region.unwrap_or_else(|| region.to_owned()),
                mcode: match &software.mcd {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                },
                name: name.clone(),
                version: software.versions.first().cloned().unwrap_or_default(),
            };

            for link in links {
                found.push((link, target.clone()));
            }
        }
    }

    Ok(found)
}

/// Wyciąga adresy plików `latInfo` z drzewa katalogu.
///
/// Chodzimy po strukturze rekurencyjnie zamiast po znanej ścieżce
/// (`data[].mdlList[].genRepList[].genList[]`), bo to cudzy kształt i może się
/// zmienić bez uprzedzenia.
fn file_urls(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(value, &mut out);
    out.sort();
    out.dedup();
    out
}

fn walk(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                if key == "fileNm"
                    && let Some(url) = item.as_str()
                    && url.starts_with("https://")
                {
                    out.push(url.to_owned());
                } else {
                    walk(item, out);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_brand_comes_from_the_scheme() {
        assert_eq!(brand_of("com.hyundai.update.nau://?abc"), Some("hyundai"));
        assert_eq!(brand_of("com.kia.update.nau://?abc"), Some("kia"));
        assert_eq!(brand_of("com.genesis.update.nau://?abc"), Some("genesis"));
        assert_eq!(brand_of("https://example.invalid"), None);
        assert_eq!(brand_of("kompletnie co innego"), None);
    }

    /// Kształt katalogu jest cudzy, więc szukamy pola, nie ścieżki do niego.
    #[test]
    fn file_urls_are_found_at_any_depth() {
        let value = serde_json::json!({
            "data": [{
                "mdlList": [{
                    "genRepList": [{
                        "genList": [
                            {"genNm": "Bayon", "fileNm": "https://x.invalid/a.json"},
                            {"genNm": "i20", "fileNm": "https://x.invalid/b.json"}
                        ]
                    }]
                }]
            }],
            "gdzie_indziej": {"fileNm": "https://x.invalid/c.json"},
            "nie_adres": {"fileNm": "/wzgledny.json"}
        });

        assert_eq!(
            file_urls(&value),
            vec![
                "https://x.invalid/a.json",
                "https://x.invalid/b.json",
                "https://x.invalid/c.json"
            ]
        );
    }

    #[test]
    fn a_latest_info_file_yields_the_link_and_the_vehicle() {
        let text = r#"{"genNm":"Bayon","swInfo":[{"mcd":1478,"region":"EU",
            "swVer":["BC3.EUR.S5W_L.001.001.260128"],
            "downInfo":["com.hyundai.update.nau://?AAAA"]}]}"#;

        let info: LatestInfo = serde_json::from_str(text).unwrap();

        assert_eq!(info.name.as_deref(), Some("Bayon"));
        assert_eq!(info.software[0].mcd.to_string(), "1478");
        assert_eq!(
            info.software[0].links.as_ref().unwrap()[0],
            "com.hyundai.update.nau://?AAAA"
        );
    }

    /// Wpisy bez `downInfo` (model bez pobierania) nie mogą wywrócić odczytu.
    #[test]
    fn entries_without_a_link_are_tolerated() {
        let text = r#"{"genNm":"X","swInfo":[{"mcd":"1","swVer":[]}]}"#;
        let info: LatestInfo = serde_json::from_str(text).unwrap();

        assert!(info.software[0].links.is_none());
    }

    #[test]
    fn merging_replaces_entries_for_the_same_link() {
        let mut table = Table::default();
        let target = Target {
            brand: "hyundai".to_owned(),
            region: "EU".to_owned(),
            mcode: "1478".to_owned(),
            name: "Bayon".to_owned(),
            version: "stara".to_owned(),
        };

        table.merge(vec![("link".to_owned(), target.clone())]);
        table.merge(vec![(
            "link".to_owned(),
            Target {
                version: "nowa".to_owned(),
                ..target
            },
        )]);

        assert_eq!(table.links.len(), 1);
        assert_eq!(table.find("link").unwrap().version, "nowa");
    }
}
