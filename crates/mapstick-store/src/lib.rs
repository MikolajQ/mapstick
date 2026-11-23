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
    /// Data wydania najnowszej wersji, tak jak podaje ją serwis (np. `2026.07`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_released: Option<String>,

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

    /// Czy sprawdzać ten model w tle, bez uruchamiania aplikacji.
    #[serde(default)]
    pub watch: bool,
    /// Wersja, o której ostatnio powiadomiliśmy, i kiedy. Przypominamy
    /// codziennie, dopóki auto jej nie ma — ale nie częściej, choćby coś
    /// budziło sprawdzanie co godzinę.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notified_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notified_at: Option<String>,
}

/// Jak często sprawdzać aktualizacje w tle.
///
/// Producent wydaje mapy kilka razy w roku, więc częściej niż raz na tydzień
/// nie ma czego szukać — ale to wybór użytkownika, nie nasz.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interval {
    Daily,
    #[default]
    Weekly,
    Monthly,
}

impl Interval {
    pub const ALL: &'static [Interval] = &[Interval::Daily, Interval::Weekly, Interval::Monthly];

    /// Angielski klucz tłumaczenia — patrz `mapstick::i18n`. Ta biblioteka
    /// nie zależy od Slinta, więc samo tłumaczenie robi wywołujący.
    pub fn label(self) -> &'static str {
        match self {
            Interval::Daily => "daily",
            Interval::Weekly => "weekly",
            Interval::Monthly => "monthly",
        }
    }

    /// Długość odstępu w godzinach — znacznik czasu to punkt na osi, nie data
    /// w kalendarzu, więc „dni” nie mają tu sensu.
    pub fn hours(self) -> i64 {
        match self {
            Interval::Daily => 24,
            Interval::Weekly => 7 * 24,
            Interval::Monthly => 30 * 24,
        }
    }
}

/// Wariant zestawu multimedialnego, wyczytany z numeru wersji.
///
/// Jeden model potrafi mieć dwa: `NE1.ERT.ccNC_Lite.001.002.260312`
/// i `NE1.EUR.ccNC.001.002.260312` to ten sam IONIQ 5 z dwoma różnymi
/// głowicami. Producent wystawia obie naraz, a jedno pobranie niesie pliki
/// dla obu — auto bierze swoje. Porównywanie ich ze sobą nie ma sensu:
/// „nowsze” trzeba liczyć osobno w każdej rodzinie.
///
/// Rodzinę wyznaczają trzy pierwsze człony; reszta to numer wydania.
pub fn variant(version: &str) -> &str {
    let mut end = 0;

    for (count, part) in version.split('.').enumerate().take(3) {
        if count > 0 {
            end += 1;
        }
        end += part.len();
    }

    &version[..end]
}

impl Favourite {
    /// Wersja, którą uznajemy za obecną w aucie.
    ///
    /// Potwierdzona ręcznie bije wszystko inne: tylko użytkownik wie, co auto
    /// naprawdę połknęło. Dalej to, co odczytaliśmy z nośnika albo pobraliśmy
    /// — lepsze niż nic, choć żadne z nich nie jest dowodem.
    pub fn known_version(&self) -> Option<&str> {
        self.installed_version
            .as_deref()
            .or(self.downloaded_version.as_deref())
    }

    /// Która z wystawionych wersji dotyczy tego auta.
    ///
    /// Jeśli wiemy, co w nim siedzi, trzymamy się tej samej rodziny. Bez tej
    /// wiedzy zostaje pierwsza z listy — i tak trzeba coś pokazać, a i tak
    /// prosimy wtedy o zaznaczenie wersji.
    pub fn matching<'a>(&self, versions: &'a [String]) -> Option<&'a String> {
        if let Some(known) = self.known_version()
            && let Some(same) = versions.iter().find(|v| variant(v) == variant(known))
        {
            return Some(same);
        }

        versions.first()
    }

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

/// Gotowe oprogramowanie leżące już na dysku.
///
/// Zapisujemy je osobno od ulubionych, bo pobrać można dowolny model, także
/// taki, którego nikt nie oznaczył gwiazdką. Bez tego wpisu aplikacja po
/// ponownym uruchomieniu nie wie, że dane już są, i jedyne, co potrafi
/// zaproponować, to pobranie ich drugi raz.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Download {
    pub model_id: String,
    /// Nazwa modelu w chwili pobrania — do pokazania, gdy katalogu już nie ma.
    pub label: String,
    pub brand: String,
    pub region: String,
    pub version: String,
    /// Katalog z rozpakowanym wynikiem.
    pub path: PathBuf,
    pub at: String,
}

/// Ślad po sprawdzaniu w tle.
///
/// Zapisywany przy każdym przebiegu, także nieudanym. Bez niego cisza znaczy
/// dwie różne rzeczy naraz — „nie ma nic nowego” i „sprawdzanie od miesiąca
/// nie działa” — i nie da się ich od siebie odróżnić.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Background {
    /// Ostatnie uruchomienie przez harmonogram, udane czy nie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ran_at: Option<String>,
    /// Ostatnia odpowiedź producenta uzyskana w tle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<String>,
    /// Co poszło nie tak przy ostatnim pytaniu. `None` — nic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Kiedy ostatnio daliśmy znać, że czuwamy. Liczy się każde powiadomienie,
    /// także to o nowym wydaniu.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_at: Option<String>,
}

fn default_report_interval() -> Option<Interval> {
    Some(Interval::Weekly)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Store {
    /// Numer schematu, od pierwszego dnia — żeby późniejsza zmiana formatu nie
    /// wymagała zgadywania, co się czyta.
    pub version: u32,
    /// Jak często pyta o nowe wydania sprawdzanie w tle.
    #[serde(default)]
    pub check_interval: Interval,
    /// Jak często sprawdzanie w tle melduje, że działa. `None` — nigdy.
    ///
    /// Meldunek jest po to, żeby jego **brak** był widoczny: usługa, której
    /// system nie potrafi uruchomić, nie powie o tym sama.
    #[serde(default = "default_report_interval")]
    pub report_interval: Option<Interval>,
    #[serde(default)]
    pub background: Background,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_selection: Option<LastSelection>,
    #[serde(default)]
    pub items: Vec<Favourite>,
    #[serde(default)]
    pub downloads: Vec<Download>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            version: 1,
            check_interval: Interval::default(),
            report_interval: default_report_interval(),
            background: Background::default(),
            last_selection: None,
            items: Vec::new(),
            downloads: Vec::new(),
        }
    }
}

impl Store {
    /// Modele, o które użytkownik prosił, żeby ich pilnować.
    pub fn watched(&self) -> impl Iterator<Item = &Favourite> {
        self.items.iter().filter(|f| f.watch)
    }

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

    /// Zapisuje pobranie, zastępując wcześniejsze dla tego samego modelu.
    ///
    /// Jeden model to jedno miejsce na dysku: starsze wydanie w innym katalogu
    /// i tak nie ma po co być pamiętane, bo aplikacja proponuje wyłącznie to,
    /// co zgadza się z bieżącą wersją u producenta.
    pub fn record_download(&mut self, download: Download) {
        self.downloads.retain(|d| d.model_id != download.model_id);
        self.downloads.push(download);
    }

    pub fn download_for(&self, model_id: &str) -> Option<&Download> {
        self.downloads.iter().find(|d| d.model_id == model_id)
    }

    pub fn forget_download(&mut self, model_id: &str) -> bool {
        let before = self.downloads.len();
        self.downloads.retain(|d| d.model_id != model_id);

        self.downloads.len() != before
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

/// Wyłączny dostęp do pliku ulubionych na czas odczytu i zapisu.
///
/// Sprawdzanie w tle i otwarte okno to dwa procesy piszące ten sam plik.
/// Zapis jest całościowy (plik tymczasowy i podmiana), więc bez blokady jeden
/// nadpisałby zmiany drugiego — i to akurat te, dla których sprawdzanie
/// w ogóle się uruchomiło.
pub struct Lock(std::fs::File);

/// Bierze blokadę obok pliku ulubionych. Osobny plik, bo zapis podmienia sam
/// plik ulubionych i blokada na nim zniknęłaby razem z podmienianym i-węzłem.
pub fn lock(path: &Path) -> Result<Lock> {
    let path = path.with_extension("lock");

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Write(parent.to_owned(), e))?;
    }

    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| Error::Write(path.clone(), e))?;

    fs4::fs_std::FileExt::lock_exclusive(&file).map_err(|e| Error::Write(path.clone(), e))?;

    Ok(Lock(file))
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs4::fs_std::FileExt::unlock(&self.0);
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

    let directory = base.join("mapstick");
    let path = directory.join("favourites.json");

    // Do wersji 0.1 plik nazywał się po polsku.
    adopt_legacy(&directory.join("ulubione.json"), &path);
    let _ = std::fs::remove_file(directory.join("ulubione.lock"));

    path
}

/// Przejmuje plik albo katalog spod starej nazwy, jeśli pod nową jeszcze nic
/// nie ma. Zmiana nazwy jest atomowa, więc dwa procesy naraz (okno i
/// sprawdzanie w tle) nie zrobią sobie krzywdy — drugi po prostu nie zastanie
/// już starej nazwy.
pub fn adopt_legacy(old: &Path, new: &Path) {
    if new.exists() || !old.exists() {
        return;
    }

    if let Err(e) = std::fs::rename(old, new) {
        // Pierwszeństwo ma działający program: najwyżej zacznie od zera.
        eprintln!("MapStick: nie przeniosę {} do {}: {e}", old.display(), new.display());
    }
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
            latest_released: None,
            downloaded_version: None,
            downloaded_at: None,
            installed_version: None,
            installed_source: None,
            installed_confirmed: false,
            vin: None,
            watch: false,
            notified_version: None,
            notified_at: None,
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

    fn sample_download() -> Download {
        Download {
            model_id: "New_IONIQ5_EUR".to_owned(),
            label: "New IONIQ 5".to_owned(),
            brand: "hyundai".to_owned(),
            region: "EU".to_owned(),
            version: "NE1.ERT.ccNC_Lite.001.002.260312".to_owned(),
            path: PathBuf::from("/dane/ioniq"),
            at: now(),
        }
    }

    /// Jeden model to jedno miejsce na dysku — powtórne pobranie w inne
    /// miejsce zastępuje wpis, a nie dokłada drugiego.
    #[test]
    fn recording_a_download_replaces_the_previous_one() {
        let mut store = Store::default();
        store.record_download(sample_download());

        let mut again = sample_download();
        again.path = PathBuf::from("/dane/nowe");
        store.record_download(again);

        assert_eq!(store.downloads.len(), 1);
        assert_eq!(
            store.download_for("New_IONIQ5_EUR").unwrap().path,
            PathBuf::from("/dane/nowe")
        );
    }

    /// Pobrania przeżywają zapis do pliku, inaczej po ponownym uruchomieniu
    /// aplikacja znów nie wiedziałaby, że dane leżą na dysku.
    #[test]
    fn downloads_survive_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ulubione.json");

        let mut store = Store::default();
        store.record_download(sample_download());
        store.save(&path).unwrap();

        let mut loaded = Store::load(&path).unwrap();
        assert_eq!(loaded.downloads.len(), 1);
        assert!(loaded.forget_download("New_IONIQ5_EUR"));
    }

    /// Plik sprzed dopisania pobrań musi się dalej wczytywać.
    #[test]
    fn older_files_load_without_the_downloads_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stare.json");
        std::fs::write(&path, r#"{"version":1,"items":[]}"#).unwrap();

        let store = Store::load(&path).unwrap();
        assert!(store.downloads.is_empty());
    }

    #[test]
    fn the_variant_is_the_first_three_parts() {
        assert_eq!(
            variant("NE1.ERT.ccNC_Lite.001.002.260312"),
            "NE1.ERT.ccNC_Lite"
        );
        assert_eq!(variant("NE1.EUR.ccNC.001.002.260312"), "NE1.EUR.ccNC");
        assert_eq!(variant("BC3.EUR.S5W_L.001.001.260128"), "BC3.EUR.S5W_L");
        assert_eq!(variant("DH.EUR.SOP.XX.017"), "DH.EUR.SOP");
        assert_eq!(variant("krotkie"), "krotkie");
        assert_eq!(variant(""), "");
    }

    /// Dwie głowice, dwa numery — auto interesuje wyłącznie jego własny.
    #[test]
    fn the_matching_version_stays_in_the_same_family() {
        let versions = vec![
            "NE1.EUR.ccNC.001.002.260312".to_owned(),
            "NE1.ERT.ccNC_Lite.001.002.260312".to_owned(),
        ];

        let mut f = sample();
        f.installed_version = Some("NE1.ERT.ccNC_Lite.001.001.250101".to_owned());

        assert_eq!(
            f.matching(&versions).unwrap(),
            "NE1.ERT.ccNC_Lite.001.002.260312"
        );
    }

    /// Bez wiedzy o aucie bierzemy pierwszą z listy — nie ma lepszej odpowiedzi.
    #[test]
    fn without_a_known_version_the_first_one_stands_in() {
        let versions = vec![
            "NE1.EUR.ccNC.001.002.260312".to_owned(),
            "NE1.ERT.ccNC_Lite.001.002.260312".to_owned(),
        ];

        assert_eq!(
            sample().matching(&versions).unwrap(),
            "NE1.EUR.ccNC.001.002.260312"
        );
    }

    /// Rodzina, której producent już nie wystawia, nie może udawać, że jest.
    #[test]
    fn an_unlisted_family_falls_back_to_the_first() {
        let versions = vec!["NE1.EUR.ccNC.001.002.260312".to_owned()];

        let mut f = sample();
        f.installed_version = Some("XX.YY.ZZ.001".to_owned());

        assert_eq!(f.matching(&versions).unwrap(), "NE1.EUR.ccNC.001.002.260312");
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

    /// Stary plik przechodzi pod nową nazwę, ale nigdy nie nadpisuje nowego.
    #[test]
    fn a_legacy_file_is_adopted_once() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = (dir.path().join("ulubione.json"), dir.path().join("favourites.json"));

        std::fs::write(&old, "stary").unwrap();
        adopt_legacy(&old, &new);
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "stary");
        assert!(!old.exists());

        std::fs::write(&old, "jeszcze starszy").unwrap();
        adopt_legacy(&old, &new);
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "stary");
    }

    /// Plik sprzed meldunków dostaje meldunek co tydzień — a wyłączony
    /// świadomie zostaje wyłączony.
    #[test]
    fn report_interval_defaults_to_weekly_but_keeps_never() {
        let old: Store = serde_json::from_str(r#"{"version":1}"#).unwrap();
        assert_eq!(old.report_interval, Some(Interval::Weekly));

        let never: Store = serde_json::from_str(r#"{"version":1,"report_interval":null}"#).unwrap();
        assert_eq!(never.report_interval, None);

        let text = serde_json::to_string(&never).unwrap();
        let again: Store = serde_json::from_str(&text).unwrap();
        assert_eq!(again.report_interval, None);
    }
}
