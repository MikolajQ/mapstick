// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! MapStick — graficzna nakładka na nudl.

// Wydanie dla Windows bez okna konsoli: inaczej czarne okno stałoby obok
// aplikacji, a codzienne sprawdzenie z Harmonogramu błyskałoby nim co rano.
// Wersja debug je zachowuje — tam log w konsoli to zaleta.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, anyhow};
use cap_std::{ambient_authority, fs::Dir};
use mapstick_media::Medium;
use mapstick_store::{Download, Favourite, InstalledSource, LastSelection, Store};
use nudl::{
    cli::Brand,
    client::{CarInfo, FirmwareInfo, NuClientBuilder},
    download::{Downloader, ProgressMessage},
};
use slint::{ModelRc, SharedString, VecModel, Weak};
use tokio::{
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
    time::Instant,
};

mod cache;
mod copy;
mod i18n;
mod instance;
mod link;
mod schedule;
mod stale;
mod watch;

slint::include_modules!();

/// Regiony obsługiwane przez serwis producenta, w kolejności wyświetlania.
const REGIONS: &[(&str, &str)] = &[
    ("EU", "Europe"),
    ("BR", "Brazil"),
    ("CA", "Canada"),
    ("ID", "Indonesia"),
    ("IN", "India"),
    ("JP", "Japan"),
    ("KR", "South Korea"),
    ("ME", "Middle East"),
    ("NZ", "New Zealand"),
    ("RU", "Russia and CIS"),
    ("SG", "Singapore"),
    ("TR", "Türkiye"),
    ("US", "United States"),
];

const BRANDS: &[Brand] = &[Brand::Hyundai, Brand::Kia, Brand::Genesis];
/// Nazwy marek zapisywane w pliku ulubionych — stabilne, niezależne od kolejności
/// w liście rozwijanej.
const BRAND_NAMES: &[&str] = &["hyundai", "kia", "genesis"];

const CONCURRENCY: usize = 4;
const RETRIES: u8 = 3;
const UI_REFRESH: Duration = Duration::from_millis(100);

/// Polecenia płynące z wątku interfejsu do zadania sieciowego.
#[derive(Debug)]
enum Cmd {
    /// `restore` fałszywe, gdy zaraz przyjdzie odnośnik — odtwarzanie
    /// ostatniego modelu byłoby wtedy pobraniem katalogu na darmo.
    Startup { restore: bool },
    ToggleFavourite,
    SetWatch(bool),
    SetInterval(usize),
    SetInstalled(usize),
    OpenFavourite(String),
    LoadModels { brand: usize, region: usize },
    Filter(String),
    Select(usize),
    StartDownload,
    Cancel,
    /// Przerwanie razem ze sprzątnięciem tego, co zdążyło się pobrać.
    Abandon,
    SetDirectMode(bool),
    ClearCache,
    GoToMedia,
    GoBack,
    RefreshMedia,
    SelectMedium(usize),
    RequestFormat(usize),
    RemoveStale(usize),
    ConfirmFormat,
    CancelFormat,
    RequestRedownload,
    ConfirmRedownload,
    CancelRedownload,
    FavouritesRefreshed(Vec<FavouriteVersion>),
    DownloadFinished(Box<Download>),
    StartCopy,
    /// Wskazanie celu z pominięciem okna wyboru — wyłącznie do testów.
    UseDirectory(PathBuf),
    SetDeleteAfterCopy(bool),
    /// Odnośnik ze strony producenta, kliknięty przez użytkownika.
    OpenLink(String),
    /// Świeżo zebrane odnośniki z katalogu producenta.
    LinksCollected(Vec<(String, link::Target)>),
    /// Kopiowanie dobiegło końca — także przerwane albo nieudane.
    /// `forget` niesie model, którego dane zniknęły z dysku.
    CopyEnded { forget: Option<String> },
}

/// Ślad trwającego pobrania: co po nim usunąć, jeśli zostanie anulowane.
///
/// Zbieramy to w chwili startu, bo w chwili anulowania użytkownik może już
/// oglądać inny model albo inny katalog.
#[derive(Debug, Clone)]
struct Footprint {
    model_id: String,
    output: PathBuf,
    /// Katalog archiwów. Przy pobieraniu prosto na nośnik nie ma osobnego —
    /// archiwa leżą wtedy w katalogu docelowym.
    work: Option<PathBuf>,
}

/// Czym skończyło się zadanie kopiujące.
enum CopyEnd {
    /// `removed`: `None` — nie proszono o usunięcie źródła; `Some` — proszono,
    /// a wartość mówi, czy do niego doszło.
    Done {
        model_id: String,
        removed: Option<bool>,
    },
    Cancelled,
}

/// Najnowsze wydanie ulubionego modelu, przyniesione przez zadanie w tle.
#[derive(Debug)]
struct FavouriteVersion {
    model_id: String,
    /// Wszystkie wystawione wersje, bo jeden model bywa w dwóch odmianach
    /// głowicy. Którą wybrać, wie dopiero wpis w ulubionych.
    versions: Vec<String>,
    released: Option<String>,
}

/// Wiersz listy wzbogacony o dane, których nie ma w znormalizowanym `CarInfo`.
struct Entry {
    car: CarInfo,
    media: String,
    released: String,
}

impl Entry {
    fn matches(&self, needle: &str) -> bool {
        let hay = [
            self.car.name.as_str(),
            self.car.model.as_str(),
            self.car.id.as_str(),
        ];

        hay.iter().any(|h| h.to_lowercase().contains(needle))
            || self
                .car
                .versions
                .iter()
                .any(|v| v.to_lowercase().contains(needle))
    }

    fn to_row(&self) -> ModelRow {
        ModelRow {
            id: self.car.id.as_str().into(),
            name: self.car.name.as_str().into(),
            version: self.car.versions.join(", ").into(),
            media: self.media.as_str().into(),
            released: self.released.as_str().into(),
        }
    }
}

fn brand_index(name: &str) -> usize {
    BRAND_NAMES.iter().position(|b| *b == name).unwrap_or(0)
}

fn region_index(code: &str) -> usize {
    REGIONS.iter().position(|(c, _)| *c == code).unwrap_or(0)
}

/// Usuwa niedokończone archiwa leżące w katalogu docelowym.
///
/// Przy pobieraniu prosto na nośnik nie ma osobnego katalogu na archiwa, więc
/// zostają tam, gdzie trafił wynik. Rozpoznajemy je po rozszerzeniach, których
/// nie nadaje nic poza samym pobieraniem.
fn remove_partial_archives(directory: &Path) -> u64 {
    const PARTIAL: &[&str] = &["nudl_download", "nudl_verify"];

    let Ok(entries) = std::fs::read_dir(directory) else {
        return 0;
    };

    let mut freed = 0;

    for entry in entries.flatten() {
        let path = entry.path();

        let partial = path
            .extension()
            .is_some_and(|e| PARTIAL.iter().any(|p| e.eq_ignore_ascii_case(p)));

        if !partial || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);

        match std::fs::remove_file(&path) {
            Ok(()) => freed += size,
            Err(e) => tracing::warn!("Nie udało się usunąć {}: {e}", path.display()),
        }
    }

    freed
}

/// Nazwa nośnika do pokazania. Napęd bez producenta i modelu przychodzi
/// z pustą nazwą, a podpis zastępczy tłumaczymy tutaj — `mapstick-media`
/// nie ma tłumaczeń.
fn medium_label(medium: &Medium) -> String {
    if medium.label.is_empty() {
        tr!("Unnamed medium")
    } else {
        medium.label.clone()
    }
}

/// Błąd do pokazania w oknie, w języku okna.
///
/// `mapstick-media` i `mapstick-store` nie mają tłumaczeń — opisują błędy
/// po polsku, na potrzeby dziennika — więc ich warianty tłumaczymy tutaj.
/// Resztę (nudl, system) pokazujemy tak, jak przyszła: komunikaty Windows
/// i tak są w języku systemu.
fn describe(error: &(dyn std::error::Error + 'static)) -> String {
    use mapstick_media::Error as M;
    use mapstick_store::Error as S;

    if let Some(e) = error.downcast_ref::<M>() {
        return match e {
            M::ReadDir(path, _) => tr!("Could not read the folder {}", path.display()),
            M::ReadFile(path, _) => tr!("Could not read the file {}", path.display()),
            M::VerFileCount(path, count) => {
                tr!("The folder {} holds {} .ver files instead of one", path.display(), count)
            }
            M::InvalidVerFile(path, _) => tr!("Invalid .ver file: {}", path.display()),
            M::Space(path, _) => tr!("Could not check the free space in {}", path.display()),
            M::NotMounted(medium) => tr!("The medium {} is not mounted", medium),
            M::SystemDisk(medium) => {
                tr!("Refused: {} holds system data and cannot be a target", medium)
            }
            M::Backend(source) => tr!("Could not talk to the disk service: {}", source),
            M::Service(reason) => tr!("The disk service refused: {}", reason),
            M::Cancelled => tr!("cancelled in the permission prompt"),
            M::Unsupported => tr!("This system is not supported yet"),
        };
    }

    if let Some(e) = error.downcast_ref::<S>() {
        return match e {
            S::Read(path, _) => tr!("Could not read {}", path.display()),
            S::Write(path, _) => tr!("Could not write {}", path.display()),
            S::Parse(path, _) => tr!("The file {} has an invalid format", path.display()),
        };
    }

    error.to_string()
}

/// Ile bajtów poleci przez sieć i ile zajmie na nośniku.
///
/// To dwie różne liczby: pliki przychodzą spakowane, a na nośnik trafiają
/// rozpakowane. Zbiorcze `FirmwareInfo::size` opisuje ten drugi rozmiar, ale
/// serwis zwraca w nim zero dla każdego sprawdzonego modelu (`Bayon_EU`:
/// pole zbiorcze 0 przy 25,5 GiB w 35 plikach), więc liczymy z listy plików,
/// a pole zbiorcze zostawiamy jako zapasowe. Zgłoszone jako nudl#68.
fn firmware_totals(firmware: &FirmwareInfo) -> (u64, u64) {
    let download = firmware.files.iter().map(|f| f.download_size()).sum();

    let extracted: u64 = firmware.files.iter().map(|f| f.size).sum();
    let extracted = if extracted > 0 {
        extracted
    } else {
        firmware.size
    };

    (download, extracted)
}

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;

    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Pobiera katalog modeli w postaci surowej i przerabia na [`Entry`].
///
/// Surowa odpowiedź niesie `mediaType` i `releaseDate`, których nie ma
/// w znormalizowanym `CarInfo` — inaczej trzeba by o te same dane pytać
/// serwis dwa razy.
///
/// Wolna funkcja, nie metoda: sprawdzanie ulubionych dzieje się w osobnym
/// zadaniu, które nie ma dostępu do [`Worker`].
async fn fetch_entries(brand_code: &str, region: &str) -> Result<Vec<Entry>> {
    let client = NuClientBuilder::new().build()?;
    client.validate_region(brand_code, region).await?;
    let guid = client.get_guid(region).await?;
    let raw = client.get_cars_raw(region, &guid, brand_code).await?;

    let mut entries = Vec::with_capacity(raw.cars.len());
    for car in raw.cars {
        let media = car.media_type.clone();
        let released = car.release_date.clone().unwrap_or_default();

        // Pojedyncze auto z nieoczekiwanym kształtem danych nie może
        // wywrócić całej listy.
        match CarInfo::try_from(car) {
            Ok(info) => entries.push(Entry {
                car: info,
                media,
                released,
            }),
            Err(e) => tracing::warn!("Pomijam pojazd o nieoczekiwanych danych: {e}"),
        }
    }

    Ok(entries)
}

/// Stan zadania sieciowego. Żyje w całości poza wątkiem interfejsu.
struct Worker {
    ui: Weak<AppWindow>,
    /// Nadajnik poleceń — zadania w tle odsyłają nim wyniki do pętli.
    tx: UnboundedSender<Cmd>,
    brand: Brand,
    region: String,
    entries: Vec<Entry>,
    filtered: Vec<usize>,
    filter: String,
    selected: Option<usize>,
    firmware: Option<FirmwareInfo>,
    output_dir: Option<PathBuf>,
    /// Nośnik kliknięty na liście. Zapamiętany po ścieżce, bo numery wierszy
    /// zmieniają się przy każdym odświeżeniu, a wybór ma przeżyć wskazanie
    /// katalogu na dysku twardym.
    chosen_medium: Option<PathBuf>,
    /// Wersje pod listą „w aucie”, w kolejności pozycji. `None` to „nie wiem”.
    installed_choices: Vec<Option<String>>,
    running: Option<Footprint>,
    download: Option<JoinHandle<()>>,
    media: Vec<Medium>,
    pending_format: Option<usize>,
    /// Gdy prawda, archiwa lądują na nośniku razem z wynikiem.
    direct_to_medium: bool,
    store: Store,
    store_path: PathBuf,
    /// Model do zaznaczenia, gdy tylko lista modeli będzie gotowa.
    pending_select: Option<String>,
    /// Przełącznik przerwania kopiowania. Kopiowanie jest blokujące, więc nie
    /// da się go zdjąć zwykłym `abort()` — musi samo zauważyć, że ma przestać.
    copy_cancel: Option<Arc<AtomicBool>>,
    /// Czy po skopiowaniu skasować dane z dysku (czyli „przenieś”).
    delete_after_copy: bool,
    /// Odnośnik → pojazd, zbierane z katalogu producenta.
    link_table: link::Table,
    /// Model do zaznaczenia po kodzie `mcode`, gdy przyjdzie z odnośnika.
    pending_mcode: Option<String>,
}

impl Worker {
    fn new(ui: Weak<AppWindow>, tx: UnboundedSender<Cmd>) -> Self {
        Self {
            ui,
            tx,
            brand: Brand::Hyundai,
            region: REGIONS[0].0.to_owned(),
            entries: Vec::new(),
            filtered: Vec::new(),
            filter: String::new(),
            selected: None,
            firmware: None,
            output_dir: None,
            chosen_medium: None,
            installed_choices: Vec::new(),
            running: None,
            download: None,
            media: Vec::new(),
            pending_format: None,
            direct_to_medium: false,
            store: Store::default(),
            store_path: mapstick_store::default_path(),
            pending_select: None,
            copy_cancel: None,
            delete_after_copy: false,
            link_table: link::Table::default(),
            pending_mcode: None,
        }
    }

    /// Wykonuje domknięcie na wątku interfejsu.
    fn with_ui(&self, f: impl FnOnce(AppWindow) + Send + 'static) {
        let _ = self.ui.upgrade_in_event_loop(f);
    }

    fn set_status(&self, text: impl Into<String>, is_error: bool) {
        let text = text.into();

        if is_error {
            tracing::warn!("{text}");
        } else {
            tracing::info!("{text}");
        }

        self.with_ui(move |ui| {
            ui.set_status(text.into());
            ui.set_status_is_error(is_error);
        });
    }

    fn set_busy(&self, busy: bool) {
        self.with_ui(move |ui| ui.set_busy(busy));
    }

    fn push_rows(&mut self) {
        let needle = self.filter.to_lowercase();

        self.filtered = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| needle.is_empty() || e.matches(&needle))
            .map(|(i, _)| i)
            .collect();

        let rows: Vec<ModelRow> = self
            .filtered
            .iter()
            .map(|&i| self.entries[i].to_row())
            .collect();
        let total = self.entries.len() as i32;

        self.with_ui(move |ui| {
            ui.set_models(ModelRc::new(VecModel::from(rows)));
            ui.set_total_count(total);
            ui.set_selected_index(-1);
            ui.set_detail_ready(false);
        });

        self.selected = None;
        self.firmware = None;
    }

    // ---------------- ulubione ----------------

    /// Pozycja bieżącej marki w [`BRANDS`].
    fn brand_position(&self) -> usize {
        BRANDS
            .iter()
            .position(|b| b.as_code_str() == self.brand.as_code_str())
            .unwrap_or(0)
    }

    fn save_store(&self) {
        if let Err(e) = self.store.save(&self.store_path) {
            tracing::warn!("Nie udało się zapisać ulubionych: {e}");
        }
    }

    fn push_favourites(&self) {
        let rows: Vec<FavouriteRow> = self
            .store
            .items
            .iter()
            .map(|f| {
                // Ramka mówi wyłącznie o tym, co jest do wzięcia u producenta.
                // O wersji siedzącej w aucie celowo tu nie zgadujemy — na to
                // jest widok modelu, gdzie da się dopisać, skąd ją znamy.
                //
                // Data idzie przed wersją: przy wąskiej kolumnie skróci się
                // wersja, a nie ona.
                let version = match (&f.latest_released, &f.latest_seen_version) {
                    (Some(released), Some(v)) => tr!("{} - {}", released, v),
                    (None, Some(v)) => v.clone(),
                    (_, None) => "—".to_owned(),
                };

                FavouriteRow {
                    model_id: f.model_id.as_str().into(),
                    label: f.label.as_str().into(),
                    version: version.into(),
                }
            })
            .collect();

        let empty = self.store.items.is_empty();

        self.with_ui(move |ui| {
            ui.set_favourites(ModelRc::new(VecModel::from(rows)));
            ui.set_favourites_note(if empty {
                tr!("Pick a model and press “Add to favourites” to see its version here every time you start.")
                    .into()
            } else {
                "".into()
            });
        });
    }

    fn push_selected_favourite_state(&mut self) {
        let favourite = self
            .selected
            .map(|i| self.entries[i].car.id.clone())
            .and_then(|id| self.store.find(&id));

        let is_favourite = favourite.is_some();
        let watch = favourite.is_some_and(|f| f.watch);

        let interval = mapstick_store::Interval::ALL
            .iter()
            .position(|i| *i == self.store.check_interval)
            .unwrap_or(1) as i32;

        let (labels, choices, chosen) = self.installed_options();
        self.installed_choices = choices;

        let note = self.watch_note();
        let variants = self.variants_note();

        self.with_ui(move |ui| {
            ui.set_variants_note(variants.into());
            ui.set_selected_is_favourite(is_favourite);
            ui.set_selected_watch(watch);
            ui.set_interval_index(interval);
            ui.set_installed_choices(ModelRc::new(VecModel::from(labels)));
            ui.set_installed_index(chosen as i32);
            ui.set_watch_note(note.into());
        });
    }

    /// Wersje, które mogą siedzieć w aucie: co wystawia producent, co sami
    /// pobraliśmy, co przeczytaliśmy z nośnika. Pierwsza pozycja to „nie wiem”,
    /// bo niewiedza jest stanem uczciwym i trzeba dać do niej wrócić.
    fn installed_options(&self) -> (Vec<SharedString>, Vec<Option<String>>, usize) {
        let mut labels = vec![SharedString::from(tr!("don’t know"))];
        let mut choices: Vec<Option<String>> = vec![None];

        let Some(index) = self.selected else {
            return (labels, choices, 0);
        };

        let entry = &self.entries[index];
        let favourite = self.store.find(&entry.car.id);

        // Dwie wersje jednego modelu to zwykle dwie głowice (`ccNC`
        // i `ccNC_Lite`), a nie starsza i nowsza. Sam numer nie mówi, która
        // jest w aucie — mówi to, gdzie ją widzieliśmy.
        let seen_on_medium = favourite
            .filter(|f| f.installed_source == Some(InstalledSource::VerFile))
            .and_then(|f| f.installed_version.clone());
        let downloaded = favourite.and_then(|f| f.downloaded_version.clone());

        let mut add = |version: &str, released: Option<&str>| {
            if version.is_empty() || choices.iter().any(|c| c.as_deref() == Some(version)) {
                return;
            }

            let hint = if seen_on_medium.as_deref() == Some(version) {
                i18n::tr("  ·  written in the .ver file")
            } else if downloaded.as_deref() == Some(version) {
                i18n::tr("  ·  we downloaded this")
            } else {
                ""
            };

            labels.push(match released {
                Some(date) if !date.is_empty() => format!("{date} — {version}{hint}").into(),
                _ => format!("{version}{hint}").into(),
            });
            choices.push(Some(version.to_owned()));
        };

        for version in &entry.car.versions {
            add(version, Some(&entry.released));
        }

        if let Some(f) = favourite {
            if let Some(v) = &f.downloaded_version {
                add(v, None);
            }
            if let Some(v) = &f.installed_version {
                add(v, None);
            }
        }

        // Podpowiedź: ostatnie pobranie, bo to ono zwykle trafia do auta.
        // Potwierdzony wybór użytkownika bije podpowiedź.
        let wanted = favourite.and_then(|f| {
            if f.installed_confirmed {
                f.installed_version.clone()
            } else {
                f.installed_version.clone().or(f.downloaded_version.clone())
            }
        });

        let chosen = wanted
            .and_then(|v| choices.iter().position(|c| c.as_deref() == Some(v.as_str())))
            .unwrap_or(0);

        (labels, choices, chosen)
    }

    /// Zdanie o modelu wystawionym w kilku numerach naraz.
    ///
    /// Wygląda to jak wybór między starszą a nowszą wersją, a jest wyborem
    /// głowicy — przy czym pliki i tak są te same. Bez tego zdania numer
    /// wybiera się rzutem monetą.
    fn variants_note(&self) -> String {
        let Some(index) = self.selected else {
            return String::new();
        };

        let versions = &self.entries[index].car.versions;
        if versions.len() < 2 {
            return String::new();
        }

        // Plik `.ver` nie rozstrzyga: producent zapisuje w nim zawsze pierwszy
        // numer z listy, niezależnie od tego, którą głowicę ma auto.
        tr!(
            "This model is published for several head-unit variants (e.g. ccNC and \
             ccNC Lite). The files to download are identical for all of them — pick the \
             number you see on the car’s screen."
        )
    }

    /// Zdanie o tym, kto i jak często nas budzi.
    fn watch_note(&self) -> String {
        let watched = self.store.watched().count();

        if watched == 0 {
            return String::new();
        }

        let count_phrase = match watched {
            1 => tr!("1 model"),
            n => tr!("{} models", n),
        };

        match schedule::current() {
            Some(kind) => tr!(
                "Asking the maker {} (watched: {}); a new release is brought up daily — \
                 woken by {}.",
                i18n::tr(self.store.check_interval.label()),
                count_phrase,
                i18n::tr(kind.label())
            ),
            None => tr!("No way to check in the background — the schedule was not set up."),
        }
    }

    /// Włącza albo wyłącza pilnowanie modelu, zakładając harmonogram przy
    /// pierwszym włączeniu i sprzątając go po ostatnim wyłączeniu.
    fn set_watch(&mut self, on: bool) {
        let Some(index) = self.selected else {
            return;
        };
        let id = self.entries[index].car.id.clone();

        let Some(favourite) = self.store.get_mut(&id) else {
            return;
        };

        favourite.watch = on;
        let label = favourite.label.clone();
        self.save_store();

        self.sync_schedule(if on {
            tr!("Watching {} — I will let you know when a new release appears.", label)
        } else {
            tr!("No longer watching {}.", label)
        });
    }

    fn set_interval(&mut self, index: usize) {
        let Some(interval) = mapstick_store::Interval::ALL.get(index).copied() else {
            return;
        };

        self.store.check_interval = interval;
        self.save_store();

        self.sync_schedule(tr!("Asking the maker {}.", i18n::tr(interval.label())));
    }

    /// Dostraja harmonogram do tego, czego pilnujemy, i melduje wynik.
    fn sync_schedule(&mut self, message: String) {
        let watched = self.store.watched().count();

        let outcome = if watched == 0 {
            schedule::remove().map(|()| None)
        } else {
            schedule::install(self.store.check_interval).map(Some)
        };

        match outcome {
            Ok(Some(kind)) => self.set_status(tr!("{} Woken by {}.", message, i18n::tr(kind.label())), false),
            Ok(None) => self.set_status(message, false),
            Err(e) => self.set_status(
                tr!("{} The schedule could not be set up, though: {}", message, format!("{e:#}")),
                true,
            ),
        }

        self.push_selected_favourite_state();
    }

    /// Zapisuje wersję, którą użytkownik wskazał jako obecną w aucie.
    fn set_installed(&mut self, index: usize) {
        let Some(choice) = self.installed_choices.get(index).cloned() else {
            return;
        };
        let Some(model_index) = self.selected else {
            return;
        };
        let id = self.entries[model_index].car.id.clone();

        let Some(favourite) = self.store.get_mut(&id) else {
            return;
        };

        let label = favourite.label.clone();

        match choice {
            Some(version) => {
                favourite.installed_version = Some(version.clone());
                favourite.installed_source = Some(InstalledSource::Manual);
                favourite.installed_confirmed = true;

                // Powiadomienie o tej wersji przestaje mieć sens, skoro auto
                // ją ma; o następnej powiemy od nowa.
                favourite.notified_version = Some(version.clone());

                self.set_status(tr!("{}: the car has {}.", label, version), false);
            }
            None => {
                favourite.installed_version = None;
                favourite.installed_source = None;
                favourite.installed_confirmed = false;
                // „Nie wiem” to prośba o przypomnienie — od najbliższego razu.
                favourite.notified_version = None;
                favourite.notified_at = None;

                self.set_status(tr!("{}: unknown what the car has.", label), false);
            }
        }

        self.save_store();
        self.push_favourites();
        self.push_selected_favourite_state();
        self.push_target_state();
    }

    fn toggle_favourite(&mut self) {
        let Some(index) = self.selected else {
            return;
        };
        let car = &self.entries[index].car;

        if self.store.remove(&car.id) {
            self.set_status(tr!("{} removed from favourites.", car.name), false);
        } else {
            self.store.upsert(Favourite {
                label: car.name.clone(),
                brand: BRAND_NAMES[self.brand_position()].to_owned(),
                region: self.region.clone(),
                model_id: car.id.clone(),
                model_name: car.model.clone(),
                latest_seen_version: car.versions.first().cloned(),
                latest_checked_at: Some(mapstick_store::now()),
                latest_released: Some(self.entries[index].released.clone())
                    .filter(|r| !r.is_empty()),
                downloaded_version: None,
                downloaded_at: None,
                installed_version: None,
                installed_source: None,
                installed_confirmed: false,
                vin: None,
                // Pilnowanie w tle włącza się osobnym polem — gwiazdka mówi
                // „interesuje mnie ten model”, a nie „sprawdzaj go beze mnie”.
                watch: false,
                notified_version: None,
                notified_at: None,
            });
            self.set_status(tr!("{} added to favourites.", car.name), false);

            // Świeży wpis nie wie nic o wersji w aucie. Jeśli akurat jest
            // podłączony nośnik z tym firmware'em, uzupełniamy ją od razu,
            // zamiast czekać na wejście w widok nośnika.
            self.learn_installed_from_media();
        }

        self.save_store();
        self.push_favourites();
        self.push_selected_favourite_state();
    }

    /// Przepisuje najnowsze wersje i daty wydania do pasujących ulubionych.
    fn update_favourites_from(&mut self, entries: &[Entry]) -> bool {
        let mut changed = false;

        for entry in entries {
            let Some(favourite) = self.store.get_mut(&entry.car.id) else {
                continue;
            };

            favourite.latest_seen_version = favourite.matching(&entry.car.versions).cloned();
            favourite.latest_released = Some(entry.released.clone()).filter(|r| !r.is_empty());
            favourite.latest_checked_at = Some(mapstick_store::now());
            changed = true;
        }

        if changed {
            self.save_store();
            self.push_favourites();
        }

        changed
    }

    /// Zleca sprawdzenie wersji ulubionych osobnemu zadaniu.
    ///
    /// Odpytujemy po jednym zapytaniu na kombinację marki i regionu, bo lista
    /// modeli i tak przychodzi w całości. Robi to zadanie w tle, żeby start
    /// aplikacji nie czekał na sieć — ostatnio wybrany model ma się pokazać
    /// od razu, a wersje ulubionych dopisać się same, gdy przyjdą.
    fn spawn_favourite_refresh(&self) {
        let mut combos: Vec<(String, String)> = self
            .store
            .items
            .iter()
            .map(|f| (f.brand.clone(), f.region.clone()))
            .collect();
        combos.sort();
        combos.dedup();

        if combos.is_empty() {
            return;
        }

        let wanted: HashSet<String> = self
            .store
            .items
            .iter()
            .map(|f| f.model_id.clone())
            .collect();
        let tx = self.tx.clone();

        tokio::spawn(async move {
            let mut found = Vec::new();

            for (brand, region) in combos {
                let code = BRANDS[brand_index(&brand)].as_code_str();

                match fetch_entries(code, &region).await {
                    Ok(entries) => found.extend(
                        entries
                            .into_iter()
                            .filter(|e| wanted.contains(&e.car.id))
                            .map(|e| FavouriteVersion {
                                model_id: e.car.id,
                                versions: e.car.versions,
                                released: Some(e.released).filter(|r| !r.is_empty()),
                            }),
                    ),
                    Err(e) => tracing::warn!("Nie udało się sprawdzić {brand}/{region}: {e}"),
                }
            }

            let _ = tx.send(Cmd::FavouritesRefreshed(found));
        });
    }

    /// Przyjmuje wersje przyniesione z tła.
    ///
    /// Wpisy, których w międzyczasie nie ma już w ulubionych, po prostu
    /// pomijamy — użytkownik mógł je usunąć, zanim odpowiedź wróciła.
    fn apply_favourite_versions(&mut self, versions: Vec<FavouriteVersion>) {
        let mut changed = false;

        for item in versions {
            let Some(favourite) = self.store.get_mut(&item.model_id) else {
                continue;
            };

            favourite.latest_seen_version = favourite.matching(&item.versions).cloned();
            favourite.latest_released = item.released;
            favourite.latest_checked_at = Some(mapstick_store::now());
            changed = true;
        }

        if changed {
            self.save_store();
            self.push_favourites();
        }
    }

    /// Zapamiętuje, co ma się odtworzyć przy następnym uruchomieniu.
    ///
    /// Model podajemy tylko wtedy, gdy ktoś w niego kliknął. Przy samej zmianie
    /// marki albo regionu zostaje ten zapamiętany wcześniej — ale wyłącznie
    /// jeśli wciąż jest na świeżo pobranej liście, bo inaczej wskazywałby auto
    /// innej marki, niż mówią oba pola.
    fn remember_selection(&mut self, model_id: Option<String>) {
        let model_id = model_id.or_else(|| {
            let previous = self.store.last_selection.as_ref()?.model_id.as_ref()?;

            self.entries
                .iter()
                .any(|e| &e.car.id == previous)
                .then(|| previous.clone())
        });

        self.store.last_selection = Some(LastSelection {
            brand: BRAND_NAMES[self.brand_position()].to_owned(),
            region: self.region.clone(),
            model_id,
        });
        self.save_store();
    }

    async fn load_models(&mut self, brand: usize, region: usize) -> Result<()> {
        self.brand = *BRANDS
            .get(brand)
            .ok_or_else(|| anyhow!("Nieznana marka o indeksie {brand}"))?;
        self.region = REGIONS
            .get(region)
            .ok_or_else(|| anyhow!("Nieznany region o indeksie {region}"))?
            .0
            .to_owned();

        let code = self.brand.as_code_str();
        let entries = fetch_entries(code, &self.region).await?;

        // Lista i tak przyszła — przy okazji odświeżamy ulubione z tej marki
        // i regionu, bez dodatkowego zapytania.
        self.update_favourites_from(&entries);

        // …i sprzątamy archiwa wydań, których producent już nie wystawia.
        self.sweep_cache(
            entries
                .iter()
                .map(|e| (e.car.id.clone(), e.car.versions.clone()))
                .collect(),
        );

        let count = entries.len();
        self.entries = entries;
        self.filter.clear();
        self.push_rows();

        self.set_status(tr!("Models found: {}.", count), false);

        // Sama zmiana marki albo regionu też jest wyborem. Bez tego zamknięcie
        // okna przed kliknięciem w model wracałoby przy starcie do auta
        // oglądanego poprzednim razem.
        self.remember_selection(None);

        // Odtworzenie ostatniego wyboru, skok z ulubionych albo z odnośnika.
        // Odnośnik niesie `mcode`, bo tym kluczem opisuje pojazdy katalog
        // strony; ulubione i ostatni wybór — identyfikator modelu.
        let wanted = match (self.pending_select.take(), self.pending_mcode.take()) {
            (Some(id), _) => self
                .filtered
                .iter()
                .position(|&i| self.entries[i].car.id == id),
            (None, Some(mcode)) => self
                .filtered
                .iter()
                .position(|&i| self.entries[i].car.mcode == mcode),
            (None, None) => None,
        };

        if let Some(visible) = wanted {
            self.select(visible).await?;
        }

        Ok(())
    }

    async fn select(&mut self, visible_index: usize) -> Result<()> {
        let Some(&index) = self.filtered.get(visible_index) else {
            return Ok(());
        };

        self.selected = Some(index);
        self.firmware = None;

        // „Wznów” dotyczyło poprzedniego modelu — po zmianie wskazywałoby
        // pobranie czegoś zupełnie innego.
        self.forget_pause();

        let entry = &self.entries[index];
        let (name, id, version, media, released) = (
            entry.car.name.clone(),
            entry.car.id.clone(),
            entry.car.versions.join(", "),
            entry.media.clone(),
            entry.released.clone(),
        );
        let visible = visible_index as i32;

        let id_for_store = entry.car.id.clone();
        self.remember_selection(Some(id_for_store));
        self.push_selected_favourite_state();

        self.with_ui(move |ui| {
            ui.set_selected_index(visible);
            ui.set_detail_name(name.into());
            ui.set_detail_id(id.into());
            ui.set_detail_version(version.into());
            ui.set_detail_media(media.into());
            ui.set_detail_released(released.into());
            ui.set_detail_size(tr!("checking the size…").into());
            ui.set_detail_ready(true);
        });

        // Rozmiar znany jest dopiero po odpytaniu /car/download, więc dociągamy
        // go osobno — dzięki temu sprawdzenie wolnego miejsca będzie dokładne.
        let client = NuClientBuilder::new().build()?;
        let firmware = client
            .get_firmware_info(&self.region, &self.entries[index].car)
            .await?;

        let (download, extracted) = firmware_totals(&firmware);
        tracing::info!(
            "{}: do pobrania {download} B, na nośniku {extracted} B, plików {}",
            self.entries[index].car.id,
            firmware.files.len()
        );

        // Dane map są już spakowane, więc rozmiar pobrania i rozmiar na
        // nośniku zwykle różnią się o promile (Bayon_EU: 25,47 vs 25,50 GiB).
        // Pokazujemy jedną liczbę, a rozbijamy ją dopiero wtedy, gdy różnica
        // jest na tyle duża, że mogłaby zmylić przy sprawdzaniu miejsca.
        let differ = extracted > 0 && download.abs_diff(extracted) * 50 > extracted;

        let size = match (extracted, differ) {
            (0, _) => tr!(
                "{} to download · files: {}",
                format_size(download),
                firmware.files.len()
            ),
            (_, false) => tr!(
                "{} · files: {}",
                format_size(extracted),
                firmware.files.len()
            ),
            (_, true) => tr!(
                "{} to download, {} on the medium · files: {}",
                format_size(download),
                format_size(extracted),
                firmware.files.len()
            ),
        };
        self.firmware = Some(firmware);

        self.with_ui(move |ui| ui.set_detail_size(size.into()));
        Ok(())
    }

    /// Ile miejsca trzeba na nośniku dla wybranego modelu.
    ///
    /// Przy pobieraniu wprost na nośnik muszą się na nim zmieścić naraz
    /// archiwa i to, co z nich powstaje — czyli mniej więcej dwa razy tyle.
    fn required_bytes(&self) -> u64 {
        self.firmware
            .as_ref()
            .map(|f| {
                let (download, extracted) = firmware_totals(f);

                if self.direct_to_medium {
                    download + extracted
                } else {
                    extracted
                }
            })
            .unwrap_or(0)
    }

    /// Czy we wskazanym celu leży już dokładnie ta wersja, którą chcemy wgrać.
    ///
    /// Sprawdzamy sam katalog docelowy, a nie to, który nośnik został kliknięty
    /// — inaczej wybranie katalogu ręcznie omijałoby kontrolę.
    fn push_target_state(&mut self) {
        let current = match (&self.output_dir, self.selected) {
            (Some(dir), Some(index)) => {
                let car = &self.entries[index].car;

                mapstick_media::read_installed(dir)
                    .ok()
                    .flatten()
                    .is_some_and(|i| {
                        i.model_id == car.id
                            && car.versions.contains(&i.firmware_version)
                            && i.complete
                    })
            }
            _ => false,
        };

        tracing::info!("cel zawiera bieżącą wersję: {current}");

        // Katalog na dysku z gotową bieżącą wersją zapamiętujemy, choćby nie
        // wziął się z naszego pobierania — użytkownik mógł go wskazać sam.
        // Bez tego aplikacja nie miałaby skąd wiedzieć, że jest co przenieść
        // na podłączony za chwilę nośnik.
        if current && !self.target_is_medium() {
            self.note_ready_directory();
        }

        self.with_ui(move |ui| ui.set_target_has_current(current));
        self.push_copy_state();
    }

    /// Jedyny podłączony nośnik przejmuje rolę celu, dopóki celu nie ma.
    ///
    /// Wskazanego ręcznie katalogu nie podmieniamy nigdy — nawet gdy leży
    /// w nim gotowe oprogramowanie. Kiedyś tak było, żeby od razu pokazać
    /// „Skopiuj na nośnik”, ale odbierało to użytkownikowi wybór, którego
    /// właśnie dokonał. Miejsce kopiowania wskazuje teraz
    /// [`Worker::copy_destination`] i cel pobierania nie musi go udawać.
    fn adopt_single_medium(&mut self) {
        if self.media.len() != 1 || self.media[0].mount_point.is_none() {
            return;
        }

        if self.output_dir.is_none() {
            self.select_medium(0);
        }
    }

    /// Zapisuje wskazany katalog jako gotowe oprogramowanie dla tego modelu.
    ///
    /// Nie ruszamy przy tym „poprzednio pobranej wersji” w ulubionych: tych
    /// plików nie pobraliśmy i nie mamy prawa tego twierdzić.
    fn note_ready_directory(&mut self) {
        let (Some(index), Some(path)) = (self.selected, self.output_dir.clone()) else {
            return;
        };

        let car = &self.entries[index].car;
        let version = car.versions.first().cloned().unwrap_or_default();

        if self
            .store
            .download_for(&car.id)
            .is_some_and(|d| d.path == path && d.version == version)
        {
            return;
        }

        tracing::info!("{}: gotowe pliki znalezione w {}", car.id, path.display());

        self.store.record_download(Download {
            model_id: car.id.clone(),
            label: car.name.clone(),
            brand: BRAND_NAMES[self.brand_position()].to_owned(),
            region: self.region.clone(),
            version,
            path,
            at: mapstick_store::now(),
        });
        self.save_store();
    }

    /// Katalog na dysku, w którym leży już gotowe oprogramowanie dla wybranego
    /// modelu — czyli to, co można przenieść na nośnik zamiast pobierać.
    ///
    /// Sam wpis w pliku nie wystarcza: katalog mógł zniknąć albo zostać
    /// podmieniony, więc wersję potwierdzamy odczytem `.ver` na miejscu.
    /// Nośnik, na który trafiłyby gotowe dane z dysku twardego.
    ///
    /// To nie musi być cel pobierania. Katalog wskazany ręcznie bywa właśnie
    /// źródłem — i wtedy pytanie „dokąd skopiować” ma oczywistą odpowiedź,
    /// o ile podłączony nośnik jest jeden. Przy kilku nie zgadujemy: niech
    /// użytkownik kliknie ten, o który mu chodzi.
    fn copy_destination(&self) -> Option<PathBuf> {
        if self.target_is_medium() {
            return self.output_dir.clone();
        }

        // Nośnik wskazany wcześniej kliknięciem, o ile wciąż jest podłączony.
        if let Some(chosen) = &self.chosen_medium
            && self
                .media
                .iter()
                .any(|m| m.mount_point.as_deref() == Some(chosen.as_path()))
        {
            return Some(chosen.clone());
        }

        match self.media.as_slice() {
            [only] => only.mount_point.clone(),
            _ => None,
        }
    }

    fn ready_copy(&self) -> Option<PathBuf> {
        let index = self.selected?;
        let car = &self.entries[index].car;
        let record = self.store.download_for(&car.id)?;

        // Interesuje nas tylko to, co zgadza się z bieżącym wydaniem.
        if !car.versions.contains(&record.version) {
            return None;
        }

        // Kopiowanie katalogu w to samo miejsce nie ma sensu.
        if self.copy_destination().as_deref() == Some(record.path.as_path()) {
            return None;
        }

        let installed = mapstick_media::read_installed(&record.path).ok().flatten()?;

        // Przerwane pobranie ma manifest, ale nie ma plików — nie ma czego
        // z niego kopiować.
        (installed.model_id == car.id
            && installed.firmware_version == record.version
            && installed.complete)
            .then(|| record.path.clone())
    }

    fn push_copy_state(&self) {
        let shown = self
            .ready_copy()
            .map(|p| p.display().to_string())
            .unwrap_or_default();

        // Bez nośnika nie ma dokąd kopiować, choćby dane na dysku były gotowe.
        let onto = self
            .copy_destination()
            .filter(|_| !shown.is_empty())
            .map(|p| p.display().to_string())
            .unwrap_or_default();

        tracing::info!(
            "gotowe dane na dysku twardym: {} → {}",
            if shown.is_empty() { "brak" } else { &shown },
            if onto.is_empty() { "nie ma dokąd" } else { &onto }
        );

        self.with_ui(move |ui| {
            ui.set_copy_source(shown.into());
            ui.set_copy_target(onto.into());
        });
    }

    /// Zapisuje, że dane leżą już na dysku.
    ///
    /// Ten sam wpis odpowiada za dwie rzeczy: propozycję skopiowania na nośnik
    /// i pole „poprzednio pobrana wersja” w ulubionych.
    fn record_download(&mut self, record: Download) {
        if let Some(favourite) = self.store.get_mut(&record.model_id) {
            favourite.downloaded_version = Some(record.version.clone());
            favourite.downloaded_at = Some(record.at.clone());
        }

        tracing::info!(
            "{}: {} zapisane w {}",
            record.model_id,
            record.version,
            record.path.display()
        );

        self.store.record_download(record);
        self.save_store();
        self.push_favourites();
        self.push_target_state();
    }

    /// Pliki poprzedniego wydania leżące we wskazanym miejscu.
    ///
    /// Puste, dopóki nie wiemy, co zapisze nowe wydanie — bez tej listy nie
    /// da się odróżnić sieroty od pliku, który i tak zostanie nadpisany.
    fn stale_on_target(&self, target: &Path) -> stale::Stale {
        let (Some(index), Some(firmware)) = (self.selected, self.firmware.as_ref()) else {
            return stale::Stale::default();
        };

        let keep: HashSet<String> = firmware
            .files
            .iter()
            .map(|f| match &f.directory {
                Some(d) => format!("{d}/{}", f.name),
                None => f.name.clone(),
            })
            .collect();

        stale::find(target, &self.entries[index].car.id, &keep)
    }

    /// Usuwa z nośnika pliki poprzedniego wydania, o które nikt się już nie
    /// upomni. Ręczne wyjście na wypadek, gdy brakuje miejsca na nowe.
    fn remove_stale(&mut self, index: usize) {
        let Some(mount) = self.media.get(index).and_then(|m| m.mount_point.clone()) else {
            return;
        };

        let stale = self.stale_on_target(&mount);

        if stale.is_empty() {
            self.set_status(tr!("There are no files of a previous release there."), false);
            return;
        }

        let bytes = stale.bytes;
        let count = stale::remove(&mount, &stale);

        self.set_status(
            tr!(
                "Removed {} files of the previous release — {} recovered.",
                count,
                format_size(bytes)
            ),
            false,
        );
    }

    /// Wszystko, co opisuje manifest w katalogu docelowym.
    ///
    /// W odróżnieniu od [`Worker::stale_on_target`] nic nie oszczędzamy:
    /// pobieranie uznaje każdy zastany plik wynikowy za gotowy (sprawdza samo
    /// istnienie, nie sumę kontrolną), więc ponowne pobranie do pełnego
    /// katalogu nie pobrałoby ani bajtu.
    fn everything_on_target(&self) -> Option<(PathBuf, String, stale::Stale)> {
        let target = self.output_dir.clone()?;
        let model_id = self.entries.get(self.selected?)?.car.id.clone();
        let found = stale::find(&target, &model_id, &HashSet::new());

        Some((target, model_id, found))
    }

    /// Pyta, zanim skasuje gotowe pliki, żeby pobrać je jeszcze raz.
    fn request_redownload(&mut self) {
        let Some((target, _, doomed)) = self.everything_on_target() else {
            return;
        };

        let summary = tr!(
            "{}\nTo be removed: {} files, {}",
            target.display(),
            doomed.paths.len(),
            format_size(doomed.bytes)
        );

        self.with_ui(move |ui| {
            ui.set_redownload_target(summary.into());
            ui.set_redownload_dialog(true);
        });
    }

    /// Usuwa poprzednie pobranie i zaczyna je od nowa.
    ///
    /// Archiwów w pamięci podręcznej nie ruszamy: po udanym pobraniu i tak ich
    /// nie ma, a gdy zostały, to po pobraniu przerwanym — i wtedy są dokładnie
    /// tym, czego potrzeba, żeby nie ciągnąć wszystkiego drugi raz.
    fn confirm_redownload(&mut self) {
        self.with_ui(|ui| ui.set_redownload_dialog(false));

        let Some((target, model_id, doomed)) = self.everything_on_target() else {
            return;
        };

        let bytes = doomed.bytes;
        let removed = stale::remove(&target, &doomed);

        // Manifest nie należy do plików, które wymienia, a to on decyduje
        // o zdaniu „leży już bieżąca wersja”. Zostawiony kazałby aplikacji
        // twierdzić, że wszystko jest na miejscu, gdy nie ma już niczego.
        let manifest = target.join(format!("{model_id}.ver"));
        if let Err(e) = std::fs::remove_file(&manifest)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("Nie udało się usunąć {}: {e}", manifest.display());
        }

        // Wpis o gotowym pobraniu opisywał to, co przed chwilą skasowaliśmy.
        if self
            .store
            .download_for(&model_id)
            .is_some_and(|d| d.path == target)
        {
            self.store.forget_download(&model_id);
            self.save_store();
        }

        tracing::info!(
            "Ponowne pobranie {model_id}: usunięto {removed} plików ({bytes} B) z {}",
            target.display()
        );

        self.push_target_state();
        self.push_space_note();

        if let Err(e) = self.start_download() {
            self.set_status(e.to_string(), true);
        }
    }

    /// Kasuje archiwa, które nie mogą się już przydać.
    ///
    /// `known` wypełniamy wyłącznie pełnymi spisami wersji z katalogu
    /// producenta. Przy niepełnym można by skasować archiwum wariantu, który
    /// dzieli identyfikator modelu z innym — a te bywają dwa (HEV i PHEV).
    fn sweep_cache(&self, known: HashMap<String, Vec<String>>) {
        let swept = cache::sweep(&known, cache::MAX_AGE);

        if swept.directories > 0 {
            tracing::info!(
                "Sprzątnięto {} katalogów archiwów, odzyskane {} B",
                swept.directories,
                swept.bytes
            );
            self.push_leftovers();
        }
    }

    /// Ile miejsca zajmują archiwa pozostałe po wcześniejszych pobraniach.
    fn push_leftovers(&self) {
        let size = cache::directory_size(&cache::base());
        let text = if size > 0 {
            format_size(size)
        } else {
            String::new()
        };

        tracing::info!("pozostałości archiwów: {} B", size);
        self.with_ui(move |ui| ui.set_leftovers(text.into()));
    }

    fn clear_cache(&self) {
        let work = cache::base();

        match std::fs::remove_dir_all(&work) {
            Ok(()) => self.set_status(tr!("Leftover archives removed."), false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.set_status(tr!("Nothing to remove."), false)
            }
            Err(e) => self.set_status(tr!("Could not clean up: {}", e), true),
        }

        self.push_leftovers();
    }

    /// Opis zapotrzebowania na miejsce, pokazywany przy przełączniku trybu.
    fn push_space_note(&self) {
        let Some(firmware) = &self.firmware else {
            self.with_ui(|ui| {
                ui.set_space_note("".into());
                ui.set_space_warning(false);
            });
            return;
        };

        let (download, extracted) = firmware_totals(firmware);

        let (note, warning) = if self.direct_to_medium {
            (
                tr!(
                    "Downloaded files need unpacking, so the removable medium has to hold \
                     the archives and the unpacked result at once: about {}. Writes to the \
                     flash memory double as a result. Use this only if you have a large \
                     medium and not enough free space on the hard disk.",
                    format_size(download + extracted)
                ),
                true,
            )
        } else {
            let work = cache::base();
            let free = mapstick_media::space_for(&work)
                .or_else(|_| {
                    work.parent()
                        .map(mapstick_media::space_for)
                        .unwrap_or(Err(mapstick_media::Error::Unsupported))
                })
                .map(|s| s.free)
                .unwrap_or(0);

            let enough = free == 0 || free >= mapstick_media::with_margin(download);

            (
                (if self.target_is_medium() {
                        tr!(
                            "The downloaded archives ({}) go to the hard disk for the time \
                             being; only the unpacked result ({}) lands on the removable medium.",
                            format_size(download),
                            format_size(extracted)
                        )
                    } else {
                        tr!(
                            "The downloaded archives will be unpacked into the chosen folder \
                             ({} needed). Once the download finishes, plug in the removable \
                             device and click “Refresh” to copy or move the data onto it.",
                            format_size(extracted)
                        )
                    }),
                !enough,
            )
        };

        self.with_ui(move |ui| {
            ui.set_space_note(note.into());
            ui.set_space_warning(warning);
        });
    }

    /// Czy cel to jeden z wykrytych nośników wymiennych, a nie zwykły katalog.
    fn target_is_medium(&self) -> bool {
        let Some(target) = &self.output_dir else {
            return false;
        };

        self.media
            .iter()
            .any(|m| m.mount_point.as_deref() == Some(target.as_path()))
    }

    fn push_media_rows(&self) {
        let needed = self.required_bytes();

        // Wersja, którą użytkownik zamierza wgrać — do porównania z tym, co
        // już leży na nośniku.
        let target = self
            .selected
            .map(|i| (&self.entries[i].car.id, &self.entries[i].car.versions));

        let rows: Vec<MediumRow> = self
            .media
            .iter()
            .map(|m| {
                // Plik .ver mówi, co użytkownik ostatnio wgrywał tym nośnikiem
                // — czyli najpewniejsze źródło wersji siedzącej w aucie.
                let found = m
                    .mount_point
                    .as_deref()
                    .and_then(|p| mapstick_media::read_installed(p).ok().flatten());

                let same_version = match (&found, target) {
                    (Some(f), Some((id, versions))) => {
                        f.model_id == *id && versions.contains(&f.firmware_version)
                    }
                    _ => false,
                };
                let other_version = found.is_some() && !same_version && target.is_some();

                let (status, usable) = match () {
                    _ if m.mount_point.is_none() => (tr!("not mounted"), false),
                    _ if !m.is_exfat() => (tr!("needs exFAT"), false),
                    // Ta sama wersja: nudl pominie wszystkie pliki, więc
                    // pobieranie skończy się natychmiast i niczego nie zmieni.
                    _ if same_version => (tr!("the medium already holds the current version"), true),
                    _ if needed > 0 && !m.fits(needed) => (tr!("not enough space"), false),
                    // Prawie wszystkie pliki mają stałe ścieżki, więc nowe
                    // wydanie nadpisze je w miejscu; osierocone zostaje głównie
                    // archiwum map, bo jego nazwa niesie numer bazy. Auto go nie
                    // zainstaluje — czyta manifest — więc to kwestia miejsca,
                    // nie poprawności.
                    _ if other_version => (
                        tr!(
                            "The removable medium holds a different firmware version; it \
                             will be removed once the current one is downloaded"
                        ),
                        true,
                    ),
                    _ => (tr!("ready"), true),
                };

                let (installed_model, installed_version) = found
                    .map(|i| (i.model_id, i.firmware_version))
                    .unwrap_or_default();

                let stale = m
                    .mount_point
                    .as_deref()
                    .map(|p| self.stale_on_target(p))
                    .filter(|s| !s.is_empty())
                    .map(|s| format_size(s.bytes))
                    .unwrap_or_default();

                let details = tr!(
                    "{} · free {} of {}{}",
                    m.filesystem.as_deref().unwrap_or(i18n::tr("no file system")),
                    format_size(m.free_bytes),
                    format_size(m.total_bytes),
                    m.connection
                        .as_deref()
                        .map(|c| format!(" · {c}"))
                        .unwrap_or_default(),
                );

                tracing::info!("{}: {status}", m.id);

                MediumRow {
                    id: m.id.as_str().into(),
                    label: medium_label(m).into(),
                    details: details.into(),
                    status: status.as_str().into(),
                    usable,
                    installed_model: installed_model.into(),
                    installed_version: installed_version.into(),
                    stale: stale.into(),
                }
            })
            .collect();

        self.with_ui(move |ui| {
            ui.set_media(ModelRc::new(VecModel::from(rows)));
            ui.set_selected_medium(-1);
        });
    }

    async fn refresh_media(&mut self) {
        self.reread_media(true).await;
    }

    /// Odczytuje nośniki na nowo, nie odzywając się na pasku stanu.
    ///
    /// Po pobraniu albo skopiowaniu wolnego miejsca jest o dziesiątki gigabajtów
    /// mniej, a wiersz nośnika dalej pokazywałby odczyt sprzed zapisu. Meldunek
    /// o liczbie nośników zabrałby przy tym miejsce wiadomości o tym, co się
    /// właśnie skończyło.
    async fn refresh_space(&mut self) {
        self.reread_media(false).await;
    }

    async fn reread_media(&mut self, announce: bool) {
        self.with_ui(|ui| ui.set_media_busy(true));

        match mapstick_media::list_media().await {
            Ok(media) => {
                let count = media.len();
                self.media = media;
                self.learn_installed_from_media();
                self.push_media_rows();

                if count == 0 {
                    if announce {
                        self.set_status(
                            tr!("No removable medium found. Internal and system disks are skipped."),
                            false,
                        );
                    }
                } else {
                    if announce {
                        self.set_status(tr!("Media found: {}.", count), false);
                    }

                    // Jeden podłączony nośnik to jedyny sensowny cel. Bez tego
                    // „Pobierz” zostaje wyszarzony, a jedyną podpowiedzią jest
                    // kliknięcie w wiersz, który wcale nie wygląda na klikalny.
                    self.adopt_single_medium();
                }
            }
            Err(e) => {
                self.media.clear();
                self.push_media_rows();
                self.set_status(tr!("Could not detect media: {}", describe(&e)), true);
            }
        }

        // Zawartość celu mogła się zmienić między odświeżeniami — choćby
        // dlatego, że ktoś wgrał pliki na nośnik ręcznie. Bez ponownego
        // sprawdzenia aplikacja trwałaby przy tym, co widziała ostatnio.
        self.push_target_state();

        self.with_ui(|ui| ui.set_media_busy(false));
    }

    /// Zapisuje w ulubionych to, co znaleziono na podłączonych nośnikach.
    ///
    /// Plik `.ver` na pendrivie to najpewniejsze źródło wiedzy o wersji
    /// siedzącej w aucie — pod warunkiem, że użytkownik aktualizował właśnie
    /// tym nośnikiem. Dlatego zapisujemy z adnotacją źródła i bez znacznika
    /// potwierdzenia; ten należy do człowieka, który sprawdzi wersję na ekranie
    /// nawigacji.
    fn learn_installed_from_media(&mut self) {
        let found: Vec<_> = self
            .media
            .iter()
            .filter_map(|m| m.mount_point.as_deref())
            .filter_map(|p| mapstick_media::read_installed(p).ok().flatten())
            .collect();

        let mut changed = false;

        for installed in found {
            let Some(favourite) = self.store.get_mut(&installed.model_id) else {
                continue;
            };

            if favourite.installed_version.as_deref() != Some(&installed.firmware_version) {
                favourite.installed_version = Some(installed.firmware_version.clone());
                favourite.installed_source = Some(InstalledSource::VerFile);
                favourite.installed_confirmed = false;
                changed = true;

                tracing::info!(
                    "{}: wersja w aucie odczytana z nośnika — {}",
                    installed.model_id,
                    installed.firmware_version
                );
            }
        }

        if changed {
            self.save_store();
            self.push_favourites();
        }
    }

    fn select_medium(&mut self, index: usize) {
        let Some(medium) = self.media.get(index) else {
            return;
        };
        let Some(mount) = medium.mount_point.clone() else {
            self.set_status(
                tr!("This medium is not mounted — plug it in again or mount it."),
                true,
            );
            return;
        };

        self.forget_pause();

        let shown = mount.display().to_string();
        self.chosen_medium = Some(mount.clone());
        self.output_dir = Some(mount);
        let visible = index as i32;

        self.with_ui(move |ui| {
            ui.set_selected_medium(visible);
            ui.set_output_dir(shown.into());
        });

        self.push_target_state();
    }

    fn request_format(&mut self, index: usize) {
        let Some(medium) = self.media.get(index) else {
            return;
        };

        // Potwierdzenie pokazuje to, po czym człowiek rozpozna, że wskazał nie
        // ten nośnik: nazwę napędu, pojemność i ścieżkę urządzenia.
        let target = format!(
            "{}\n{}  ·  {}{}",
            medium_label(medium),
            medium.id,
            format_size(medium.total_bytes),
            medium
                .mount_point
                .as_ref()
                .map(|p| format!("  ·  {}", p.display()))
                .unwrap_or_default(),
        );

        self.pending_format = Some(index);
        self.with_ui(move |ui| {
            ui.set_format_target(target.into());
            ui.set_format_dialog(true);
        });
    }

    async fn confirm_format(&mut self) {
        let Some(index) = self.pending_format.take() else {
            return;
        };
        let Some(medium) = self.media.get(index).cloned() else {
            return;
        };

        self.with_ui(|ui| ui.set_formatting(true));
        self.set_status(tr!("Formatting {} as exFAT…", medium.id), false);

        let result = mapstick_media::format_exfat(&medium, "MAPSTICK").await;

        self.with_ui(|ui| {
            ui.set_formatting(false);
            ui.set_format_dialog(false);
        });

        match result {
            Ok(()) => {
                self.forget_formatted(&medium);
                self.set_status(tr!("{} formatted as exFAT.", medium.id), false);
                self.refresh_media().await;
            }
            Err(e) => self.set_status(tr!("Formatting failed: {}", describe(&e)), true),
        }
    }

    /// Zapomina wszystko, co aplikacja wiedziała o sformatowanym nośniku.
    ///
    /// Po formatowaniu nie ma tam ani plików, ani nawet dawnego punktu
    /// montowania — nazwa katalogu bierze się z etykiety, a tę właśnie
    /// zmieniliśmy. Cel zostaje pusty i wypełni go odświeżenie listy nośników.
    fn forget_formatted(&mut self, medium: &Medium) {
        let Some(mount) = medium.mount_point.clone() else {
            return;
        };

        if self
            .output_dir
            .as_deref()
            .is_some_and(|d| d.starts_with(&mount))
        {
            self.output_dir = None;
            self.with_ui(|ui| ui.set_output_dir(String::new().into()));
        }

        if self.chosen_medium.as_deref() == Some(mount.as_path()) {
            self.chosen_medium = None;
        }

        let wiped: Vec<String> = self
            .store
            .downloads
            .iter()
            .filter(|d| d.path.starts_with(&mount))
            .map(|d| d.model_id.clone())
            .collect();

        if !wiped.is_empty() {
            for model_id in wiped {
                self.store.forget_download(&model_id);
            }
            self.save_store();
        }
    }

    /// Kończy stan wstrzymania, gdy „Wznów” przestało cokolwiek znaczyć.
    ///
    /// Archiwów przy tym nie ruszamy: wznowienie po powrocie do tego samego
    /// modelu dalej ma z czego skorzystać, a niepotrzebne sprzątnie
    /// [`cache::sweep`], gdy producent wystawi nowszą wersję.
    fn forget_pause(&mut self) {
        self.running = None;
        self.with_ui(|ui| ui.set_paused(false));
    }

    /// Przyjmuje wskazany katalog jako cel.
    ///
    /// Przychodzi tu wynik okna wyboru — i to samo robi `MAPSTICK_DEV_CEL`
    /// przy testach na maszynie bez myszy.
    fn use_directory(&mut self, path: PathBuf) {
        tracing::info!("Cel wskazany ręcznie: {}", path.display());

        self.forget_pause();

        let shown = path.display().to_string();
        self.output_dir = Some(path);

        self.with_ui(move |ui| ui.set_output_dir(shown.into()));

        // Lista nośników jest już wykryta, więc propozycja skopiowania pokazuje
        // się od razu — bez naciskania „Odśwież”.
        self.push_target_state();
    }

    fn start_download(&mut self) -> Result<()> {
        let (Some(index), Some(firmware)) = (self.selected, self.firmware.clone()) else {
            return Err(anyhow!("Najpierw wybierz model i poczekaj na rozmiar."));
        };
        let output = self
            .output_dir
            .clone()
            .ok_or_else(|| anyhow!(tr!("No destination folder was chosen.")))?;

        let car = self.entries[index].car.clone();
        let authority = ambient_authority();
        Dir::create_ambient_dir_all(&output, authority)?;
        let out_dir = Dir::open_ambient_dir(&output, authority)?;

        let ui = self.ui.clone();
        let tx = self.tx.clone();
        let client = NuClientBuilder::new().build()?;

        // Opis pobrania kompletujemy tutaj, a nie po powrocie zadania: do tego
        // czasu użytkownik może już oglądać inny model.
        let record = Download {
            model_id: car.id.clone(),
            label: car.name.clone(),
            brand: BRAND_NAMES[self.brand_position()].to_owned(),
            region: self.region.clone(),
            version: car.versions.first().cloned().unwrap_or_default(),
            path: output.clone(),
            at: mapstick_store::now(),
        };

        // Sieroty po poprzednim wydaniu wyliczamy teraz, bo za chwilę nowy
        // manifest nadpisze stary i nie będzie już czym ich rozpoznać.
        let stale = self.stale_on_target(&output);

        // Domyślnie archiwa lądują na dysku, a na nośnik trafia sam wynik.
        // Zapisów na kość pamięci jest wtedy o połowę mniej, a rozpakowywanie
        // nakłada się w czasie z pobieraniem kolejnych archiwów.
        // Po udanym pobraniu archiwa nie są już do niczego potrzebne.
        let work = cache::for_download(&car.id, &record.version);
        let cleanup = (!self.direct_to_medium).then(|| work.clone());

        self.running = Some(Footprint {
            model_id: car.id.clone(),
            output: output.clone(),
            work: cleanup.clone(),
        });

        let (downloader, mut progress_rx) = if self.direct_to_medium {
            Downloader::new(out_dir, client, car, firmware, CONCURRENCY, RETRIES, false)
        } else {
            Dir::create_ambient_dir_all(&work, authority)?;
            let work_dir = Dir::open_ambient_dir(&work, authority)?;

            Downloader::new_split(
                out_dir,
                work_dir,
                client,
                car,
                firmware,
                CONCURRENCY,
                RETRIES,
                false,
            )
        };

        let _ = ui.upgrade_in_event_loop(|ui| {
            ui.set_downloading(true);
            ui.set_paused(false);
            ui.set_progress(0.0);
            ui.set_progress_text(tr!("Starting…").into());
        });

        self.download = Some(tokio::spawn(async move {
            let handle = downloader.download();
            tokio::pin!(handle);

            let (mut dl_done, mut dl_total) = (0u64, 0u64);
            let (mut pp_done, mut pp_total) = (0u64, 0u64);
            let mut last_paint = Instant::now() - UI_REFRESH;

            // Prędkość liczona z przyrostów między odmalowaniami. Same
            // przyrosty skaczą — pliki przychodzą kawałkami — więc pokazujemy
            // średnią wykładniczą, która nadąża, ale nie miga.
            let mut speed = 0f64;
            let mut counted = 0u64;
            let mut closed = false;

            // Bez własnego tykania pasek stoi, gdy sieć milczy, i pokazuje
            // prędkość sprzed zerwania połączenia.
            let mut tick = tokio::time::interval(UI_REFRESH * 5);

            let result = loop {
                tokio::select! {
                    r = &mut handle => break r,
                    _ = tick.tick() => {}
                    msg = progress_rx.recv(), if !closed => {
                        match msg {
                            Some(ProgressMessage::TotalDownload(b)) => dl_total = b,
                            Some(ProgressMessage::TotalPostProcess(b)) => pp_total = b,
                            Some(ProgressMessage::Download(b)) => dl_done += b,
                            Some(ProgressMessage::PostProcess(b)) => pp_done += b,
                            None => closed = true,
                        }
                    }
                }

                if last_paint.elapsed() < UI_REFRESH {
                    continue;
                }

                let since = last_paint.elapsed().as_secs_f64();
                last_paint = Instant::now();

                let just_now = dl_done.saturating_sub(counted) as f64 / since;
                counted = dl_done;
                speed = if speed == 0.0 {
                    just_now
                } else {
                    speed * 0.7 + just_now * 0.3
                };

                // Serwer bywa podaje zaniżone rozmiary, więc pasek
                // nigdy nie przekracza pełna.
                let (done, total) = if pp_total > 0 {
                    (dl_done + pp_done, dl_total + pp_total)
                } else {
                    (dl_done, dl_total)
                };
                let fraction = if total > 0 {
                    (done as f32 / total as f32).min(1.0)
                } else {
                    0.0
                };

                // Prędkość dotyczy sieci, więc znika, gdy zostało już tylko
                // rozpakowywanie.
                let rate = if dl_total > 0 && dl_done < dl_total && speed >= 1.0 {
                    format!(" ({}/s)", format_size(speed as u64))
                } else {
                    String::new()
                };

                // Jedna liczba na dwie fazy myliła: „96,1 GiB” przy
                // 48,1 GiB do pobrania wygląda jak błąd, a jest sumą
                // ściągania i rozpakowywania. Pasek liczy całą robotę,
                // ale podpis nazywa każdą fazę z osobna.
                let text = if pp_total > 0 {
                    format!(
                        "Pobrano {} z {}{} · rozpakowano {} z {}",
                        format_size(dl_done),
                        format_size(dl_total),
                        rate,
                        format_size(pp_done),
                        format_size(pp_total)
                    )
                } else {
                    format!(
                        "Pobrano {} z {}{}",
                        format_size(dl_done),
                        format_size(dl_total),
                        rate
                    )
                };

                tracing::debug!("pasek: {text}");

                let _ = ui.upgrade_in_event_loop(move |ui| {
                    ui.set_progress(fraction);
                    ui.set_progress_text(text.into());
                });
            };

            let succeeded = result.is_ok();

            // `{e:#}` zamiast `{e}`: anyhow pokazuje inaczej samą wierzchnią
            // warstwę („Failed to open directory: …”) i gubi powód, dla którego
            // się nie udało. Meldunek idzie też do dziennika — wcześniej
            // trafiał wyłącznie na pasek stanu i znikał razem z oknem.
            let message = match &result {
                Ok(()) => {
                    tracing::info!("Download finished");
                    tr!("Download finished. Checksums verified.")
                }
                Err(e) => {
                    tracing::warn!("Download failed: {e:#}");
                    tr!("Download failed: {}", format!("{e:#}"))
                }
            };

            // Sprzątamy tylko po powodzeniu — po błędzie archiwa są tym, co
            // pozwoli wznowić bez ściągania wszystkiego od nowa.
            let mut leftovers = String::new();
            if let Some(work) = &cleanup {
                if succeeded {
                    match std::fs::remove_dir_all(work) {
                        Ok(()) => tracing::info!("Usunięto archiwa z {}", work.display()),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => tracing::warn!("Nie udało się usunąć archiwów: {e}"),
                    }
                } else {
                    let size = cache::directory_size(work);
                    if size > 0 {
                        leftovers = format_size(size);
                    }
                }
            }

            // Stare mapy kasujemy dopiero po powodzeniu. Zrobione wcześniej
            // zwolniłoby miejsce, ale nieudane pobranie zostawiłoby nośnik
            // bez żadnej działającej wersji.
            if succeeded && !stale.is_empty() {
                let count = stale::remove(&output, &stale);
                tracing::info!(
                    "Usunięto {count} plików poprzedniego wydania ({} B) z {}",
                    stale.bytes,
                    output.display()
                );
            }

            // Wersję bierzemy z tego, co naprawdę wylądowało na dysku — spis
            // producenta mówi, co powinno przyjść, plik `.ver` mówi, co przyszło.
            if succeeded {
                let mut record = record;
                if let Ok(Some(installed)) = mapstick_media::read_installed(&output) {
                    record.version = installed.firmware_version;
                }
                let _ = tx.send(Cmd::DownloadFinished(Box::new(record)));
            }

            let _ = ui.upgrade_in_event_loop(move |ui| {
                ui.set_downloading(false);
                // Po nieudanym pobraniu zostaje dokładnie to samo, co po
                // wstrzymanym: archiwa do wznowienia albo do sprzątnięcia.
                ui.set_paused(!succeeded);
                ui.set_status(message.into());
                ui.set_status_is_error(!succeeded);
                ui.set_leftovers(leftovers.into());

                if succeeded {
                    ui.set_target_has_current(true);
                }
            });
        }));

        Ok(())
    }

    /// Kopiuje gotowe dane z dysku na nośnik.
    ///
    /// Nic tu nie idzie przez sieć — to przepisanie tego, co już mamy, więc
    /// całość leci na wątku blokującym, a nie w pętli asynchronicznej.
    fn start_copy(&mut self) -> Result<()> {
        let source = self.ready_copy().ok_or_else(|| {
            anyhow!(tr!("There is no ready firmware for this model on the disk."))
        })?;
        let target = self
            .copy_destination()
            .ok_or_else(|| anyhow!(tr!("No medium chosen.")))?;
        let index = self
            .selected
            .ok_or_else(|| anyhow!(tr!("No model chosen.")))?;

        let model_id = self.entries[index].car.id.clone();
        let version = self
            .store
            .download_for(&model_id)
            .map(|d| d.version.clone())
            .unwrap_or_default();

        let total = cache::directory_size(&source);

        // Tak samo jak przy pobieraniu: listę robimy teraz, kasujemy po fakcie.
        let stale = self.stale_on_target(&target);

        // Katalog przejściowy leży na nośniku, więc potrzeba tyle, ile waży
        // źródło — nie dwa razy tyle.
        if let Ok(space) = mapstick_media::space_for(&target)
            && space.free < mapstick_media::with_margin(total)
        {
            return Err(anyhow!(tr!(
                "The medium has {} free and the data takes {}.",
                format_size(space.free),
                format_size(total)
            )));
        }

        let cancel = Arc::new(AtomicBool::new(false));
        self.copy_cancel = Some(cancel.clone());

        let delete_source = self.delete_after_copy;
        let ui = self.ui.clone();
        let tx = self.tx.clone();

        self.with_ui(|ui| {
            ui.set_copying(true);
            ui.set_progress(0.0);
            ui.set_progress_text(tr!("Copying…").into());
        });

        self.download = Some(tokio::spawn(async move {
            let painter = ui.clone();
            let moved_source = source.clone();

            let result = tokio::task::spawn_blocking(move || {
                let mut done = 0u64;
                let mut last_paint = std::time::Instant::now() - UI_REFRESH;

                let outcome = copy::stage_and_place(&moved_source, &target, &cancel, &mut |n| {
                    done += n;

                    if last_paint.elapsed() < UI_REFRESH {
                        return;
                    }
                    last_paint = std::time::Instant::now();

                    let fraction = if total > 0 {
                        (done as f32 / total as f32).min(1.0)
                    } else {
                        0.0
                    };
                    let text = format!("{} z {}", format_size(done), format_size(total));

                    let _ = painter.upgrade_in_event_loop(move |ui| {
                        ui.set_progress(fraction);
                        ui.set_progress_text(text.into());
                    });
                })?;

                if outcome == copy::Outcome::Cancelled {
                    return Ok::<CopyEnd, anyhow::Error>(CopyEnd::Cancelled);
                }

                if !stale.is_empty() {
                    let count = stale::remove(&target, &stale);
                    tracing::info!(
                        "Usunięto {count} plików poprzedniego wydania ({} B)",
                        stale.bytes
                    );
                }

                // Kasujemy z dysku dopiero wtedy, gdy nośnik sam potwierdzi, że
                // ma tę wersję, i gdy przeczytaliśmy ze źródła tyle, ile waży.
                let removed = if delete_source {
                    let landed = mapstick_media::read_installed(&target)
                        .ok()
                        .flatten()
                        .is_some_and(|i| {
                            i.model_id == model_id && i.firmware_version == version && i.complete
                        });

                    if landed && done == cache::directory_size(&moved_source) {
                        std::fs::remove_dir_all(&moved_source)?;
                        Some(true)
                    } else {
                        Some(false)
                    }
                } else {
                    None
                };

                Ok(CopyEnd::Done {
                    model_id,
                    removed,
                })
            })
            .await;

            let mut forget = None;

            let (message, is_error) = match result {
                Ok(Ok(CopyEnd::Done { model_id, removed })) => {
                    forget = (removed == Some(true)).then_some(model_id);

                    match removed {
                        None => (tr!("Copied to the medium."), false),
                        Some(true) => (
                            tr!("Copied to the medium; the copy on the disk removed."),
                            false,
                        ),
                        Some(false) => (
                            tr!(
                                "Copied to the medium, but the copy on the disk was not \
                                 removed — I could not confirm they match. Check the medium \
                                 and remove it by hand."
                            ),
                            true,
                        ),
                    }
                }
                Ok(Ok(CopyEnd::Cancelled)) => (
                    tr!("Copying interrupted. The medium does not hold complete data."),
                    true,
                ),
                Ok(Err(e)) => (tr!("Copying failed: {}", e), true),
                Err(e) => (tr!("Copying interrupted: {}", e), true),
            };

            let _ = tx.send(Cmd::CopyEnded { forget });

            let _ = ui.upgrade_in_event_loop(move |ui| {
                ui.set_copying(false);
                ui.set_status(message.into());
                ui.set_status_is_error(is_error);
            });
        }));

        Ok(())
    }

    /// Przerwanie polega na porzuceniu przyszłości pobierania — nudl zapisuje
    /// postęp na bieżąco, więc wznowienie sprowadza się do ponownego startu.
    fn cancel(&mut self) {
        tracing::info!("Wstrzymane na życzenie");

        // Kopiowanie jest blokujące — `abort()` by go nie zatrzymał, więc
        // podnosimy flagę i czekamy, aż zauważy ją sama pętla kopiująca.
        if let Some(flag) = self.copy_cancel.take() {
            flag.store(true, Ordering::Relaxed);
            self.set_status(tr!("Interrupting the copy…"), false);
            return;
        }

        if let Some(handle) = self.download.take() {
            handle.abort();
        }

        self.with_ui(|ui| {
            ui.set_downloading(false);
            ui.set_paused(true);
            ui.set_status(
                tr!("Download paused. Click “Resume” to carry on from here.").into(),
            );
            ui.set_status_is_error(false);
        });
    }

    /// Przerywa pobieranie i usuwa to, co zdążyło po nim zostać.
    ///
    /// W odróżnieniu od wstrzymania nie ma po tym czego wznawiać — i o to
    /// chodzi: „anuluj” ma nie zostawiać na dysku dziesiątek gigabajtów,
    /// o których za pół roku nikt nie będzie pamiętał, skąd się wzięły.
    async fn abandon(&mut self) {

        // Najpierw niech zadanie naprawdę stanie. Kasowanie plików spod ręki
        // piszącego wątku zostawiłoby połowę z nich odtworzonych.
        if let Some(handle) = self.download.take() {
            handle.abort();
            let _ = handle.await;
        }

        let Some(footprint) = self.running.take() else {
            self.cancel();
            return;
        };

        let mut freed = 0u64;

        if let Some(work) = &footprint.work {
            freed += cache::directory_size(work);

            if let Err(e) = std::fs::remove_dir_all(work)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!("Nie udało się usunąć {}: {e}", work.display());
            }

            // Katalog modelu bez żadnej wersji w środku nie ma po co zostawać.
            if let Some(model) = work.parent()
                && std::fs::read_dir(model).is_ok_and(|mut d| d.next().is_none())
            {
                let _ = std::fs::remove_dir(model);
            }
        }

        freed += remove_partial_archives(&footprint.output);

        // Manifest powstaje na początku pobierania, więc po anulowaniu opisuje
        // wydanie, którego tam nie ma. Usuwamy i jego, i to, co wymienia.
        let doomed = stale::find(&footprint.output, &footprint.model_id, &HashSet::new());
        freed += doomed.bytes;
        stale::remove(&footprint.output, &doomed);

        let manifest = footprint
            .output
            .join(format!("{}.ver", footprint.model_id));
        if let Err(e) = std::fs::remove_file(&manifest)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("Nie udało się usunąć {}: {e}", manifest.display());
        }

        tracing::info!(
            "Pobranie {} anulowane, odzyskane {freed} B",
            footprint.model_id
        );

        self.with_ui(|ui| {
            ui.set_downloading(false);
            ui.set_paused(false);
            ui.set_progress(0.0);
            ui.set_progress_text(String::new().into());
        });

        self.push_target_state();
        self.push_leftovers();
        self.set_status(
            tr!("Download cancelled — leftovers removed: {}.", format_size(freed)),
            false,
        );
    }

    /// Odtwarza stan po uruchomieniu: ulubione, ich wersje i ostatni model.
    async fn startup(&mut self, restore: bool) {
        let freed = cache::migrate();
        if freed > 0 {
            tracing::info!("Usunięto archiwa w starym układzie: {freed} B");
        }

        // Po samym czasie — pełnych spisów wersji jeszcze nie mamy.
        self.sweep_cache(HashMap::new());

        self.store = match Store::load(&self.store_path) {
            Ok(store) => store,
            Err(e) => {
                self.set_status(tr!("Could not load favourites: {}", describe(&e)), true);
                Store::default()
            }
        };

        self.push_favourites();

        // Harmonogram zakładamy na nowo przy każdym starcie, jeśli jest
        // komu pilnować. To, co system ma w plikach, pochodzi z chwili
        // zaznaczenia pola — starsza wersja aplikacji mogła zapisać inny
        // rytm albo inną ścieżkę do binarki. Zakładanie jest idempotentne
        // i trwa ułamek sekundy.
        if self.store.watched().next().is_some() {
            match schedule::install(self.store.check_interval) {
                Ok(kind) => tracing::info!("Harmonogram odświeżony: {}", kind.label()),
                Err(e) => tracing::warn!("Nie udało się odświeżyć harmonogramu: {e:#}"),
            }
        }

        self.spawn_favourite_refresh();

        self.link_table = link::Table::load();
        tracing::info!("Tablica odnośników: {} wpisów", self.link_table.links.len());
        self.spawn_link_refresh(self.combos_in_use());

        if !restore {
            return;
        }

        let Some(last) = self.store.last_selection.clone() else {
            self.set_status(
                tr!("Pick a brand and a region — the model list loads by itself."),
                false,
            );
            return;
        };

        let (brand, region) = (brand_index(&last.brand), region_index(&last.region));
        let (b, r) = (brand as i32, region as i32);
        self.with_ui(move |ui| {
            ui.set_brand_index(b);
            ui.set_region_index(r);
        });

        self.pending_select = last.model_id;

        if let Err(e) = self.load_models(brand, region).await {
            self.set_status(tr!("Could not fetch the list: {}", e), true);
        }
    }

    /// Marki i regiony, którymi użytkownik faktycznie się posługuje.
    ///
    /// Tablicę odnośników budujemy tylko dla nich: pełny katalog to trzynaście
    /// regionów razy trzy marki, czyli tysiące zapytań o pliki, których nikt
    /// nigdy nie otworzy.
    fn combos_in_use(&self) -> Vec<(String, String)> {
        let mut combos: Vec<(String, String)> = self
            .store
            .items
            .iter()
            .map(|f| (f.brand.clone(), f.region.clone()))
            .collect();

        combos.push((
            BRAND_NAMES[self.brand_position()].to_owned(),
            self.region.clone(),
        ));

        if let Some(last) = &self.store.last_selection {
            combos.push((last.brand.clone(), last.region.clone()));
        }

        combos.sort();
        combos.dedup();
        combos
    }

    /// Zbiera odnośniki w tle, żeby kliknięcie na stronie trafiło od razu.
    fn spawn_link_refresh(&self, combos: Vec<(String, String)>) {
        if combos.is_empty() {
            return;
        }

        let tx = self.tx.clone();

        tokio::spawn(async move {
            let mut found = Vec::new();

            for (brand, region) in combos {
                let code = BRANDS[brand_index(&brand)].as_code_str();

                match link::collect(&brand, code, &region).await {
                    Ok(mut links) => found.append(&mut links),
                    Err(e) => tracing::warn!("Odnośniki {brand}/{region}: {e}"),
                }
            }

            if !found.is_empty() {
                let _ = tx.send(Cmd::LinksCollected(found));
            }
        });
    }

    fn apply_links(&mut self, found: Vec<(String, link::Target)>) {
        let added = self.link_table.merge(found);
        tracing::info!(
            "Tablica odnośników: {} wpisów ({added} nowych)",
            self.link_table.links.len()
        );

        if let Err(e) = self.link_table.save() {
            tracing::warn!("Nie udało się zapisać tablicy odnośników: {e}");
        }
    }

    /// Otwiera pojazd wskazany odnośnikiem ze strony producenta.
    ///
    /// Sam odnośnik jest nieczytelny, więc rozpoznajemy go przez zestawienie
    /// z katalogiem. Gdy go w tablicy nie ma — bo modelowi właśnie wyszła nowa
    /// wersja i ciąg się zmienił — dociągamy katalog i próbujemy raz jeszcze.
    async fn open_link(&mut self, url: String) {
        let Some(brand) = link::brand_of(&url) else {
            self.set_status(tr!("This is not an update link."), true);
            return;
        };

        if let Some(target) = self.link_table.find(&url).cloned() {
            self.open_target(target).await;
            return;
        }

        self.set_status(tr!("Recognising the link from the maker’s site…"), false);

        // Najpierw regiony, których użytkownik używa — auto stoi w jednym.
        let regions: Vec<String> = self
            .combos_in_use()
            .into_iter()
            .filter(|(b, _)| b == brand)
            .map(|(_, r)| r)
            .collect();
        let regions = if regions.is_empty() {
            vec![self.region.clone()]
        } else {
            regions
        };

        let code = BRANDS[brand_index(brand)].as_code_str();

        for region in &regions {
            match link::collect(brand, code, region).await {
                Ok(found) => self.apply_links(found),
                Err(e) => tracing::warn!("Odnośniki {brand}/{region}: {e}"),
            }

            if let Some(target) = self.link_table.find(&url).cloned() {
                self.open_target(target).await;
                return;
            }
        }

        self.set_status(
            tr!(
                "I don’t recognise this link. I checked {} in these regions: {}. If the \
                 car is from another region, pick it from the list and click again.",
                brand,
                regions.join(", ")
            ),
            true,
        );
    }

    async fn open_target(&mut self, target: link::Target) {
        let (brand, region) = (brand_index(&target.brand), region_index(&target.region));
        let (b, r) = (brand as i32, region as i32);

        self.with_ui(move |ui| {
            ui.set_page(0);
            ui.set_brand_index(b);
            ui.set_region_index(r);
        });

        self.pending_select = None;
        self.pending_mcode = Some(target.mcode.clone());
        self.set_busy(true);

        tracing::info!(
            "Odnośnik wskazuje {} ({}), {}",
            target.name,
            target.mcode,
            target.version
        );

        if let Err(e) = self.load_models(brand, region).await {
            self.set_status(tr!("Could not fetch the list: {}", e), true);
        }

        self.set_busy(false);
    }

    async fn open_favourite(&mut self, model_id: String) {
        let Some(favourite) = self.store.find(&model_id) else {
            return;
        };

        let (brand, region) = (
            brand_index(&favourite.brand),
            region_index(&favourite.region),
        );
        let (b, r) = (brand as i32, region as i32);
        self.with_ui(move |ui| {
            ui.set_page(0);
            ui.set_brand_index(b);
            ui.set_region_index(r);
        });

        self.pending_select = Some(model_id);
        self.set_busy(true);

        if let Err(e) = self.load_models(brand, region).await {
            self.set_status(tr!("Could not fetch the list: {}", e), true);
        }

        self.set_busy(false);
    }

    async fn run(mut self, mut rx: UnboundedReceiver<Cmd>) {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                Cmd::LoadModels { brand, region } => {
                    self.set_busy(true);
                    self.set_status(tr!("Fetching the model list…"), false);

                    if let Err(e) = self.load_models(brand, region).await {
                        self.entries.clear();
                        self.push_rows();
                        self.set_status(tr!("Could not fetch the list: {}", e), true);
                    }

                    self.set_busy(false);
                }
                Cmd::Filter(text) => {
                    self.filter = text;
                    self.push_rows();
                }
                Cmd::Select(i) => {
                    if let Err(e) = self.select(i).await {
                        self.with_ui(|ui| ui.set_detail_size(tr!("size unknown").into()));
                        self.set_status(tr!("Could not fetch the details: {}", e), true);
                    }
                }
                Cmd::UseDirectory(path) => self.use_directory(path),
                Cmd::StartDownload => {
                    if let Err(e) = self.start_download() {
                        self.set_status(e.to_string(), true);
                    }
                }
                Cmd::Cancel => self.cancel(),
                Cmd::Abandon => self.abandon().await,
                Cmd::SetDirectMode(direct) => {
                    self.direct_to_medium = direct;
                    self.push_space_note();
                    self.push_media_rows();
                }
                Cmd::ClearCache => self.clear_cache(),
                Cmd::Startup { restore } => self.startup(restore).await,
                Cmd::ToggleFavourite => self.toggle_favourite(),
                Cmd::SetWatch(on) => self.set_watch(on),
                Cmd::SetInterval(index) => self.set_interval(index),
                Cmd::SetInstalled(index) => self.set_installed(index),
                Cmd::OpenFavourite(id) => self.open_favourite(id).await,
                Cmd::FavouritesRefreshed(versions) => self.apply_favourite_versions(versions),
                Cmd::OpenLink(url) => self.open_link(url).await,
                Cmd::LinksCollected(found) => self.apply_links(found),
                Cmd::DownloadFinished(record) => {
                    self.record_download(*record);
                    self.refresh_space().await;
                }
                Cmd::StartCopy => {
                    if let Err(e) = self.start_copy() {
                        self.copy_cancel = None;
                        self.with_ui(|ui| ui.set_copying(false));
                        self.set_status(e.to_string(), true);
                    }
                }
                Cmd::SetDeleteAfterCopy(delete) => self.delete_after_copy = delete,
                Cmd::CopyEnded { forget } => {
                    self.copy_cancel = None;

                    if let Some(model_id) = forget {
                        self.store.forget_download(&model_id);
                        self.save_store();
                    }

                    self.refresh_space().await;
                }
                Cmd::GoToMedia => {
                    let summary = match (&self.selected, self.required_bytes()) {
                        (Some(i), needed) if needed > 0 => tr!(
                            "{} — {} to write",
                            self.entries[*i].car.name,
                            format_size(needed)
                        ),
                        (Some(i), _) => tr!("{} — size unknown", self.entries[*i].car.name),
                        _ => tr!("No model chosen"),
                    };

                    self.with_ui(move |ui| {
                        ui.set_target_summary(summary.into());
                        ui.set_page(1);
                    });
                    self.push_space_note();
                    self.push_target_state();
                    self.push_leftovers();
                    self.refresh_media().await;
                }
                Cmd::GoBack => self.with_ui(|ui| ui.set_page(0)),
                Cmd::RefreshMedia => self.refresh_media().await,
                Cmd::SelectMedium(i) => self.select_medium(i),
                Cmd::RemoveStale(i) => {
                    self.remove_stale(i);
                    self.refresh_media().await;
                }
                Cmd::RequestFormat(i) => self.request_format(i),
                Cmd::ConfirmFormat => self.confirm_format().await,
                Cmd::CancelFormat => {
                    self.pending_format = None;
                    self.with_ui(|ui| ui.set_format_dialog(false));
                }
                Cmd::RequestRedownload => self.request_redownload(),
                Cmd::ConfirmRedownload => self.confirm_redownload(),
                Cmd::CancelRedownload => {
                    self.with_ui(|ui| ui.set_redownload_dialog(false));
                }
            }
        }
    }
}

fn main() -> Result<()> {
    // Domyślnie INFO, ale RUST_LOG bierze górę — bez tego jedyną drogą do
    // szczegółów (choćby postępu odmalowywanego dziesięć razy na sekundę)
    // byłaby przebudowa aplikacji.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // MAPSTICK_LANG wygrywa nad systemem — przydaje się do testów i temu,
    // kto po prostu woli inny język niż ten, który ma ustawiony pulpit.
    let preferred = std::env::var("MAPSTICK_LANG").ok();
    let language = i18n::choose(preferred.as_deref());
    tracing::info!(
        "Język interfejsu: {}",
        language.unwrap_or("en (domyślny)")
    );

    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!(tr!("Could not set up the rustls crypto provider.")))?;

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Sprawdzenie z harmonogramu: bez okna, bez pętli zdarzeń, bez ekranu.
    // Kończy się samo, więc między sprawdzeniami nie zostaje ani jeden proces.
    if args.iter().any(|a| a == "--sprawdz") {
        let force = args.iter().any(|a| a == "--teraz");

        return watch::run(force);
    }

    // Uruchomienie z odnośnikiem ze strony producenta albo z powiadomienia.
    // Jeśli okno już stoi otwarte, oddajemy mu wiadomość i kończymy — drugie
    // okno nikomu nie służy.
    let message = args
        .first()
        .filter(|a| link::is_link(a) || a.starts_with(instance::MODEL_PREFIX))
        .cloned();

    if let Some(message) = &message
        && instance::hand_off(message)
    {
        return Ok(());
    }

    let ui = AppWindow::new()?;

    // Slint tłumaczy napisy z .slint po tym samym kodzie języka, którego
    // właśnie ustaliliśmy dla napisów budowanych w Rustcie — inaczej okno
    // mówiłoby dwoma różnymi językami naraz.
    slint::select_bundled_translation(language.unwrap_or("")).ok();

    let regions: Vec<SharedString> = REGIONS
        .iter()
        .map(|(_, label)| i18n::tr(label).into())
        .collect();
    ui.set_regions(ModelRc::new(VecModel::from(regions)));

    let intervals: Vec<SharedString> = mapstick_store::Interval::ALL
        .iter()
        .map(|i| i18n::tr(i.label()).into())
        .collect();
    ui.set_intervals(ModelRc::new(VecModel::from(intervals)));
    ui.set_models(ModelRc::new(VecModel::<ModelRow>::default()));
    ui.set_status(tr!("Pick a brand and a region — the model list loads by itself.").into());

    let (tx, rx) = mpsc::unbounded_channel();

    let weak = ui.as_weak();
    let worker_tx = tx.clone();
    std::thread::Builder::new()
        .name("mapstick-net".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("Nie udało się uruchomić środowiska tokio");

            runtime.block_on(Worker::new(weak, worker_tx).run(rx));
        })?;

    instance::listen({
        let t = tx.clone();
        move |message| {
            let _ = t.send(incoming(message));
        }
    });

    // Pomoc dla testów na maszynie bez sterowania myszą: MAPSTICK_DEV=lista
    // wczytuje modele, MAPSTICK_DEV=nosnik dodatkowo wchodzi w widok nośnika.
    let _ = tx.send(Cmd::Startup {
        restore: message.is_none(),
    });

    if let Some(message) = message {
        let _ = tx.send(incoming(message));
    }

    if let Ok(stage @ ("lista" | "nosnik")) = std::env::var("MAPSTICK_DEV").as_deref() {
        let _ = tx.send(Cmd::LoadModels {
            brand: 0,
            region: 0,
        });

        if let Ok(filter) = std::env::var("MAPSTICK_DEV_MODEL") {
            let _ = tx.send(Cmd::Filter(filter));
        }

        let _ = tx.send(Cmd::Select(0));

        if std::env::var_os("MAPSTICK_DEV_FAV").is_some() {
            let _ = tx.send(Cmd::ToggleFavourite);
        }

        // MAPSTICK_DEV_PILNUJ=1 włącza pilnowanie modelu, =0 wyłącza — bez
        // myszy nie ma jak nacisnąć tego pola.
        if let Ok(on) = std::env::var("MAPSTICK_DEV_PILNUJ") {
            let _ = tx.send(Cmd::SetWatch(on == "1"));
        }

        if std::env::var_os("MAPSTICK_DEV_DIRECT").is_some() {
            let _ = tx.send(Cmd::SetDirectMode(true));
        }

        if stage == "nosnik" {
            let _ = tx.send(Cmd::GoToMedia);
            // Polecenia są przetwarzane po kolei, a GoToMedia czeka na
            // wykrycie nośników, więc lista jest już gotowa.
            let _ = tx.send(Cmd::SelectMedium(0));
        }

        // MAPSTICK_DEV_CEL wskazuje katalog docelowy zamiast okna wyboru,
        // a MAPSTICK_DEV_KOPIUJ od razu uruchamia kopiowanie z dysku.
        if let Ok(target) = std::env::var("MAPSTICK_DEV_CEL") {
            let _ = tx.send(Cmd::UseDirectory(PathBuf::from(target)));

            // MAPSTICK_DEV_POBIERZ rusza z pobieraniem od razu — inaczej
            // paska postępu nie da się obejrzeć bez myszy.
            if std::env::var_os("MAPSTICK_DEV_POBIERZ").is_some() {
                let _ = tx.send(Cmd::StartDownload);
            }

            // MAPSTICK_DEV_WSTRZYMAJ=<sekundy> wstrzymuje pobieranie, tak jak
            // przycisk; razem z MAPSTICK_DEV_ANULUJ pozwala sprawdzić
            // sprzątanie z poziomu wstrzymania.
            if let Ok(after) = std::env::var("MAPSTICK_DEV_WSTRZYMAJ")
                && let Ok(seconds) = after.parse::<u64>()
            {
                let t = tx.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(seconds));
                    let _ = t.send(Cmd::Cancel);
                });
            }

            // MAPSTICK_DEV_ANULUJ=<sekundy> anuluje je po chwili — inaczej
            // sprzątania po anulowaniu nie da się sprawdzić bez myszy.
            if let Ok(after) = std::env::var("MAPSTICK_DEV_ANULUJ")
                && let Ok(seconds) = after.parse::<u64>()
            {
                let t = tx.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(seconds));
                    let _ = t.send(Cmd::Abandon);
                });
            }

            if let Ok(mode) = std::env::var("MAPSTICK_DEV_KOPIUJ").as_deref() {
                let _ = tx.send(Cmd::SetDeleteAfterCopy(mode == "przenies"));
                let _ = tx.send(Cmd::StartCopy);
            }

            // MAPSTICK_DEV_PONOWNIE przechodzi całą drogę „Pobierz ponownie”
            // razem z potwierdzeniem — okna dialogowego nie ma jak kliknąć
            // bez myszy.
            if std::env::var_os("MAPSTICK_DEV_PONOWNIE").is_some() {
                let _ = tx.send(Cmd::RequestRedownload);
                let _ = tx.send(Cmd::ConfirmRedownload);
            }
        }
    }

    wire_callbacks(&ui, tx);

    // MAPSTICK_DEV_WYBOR naciska „Inny katalog…” za użytkownika — bez myszy
    // nie ma jak tego sprawdzić.
    if std::env::var_os("MAPSTICK_DEV_WYBOR").is_some() {
        ui.invoke_pick_output_dir();
    }

    #[cfg(windows)]
    fit_on_screen(&ui);

    if let Err(e) = ui.run() {
        #[cfg(windows)]
        if needs_software_renderer(&e) {
            drop(ui);
            return relaunch_with_software_renderer(&e);
        }

        return Err(e.into());
    }
    instance::cleanup();

    Ok(())
}

/// Przesuwa okno na środek ekranu, a na ekranie mniejszym niż okno najpierw
/// je zmniejsza.
///
/// Windows kładzie nowe okno kaskadowo, z przesunięciem od lewego górnego
/// rogu, i nie pilnuje, czy zmieści się na ekranie: przy 1280 px szerokości
/// prawa krawędź okna (1080 px) wychodziła poza ekran, przy 1024 px nie
/// zmieściłoby się wcale. Na Linuksie okna rozkłada kompozytor, a Wayland
/// i tak nie pozwala aplikacji ustawiać ich położenia.
///
/// Okno winit powstaje dopiero w pętli zdarzeń — stąd zadanie asynchroniczne,
/// które na nie czeka.
#[cfg(windows)]
fn fit_on_screen(ui: &AppWindow) {
    use slint::winit_030::{
        WinitWindowAccessor,
        winit::dpi::{PhysicalPosition, PhysicalSize},
    };

    let weak = ui.as_weak();
    let spawned = slint::spawn_local(async move {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let Ok(window) = ui.window().winit_window().await else {
            return;
        };
        let Some(monitor) = window.current_monitor() else {
            return;
        };

        let screen = monitor.size();
        let origin = monitor.position();
        let outer = window.outer_size();
        let inner = window.inner_size();

        // Pasek zadań zabiera dół ekranu, a winit nie zna obszaru roboczego —
        // zostawiamy na niego zapas.
        let room = PhysicalSize::new(screen.width, screen.height * 92 / 100);

        let frame = PhysicalSize::new(
            outer.width.saturating_sub(inner.width),
            outer.height.saturating_sub(inner.height),
        );
        let fitted = PhysicalSize::new(outer.width.min(room.width), outer.height.min(room.height));

        if fitted != outer {
            let _ = window.request_inner_size(PhysicalSize::new(
                fitted.width.saturating_sub(frame.width),
                fitted.height.saturating_sub(frame.height),
            ));
        }

        window.set_outer_position(PhysicalPosition::new(
            origin.x + ((room.width - fitted.width) / 2) as i32,
            origin.y + ((room.height - fitted.height) / 2) as i32,
        ));
    });

    if let Err(e) = spawned {
        tracing::debug!("Nie dopasuję okna do ekranu: {e}");
    }
}

/// Czy okno padło na braku OpenGL, a nie na czymś, czego zmiana renderera
/// nie naprawi.
///
/// Femtovg (domyślny renderer Slinta) chce OpenGL 2+. Windows bez sterownika
/// grafiki — maszyna wirtualna, Pulpit zdalny, stary laptop — ma tylko
/// awaryjne OpenGL 1.1, w którym kontekst się tworzy, a dopiero pierwsze
/// `glCreateShader` zawodzi. Slint przełącza się na renderer programowy
/// tylko wtedy, gdy renderer nie da się w ogóle utworzyć, więc tego
/// przypadku sam nie złapie. `SLINT_BACKEND` ustawione ręcznie znaczy, że
/// ktoś wybrał renderer świadomie — wtedy nie zgadujemy.
#[cfg(windows)]
fn needs_software_renderer(error: &slint::PlatformError) -> bool {
    std::env::var_os("SLINT_BACKEND").is_none() && error.to_string().contains("OpenGL")
}

/// Uruchamia MapSticka jeszcze raz, z rendererem programowym, i od razu
/// kończy ten proces. Wyboru platformy Slint nie da się cofnąć w tym samym
/// procesie, a czekać na następcę nie wolno: wątek sieciowy już ruszył
/// i zapisywałby te same pliki co on.
#[cfg(windows)]
fn relaunch_with_software_renderer(error: &slint::PlatformError) -> Result<()> {
    use anyhow::Context as _;

    tracing::warn!("Brak używalnego OpenGL ({error}), uruchamiam z rendererem programowym");

    std::process::Command::new(std::env::current_exe()?)
        .args(std::env::args_os().skip(1))
        .env("SLINT_BACKEND", "winit-software")
        .spawn()
        .with_context(|| tr!("Could not start MapStick again"))?;

    std::process::exit(0);
}

/// Wiadomość z zewnątrz: odnośnik ze strony producenta albo model
/// z powiadomienia o nowym wydaniu.
fn incoming(message: String) -> Cmd {
    match message.strip_prefix(instance::MODEL_PREFIX) {
        Some(model_id) => Cmd::OpenFavourite(model_id.to_owned()),
        None => Cmd::OpenLink(message),
    }
}

fn wire_callbacks(ui: &AppWindow, tx: UnboundedSender<Cmd>) {
    let t = tx.clone();
    ui.on_load_models(move |brand, region| {
        let _ = t.send(Cmd::LoadModels {
            brand: brand.max(0) as usize,
            region: region.max(0) as usize,
        });
    });

    let t = tx.clone();
    ui.on_filter_changed(move |text| {
        let _ = t.send(Cmd::Filter(text.to_string()));
    });

    let t = tx.clone();
    ui.on_select_row(move |index| {
        if index >= 0 {
            let _ = t.send(Cmd::Select(index as usize));
        }
    });

    let t = tx.clone();
    let weak = ui.as_weak();
    ui.on_pick_output_dir(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };

        // Okno wyboru dostaje MapSticka za rodzica. Bez tego kompozytor nie ma
        // powodu stawiać go nad oknem aplikacji, a schowane za nim wygląda
        // dokładnie tak, jakby przycisk nie działał.
        let dialog = rfd::AsyncFileDialog::new()
            .set_title(tr!("Target folder"))
            .set_parent(&ui.window().window_handle());

        // Czekamy w pętli interfejsu, a nie w wątku poleceń: tam czekanie
        // wstrzymałoby wszystko inne — odświeżanie nośników, wybór modelu,
        // odnośnik ze strony producenta — dopóki okno stoi otwarte.
        let t = t.clone();
        let spawned = slint::spawn_local(async move {
            match dialog.pick_folder().await {
                Some(dir) => {
                    let _ = t.send(Cmd::UseDirectory(dir.path().to_path_buf()));
                }
                None => tracing::info!("Wybór katalogu anulowany"),
            }
        });

        if let Err(e) = spawned {
            tracing::warn!("Nie udało się otworzyć okna wyboru katalogu: {e}");
        }
    });

    let t = tx.clone();
    ui.on_set_watch(move |on| {
        let _ = t.send(Cmd::SetWatch(on));
    });

    let t = tx.clone();
    ui.on_set_interval(move |index| {
        let _ = t.send(Cmd::SetInterval(index.max(0) as usize));
    });

    let t = tx.clone();
    ui.on_set_installed(move |index| {
        let _ = t.send(Cmd::SetInstalled(index.max(0) as usize));
    });

    let t = tx.clone();
    ui.on_start_download(move || {
        let _ = t.send(Cmd::StartDownload);
    });

    let t = tx.clone();
    ui.on_start_copy(move || {
        let _ = t.send(Cmd::StartCopy);
    });

    let t = tx.clone();
    ui.on_set_delete_after_copy(move |delete| {
        let _ = t.send(Cmd::SetDeleteAfterCopy(delete));
    });

    let t = tx.clone();
    ui.on_abandon_download(move || {
        let _ = t.send(Cmd::Abandon);
    });

    let t = tx.clone();
    ui.on_cancel_download(move || {
        let _ = t.send(Cmd::Cancel);
    });

    let t = tx.clone();
    ui.on_go_to_media(move || {
        let _ = t.send(Cmd::GoToMedia);
    });

    let t = tx.clone();
    ui.on_go_back(move || {
        let _ = t.send(Cmd::GoBack);
    });

    let t = tx.clone();
    ui.on_refresh_media(move || {
        let _ = t.send(Cmd::RefreshMedia);
    });

    let t = tx.clone();
    ui.on_select_medium(move |index| {
        if index >= 0 {
            let _ = t.send(Cmd::SelectMedium(index as usize));
        }
    });

    let t = tx.clone();
    ui.on_remove_stale(move |index| {
        let _ = t.send(Cmd::RemoveStale(index.max(0) as usize));
    });

    let t = tx.clone();
    ui.on_request_format(move |index| {
        if index >= 0 {
            let _ = t.send(Cmd::RequestFormat(index as usize));
        }
    });

    let t = tx.clone();
    ui.on_confirm_format(move || {
        let _ = t.send(Cmd::ConfirmFormat);
    });

    let t = tx.clone();
    ui.on_request_redownload(move || {
        let _ = t.send(Cmd::RequestRedownload);
    });

    let t = tx.clone();
    ui.on_confirm_redownload(move || {
        let _ = t.send(Cmd::ConfirmRedownload);
    });

    let t = tx.clone();
    ui.on_cancel_redownload(move || {
        let _ = t.send(Cmd::CancelRedownload);
    });

    let t = tx.clone();
    ui.on_cancel_format(move || {
        let _ = t.send(Cmd::CancelFormat);
    });

    let t = tx.clone();
    ui.on_set_direct_mode(move |direct| {
        let _ = t.send(Cmd::SetDirectMode(direct));
    });

    let t = tx.clone();
    ui.on_clear_cache(move || {
        let _ = t.send(Cmd::ClearCache);
    });

    let t = tx.clone();
    ui.on_toggle_favourite(move || {
        let _ = t.send(Cmd::ToggleFavourite);
    });

    ui.on_open_favourite(move |id| {
        let _ = tx.send(Cmd::OpenFavourite(id.to_string()));
    });
}
