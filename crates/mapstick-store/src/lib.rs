// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! Ulubione pojazdy i zapamiętany stan interfejsu.
//!
//! Plik jest czytelny dla człowieka i przenośny między maszynami — to celowe.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Nie udało się odczytać {0:?}")]
    Read(PathBuf, #[source] std::io::Error),
    #[error("Nie udało się zapisać {0:?}")]
    Write(PathBuf, #[source] std::io::Error),
    #[error("Niepoprawny format pliku {0:?}")]
    Parse(PathBuf, #[source] serde_json::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Skąd wiemy, co jest zainstalowane w aucie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstalledSource {
    /// Odczyt pliku `.ver` z nośnika albo katalogu pobrań — źródło najpewniejsze.
    VerFile,
    /// To, co ta aplikacja sama pobrała. Nie dowodzi, że trafiło do auta.
    History,
    /// Pole `usrSwVer` z zapytania o VIN.
    Vin,
    /// Wskazane ręcznie przez użytkownika.
    Manual,
}

/// Zapamiętany pojazd.
///
/// Trzy pary pól zamiast jednej wersji, bo to trzy różne fakty: co wystawia
/// producent, co pobraliśmy i co (naszym zdaniem) siedzi w aucie. Sklejenie ich
/// w jedno pole prowadziłoby wprost do twierdzenia „masz aktualne” w sytuacji,
/// gdy ktoś zaktualizował auto innym nośnikiem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Favourite {
    pub label: String,
    pub brand: String,
    pub region: String,
    pub model_id: String,
    pub model_name: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_seen_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_checked_at: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloaded_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloaded_at: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_source: Option<InstalledSource>,
    #[serde(default)]
    pub installed_confirmed: bool,

    /// Zapisywany wyłącznie za zgodą użytkownika.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vin: Option<String>,
}

impl Favourite {
    /// Czy producent wystawia coś nowszego niż to, co mamy.
    ///
    /// `None` znaczy „nie wiadomo” i tak też ma być pokazane — aplikacja nigdy
    /// nie twierdzi „masz aktualne”, jeśli tego nie wie.
    pub fn has_update(&self) -> Option<bool> {
        let latest = self.latest_seen_version.as_ref()?;
        let known = self
            .installed_version
            .as_ref()
            .or(self.downloaded_version.as_ref())?;

        Some(latest != known)
    }
}

/// Ostatnio oglądany model — do odtworzenia ekranu po uruchomieniu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastSelection {
    pub brand: String,
    pub region: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Store {
    /// Numer schematu, od pierwszego dnia — żeby późniejsza zmiana formatu nie
    /// wymagała zgadywania, co się czyta.
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_selection: Option<LastSelection>,
    #[serde(default)]
    pub items: Vec<Favourite>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            version: 1,
            last_selection: None,
            items: Vec::new(),
        }
    }
}

impl Store {
    pub fn find(&self, model_id: &str) -> Option<&Favourite> {
        self.items.iter().find(|f| f.model_id == model_id)
    }

    pub fn contains(&self, model_id: &str) -> bool {
        self.find(model_id).is_some()
    }

    /// Dodaje albo podmienia wpis o tym samym identyfikatorze modelu.
    pub fn upsert(&mut self, favourite: Favourite) {
        match self
            .items
            .iter_mut()
            .find(|f| f.model_id == favourite.model_id)
        {
            Some(existing) => *existing = favourite,
            None => self.items.push(favourite),
        }
    }

    pub fn remove(&mut self, model_id: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|f| f.model_id != model_id);

        self.items.len() != before
    }

    pub fn get_mut(&mut self, model_id: &str) -> Option<&mut Favourite> {
        self.items.iter_mut().find(|f| f.model_id == model_id)
    }

    /// Wczytuje z pliku. Brak pliku to nie błąd — to pierwsze uruchomienie.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(Error::Read(path.to_owned(), e)),
        };

        serde_json::from_str(&text).map_err(|e| Error::Parse(path.to_owned(), e))
    }

    /// Zapisuje przez plik tymczasowy, żeby przerwanie w połowie nie zostawiło
    /// obciętego pliku zamiast listy ulubionych.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Write(parent.to_owned(), e))?;
        }

        let text = serde_json::to_string_pretty(self)
            .expect("Struktura ulubionych zawsze się serializuje");

        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, text).map_err(|e| Error::Write(temp.clone(), e))?;
        std::fs::rename(&temp, path).map_err(|e| Error::Write(path.to_owned(), e))?;

        Ok(())
    }
}

/// Znacznik czasu w formacie RFC 3339.
pub fn now() -> String {
    jiff::Timestamp::now().to_string()
}

/// Domyślne miejsce na plik ulubionych.
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(std::env::temp_dir);

    base.join("mapstick").join("ulubione.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Favourite {
        Favourite {
            label: "IONIQ 5".to_owned(),
            brand: "hyundai".to_owned(),
            region: "EU".to_owned(),
            model_id: "New_IONIQ5_EUR".to_owned(),
            model_name: "New IONIQ 5".to_owned(),
            latest_seen_version: None,
            latest_checked_at: None,
            downloaded_version: None,
            downloaded_at: None,
            installed_version: None,
            installed_source: None,
            installed_confirmed: false,
            vin: None,
        }
    }

    #[test]
    fn missing_file_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::load(&dir.path().join("nie-ma.json")).unwrap();

        assert!(store.items.is_empty());
        assert_eq!(store.version, 1);
    }

    #[test]
    fn round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ulubione.json");

        let mut store = Store::default();
        store.upsert(sample());
        store.save(&path).unwrap();

        let loaded = Store::load(&path).unwrap();
        assert_eq!(loaded.items.len(), 1);
        assert_eq!(loaded.items[0].model_id, "New_IONIQ5_EUR");
    }

    #[test]
    fn upsert_replaces_instead_of_duplicating() {
        let mut store = Store::default();
        store.upsert(sample());

        let mut second = sample();
        second.label = "Auto żony".to_owned();
        store.upsert(second);

        assert_eq!(store.items.len(), 1);
        assert_eq!(store.items[0].label, "Auto żony");
    }

    /// Brak wiedzy musi zostać brakiem wiedzy, a nie zgadywaniem.
    #[test]
    fn update_state_is_unknown_without_both_versions() {
        let mut f = sample();
        assert_eq!(f.has_update(), None);

        f.latest_seen_version = Some("A".to_owned());
        assert_eq!(f.has_update(), None);

        f.installed_version = Some("A".to_owned());
        assert_eq!(f.has_update(), Some(false));

        f.latest_seen_version = Some("B".to_owned());
        assert_eq!(f.has_update(), Some(true));
    }

    /// Gdy nie wiemy, co w aucie, ale wiemy, co sami pobraliśmy — to lepsze
    /// niż nic, choć nie dowodzi, że trafiło do auta.
    #[test]
    fn downloaded_version_stands_in_when_installed_is_unknown() {
        let mut f = sample();
        f.latest_seen_version = Some("B".to_owned());
        f.downloaded_version = Some("A".to_owned());

        assert_eq!(f.has_update(), Some(true));
    }
}
