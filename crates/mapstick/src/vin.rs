// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! Wyszukiwanie pojazdu po numerze VIN.
//!
//! Strona aktualizacji każdej marki (`update.hyundai.com` i bliźniacze) pyta
//! własny serwis bez logowania:
//!
//! ```text
//! POST /api/search/vin
//! {"commonCode":{"brand":"HM","reg":"EU","lang":"EN"},"vin":"…"}
//! ```
//!
//! Odpowiedź niesie dla każdego pasującego modelu listę oprogramowania
//! (`swInfo`), a w niej `mcd` — ten sam kod, którym katalog nudl opisuje
//! pojazdy (`mcode`). Dalej jest więc ta sama droga co przy odnośniku ze
//! strony: kod → model na liście. Nieznany numer to `status` 4413.
//!
//! To nieudokumentowany adres i raz już zmienił się po cichu
//! (`car/query/vin` → `search/vin`). VIN jest więc skrótem, nigdy jedyną
//! drogą: każdy model da się też wybrać z listy.
//!
//! VIN wskazuje konkretny egzemplarz auta. Idzie wyłącznie do serwisu
//! producenta, tak jak ze strony, i nie trafia ani do dziennika, ani do pliku.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::{link::Target, tr};

/// Kod odpowiedzi „nie ma takiego pojazdu”.
const NOT_FOUND: i64 = 4413;

/// Pojazd, który serwis przypisał do numeru — z tym, co pokazujemy przy
/// wyborze, gdy kandydatów jest kilka.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub target: Target,
    /// Rodzina modelu, np. „i30 2020~2024”.
    pub model: String,
    /// Rodzaj głowicy, np. „Standard Gen5W Navigation”.
    pub platform: String,
    /// Data wydania najnowszej wersji, np. `2026.07`.
    pub released: String,
    /// Odnośniki „Download” dla tego pojazdu — przy okazji uczą tablicę
    /// odnośników tego, czego jeszcze nie znała.
    pub links: Vec<String>,
}

/// Sprowadza wpisany numer do postaci, którą przyjmuje serwis, albo mówi, że
/// to nie jest VIN. Spacje i myślniki z przepisywania ze dowodu znikają;
/// liter I, O i Q w VIN nie ma, więc ich obecność to literówka.
pub fn normalize(input: &str) -> Option<String> {
    let vin: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();

    let valid = vin.len() == 17
        && vin.chars().all(|c| {
            c.is_ascii_digit() || (c.is_ascii_uppercase() && !matches!(c, 'I' | 'O' | 'Q'))
        });

    valid.then_some(vin)
}

/// Marka zgadnięta z kodu producenta (pierwsze trzy znaki). Tylko po to, żeby
/// zapytać najpierw tam, gdzie odpowiedź jest najbardziej prawdopodobna —
/// pytamy i tak wszystkie trzy.
fn guessed_brand(vin: &str) -> Option<&'static str> {
    const PREFIXES: &[(&str, &str)] = &[
        ("KMT", "genesis"),
        ("KMU", "genesis"),
        ("KMH", "hyundai"),
        ("TMA", "hyundai"),
        ("NLH", "hyundai"),
        ("MAL", "hyundai"),
        ("KM8", "hyundai"),
        ("5NP", "hyundai"),
        ("5NM", "hyundai"),
        ("KNA", "kia"),
        ("KNC", "kia"),
        ("KND", "kia"),
        ("KNE", "kia"),
        ("U5Y", "kia"),
        ("5XY", "kia"),
        ("3KP", "kia"),
    ];

    PREFIXES
        .iter()
        .find(|(prefix, _)| vin.starts_with(prefix))
        .map(|(_, brand)| *brand)
}

/// Kolejność pytania marek: zgadnięta z numeru, potem wybrana w oknie, potem
/// reszta.
pub fn brand_order(vin: &str, selected: &'static str, all: &[&'static str]) -> Vec<&'static str> {
    let mut order: Vec<&'static str> = Vec::with_capacity(all.len());

    for brand in guessed_brand(vin)
        .into_iter()
        .chain([selected])
        .chain(all.iter().copied())
    {
        if !order.contains(&brand) {
            order.push(brand);
        }
    }

    order
}

fn host_for(brand: &str) -> &'static str {
    match brand {
        "kia" => "https://update.kia.com",
        "genesis" => "https://update.genesis.com",
        _ => "https://update.hyundai.com",
    }
}

#[derive(Debug, Deserialize)]
struct Response {
    status: i64,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    data: Option<Vec<Vehicle>>,
}

#[derive(Debug, Deserialize)]
struct Vehicle {
    #[serde(rename = "genNm", default)]
    model: Option<String>,
    #[serde(rename = "swInfo", default)]
    software: Vec<Software>,
}

#[derive(Debug, Deserialize)]
struct Software {
    #[serde(default)]
    mcd: serde_json::Value,
    #[serde(rename = "pfmNm", default)]
    platform: Option<String>,
    #[serde(rename = "postDate", default)]
    released: Option<String>,
    /// „Y”, gdy oprogramowanie da się pobrać — dokładnie ten warunek blokuje
    /// przycisk na stronie.
    #[serde(rename = "downYn", default)]
    downloadable: Option<String>,
    #[serde(rename = "swVer", default)]
    versions: Vec<String>,
    #[serde(rename = "downInfo", default)]
    links: Vec<String>,
}

/// Pyta serwis jednej marki. `Ok(None)` — serwis nie zna tego numeru.
///
/// `brand` w postaci z pliku ulubionych (`hyundai`), `code` w postaci serwisu
/// (`HM`).
pub async fn search(
    brand: &str,
    code: &str,
    region: &str,
    vin: &str,
) -> Result<Option<Vec<Candidate>>> {
    let client = reqwest::Client::builder()
        .user_agent("MapStick")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .with_context(|| tr!("Could not create the HTTP client"))?;

    let host = host_for(brand);
    let url = format!("{host}/api/search/vin");
    let body = serde_json::json!({
        "commonCode": { "brand": code, "reg": region, "lang": "EN" },
        "vin": vin,
    });

    let text = client
        .post(&url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::REFERER, format!("{host}/{region}/EN/home"))
        .json(&body)
        .send()
        .await
        .with_context(|| tr!("Could not query {}", url))?
        .text()
        .await
        .with_context(|| tr!("Unexpected response from {}", url))?;

    let candidates = parse(brand, region, &text)?;
    tracing::info!(
        "VIN in {brand}/{region}: {}",
        match &candidates {
            Some(c) => format!("{} candidates", c.len()),
            None => "unknown".to_owned(),
        }
    );

    Ok(candidates)
}

fn parse(brand: &str, region: &str, text: &str) -> Result<Option<Vec<Candidate>>> {
    let response: Response = serde_json::from_str(text)
        .with_context(|| tr!("Unexpected response from the maker’s VIN search"))?;

    match response.status {
        200 => {}
        NOT_FOUND => return Ok(None),
        // Komunikat serwisu przychodzi po koreańsku niezależnie od języka
        // zapytania — pokazujemy kod, a treść zostaje w dzienniku.
        status => {
            tracing::warn!("VIN search: {status} {:?}", response.message);
            return Err(anyhow!(tr!(
                "The maker’s VIN search answered with code {}",
                status
            )));
        }
    }

    let mut candidates: Vec<Candidate> = Vec::new();

    for vehicle in response.data.unwrap_or_default() {
        let model = vehicle.model.unwrap_or_default();

        for software in vehicle.software {
            if software.downloadable.as_deref() != Some("Y") {
                continue;
            }

            let mcode = match &software.mcd {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => continue,
                other => other.to_string(),
            };

            let candidate = Candidate {
                target: Target {
                    brand: brand.to_owned(),
                    region: region.to_owned(),
                    mcode,
                    name: model.clone(),
                    version: software.versions.first().cloned().unwrap_or_default(),
                },
                model: model.clone(),
                platform: software.platform.unwrap_or_default(),
                released: software.released.unwrap_or_default(),
                links: software.links,
            };

            // Ten sam kod pod dwiema rodzinami modelu to wciąż ten sam wybór.
            if !candidates
                .iter()
                .any(|c| c.target.mcode == candidate.target.mcode)
            {
                candidates.push(candidate);
            }
        }
    }

    Ok(Some(candidates))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kształt prawdziwej odpowiedzi (Kia Ceed PHEV, listopad 2026) z numerem
    /// zastąpionym zerami. Dwa kandydaty na jeden numer to zachowanie serwisu,
    /// nie błąd — stąd okno wyboru.
    const TWO_CANDIDATES: &str = r#"{"status":200,"valid":"N","message":"success","data":[
        {"genNm":"Ceed PHEV 2021","vehlNm":"CEED SPORTSWAGON","vin":"U5Y00000000000000",
         "usrSwVersion":null,"swInfo":[{"swTp":"0","pfmCd":"PFM14",
         "pfmNm":"Standard Gen5W Navigation","postDate":"2026.07","downYn":"Y",
         "downInfo":["com.kia.update.nau://?AAAA"],"mcd":101555,
         "swVer":["CD_P_20.EUR.S5W_L.001.001.260128"]}]},
        {"genNm":"Xceed PHEV 2020~2021","vehlNm":"CEED SPORTSWAGON","vin":"U5Y00000000000000",
         "swInfo":[{"swTp":"0","pfmNm":"Standard Gen5W Navigation","postDate":"2026.07",
         "downYn":"Y","downInfo":["com.kia.update.nau://?BBBB"],"mcd":101585,
         "swVer":["CD_P.EUR.S5W_L.001.001.260128"]},
         {"swTp":"0","pfmNm":"Old","downYn":"N","mcd":1,"swVer":["X"]}]}]}"#;

    #[test]
    fn candidates_come_from_downloadable_software() {
        let found = parse("kia", "EU", TWO_CANDIDATES).unwrap().unwrap();

        assert_eq!(found.len(), 2, "pozycja z downYn=N odpada");
        assert_eq!(found[0].target.mcode, "101555");
        assert_eq!(found[0].target.version, "CD_P_20.EUR.S5W_L.001.001.260128");
        assert_eq!(found[0].model, "Ceed PHEV 2021");
        assert_eq!(found[0].platform, "Standard Gen5W Navigation");
        assert_eq!(found[0].released, "2026.07");
        assert_eq!(found[1].links, ["com.kia.update.nau://?BBBB"]);
        assert_eq!(found[1].target.region, "EU");
    }

    #[test]
    fn an_unknown_vin_is_not_an_error() {
        let text = r#"{"status":4413,"message":"입력하신 정보에 맞는 차량을 찾을 수 없습니다."}"#;

        assert_eq!(parse("hyundai", "EU", text).unwrap(), None);
    }

    #[test]
    fn another_status_is_an_error() {
        let text = r#"{"message":"Full authentication is required","status":401}"#;

        assert!(parse("hyundai", "EU", text).is_err());
    }

    #[test]
    fn typing_mistakes_are_forgiven_but_not_wrong_letters() {
        assert_eq!(
            normalize(" tmah 0000-000 000001 ").as_deref(),
            Some("TMAH0000000000001")
        );
        assert_eq!(normalize("TMAH000000000000"), None, "16 znaków");
        assert_eq!(
            normalize("TMAH000000000000O"),
            None,
            "O nie występuje w VIN"
        );
    }

    #[test]
    fn the_guessed_brand_is_asked_first() {
        let all = ["hyundai", "kia", "genesis"];

        assert_eq!(
            brand_order("U5YH0000000000000", "hyundai", &all),
            ["kia", "hyundai", "genesis"]
        );
        assert_eq!(
            brand_order("XXXH0000000000000", "genesis", &all),
            ["genesis", "hyundai", "kia"]
        );
    }
}
