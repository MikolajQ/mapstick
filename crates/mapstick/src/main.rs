// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! MapStick — graficzna nakładka na nudl.

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
mod instance;
mod link;
mod stale;

slint::include_modules!();

/// Regiony obsługiwane przez serwis producenta, w kolejności wyświetlania.
const REGIONS: &[(&str, &str)] = &[
    ("EU", "Europa"),
    ("BR", "Brazylia"),
    ("CA", "Kanada"),
    ("ID", "Indonezja"),
    ("IN", "Indie"),
    ("JP", "Japonia"),
    ("KR", "Korea Południowa"),
    ("ME", "Bliski Wschód"),
    ("NZ", "Nowa Zelandia"),
    ("RU", "Rosja i WNP"),
    ("SG", "Singapur"),
    ("TR", "Turcja"),
    ("US", "Stany Zjednoczone"),
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
    OpenFavourite(String),
    LoadModels { brand: usize, region: usize },
    Filter(String),
    Select(usize),
    PickOutputDir,
    StartDownload,
    Cancel,
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
    version: Option<String>,
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
                    (Some(released), Some(v)) => format!("{released} - {v}"),
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
                "Wybierz model i naciśnij „Dodaj do ulubionych”, żeby widzieć tu jego wersję \
                 przy każdym uruchomieniu."
                    .into()
            } else {
                "".into()
            });
        });
    }

    fn push_selected_favourite_state(&self) {
        let is_favourite = self
            .selected
            .map(|i| self.store.contains(&self.entries[i].car.id))
            .unwrap_or(false);

        self.with_ui(move |ui| ui.set_selected_is_favourite(is_favourite));
    }

    fn toggle_favourite(&mut self) {
        let Some(index) = self.selected else {
            return;
        };
        let car = &self.entries[index].car;

        if self.store.remove(&car.id) {
            self.set_status(format!("{} usunięty z ulubionych.", car.name), false);
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
            });
            self.set_status(format!("{} dodany do ulubionych.", car.name), false);

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

            favourite.latest_seen_version = entry.car.versions.first().cloned();
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
                                version: e.car.versions.into_iter().next(),
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

            favourite.latest_seen_version = item.version;
            favourite.latest_released = item.released;
            favourite.latest_checked_at = Some(mapstick_store::now());
            changed = true;
        }

        if changed {
            self.save_store();
            self.push_favourites();
        }
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

        self.set_status(format!("Znaleziono {count} modeli."), false);

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

        let entry = &self.entries[index];
        let (name, id, version, media, released) = (
            entry.car.name.clone(),
            entry.car.id.clone(),
            entry.car.versions.join(", "),
            entry.media.clone(),
            entry.released.clone(),
        );
        let visible = visible_index as i32;

        self.store.last_selection = Some(LastSelection {
            brand: BRAND_NAMES[self.brand_position()].to_owned(),
            region: self.region.clone(),
            model_id: Some(entry.car.id.clone()),
        });
        self.save_store();
        self.push_selected_favourite_state();

        self.with_ui(move |ui| {
            ui.set_selected_index(visible);
            ui.set_detail_name(name.into());
            ui.set_detail_id(id.into());
            ui.set_detail_version(version.into());
            ui.set_detail_media(media.into());
            ui.set_detail_released(released.into());
            ui.set_detail_size("sprawdzam rozmiar…".into());
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
            (0, _) => format!(
                "{} do pobrania · plików: {}",
                format_size(download),
                firmware.files.len()
            ),
            (_, false) => format!(
                "{} · plików: {}",
                format_size(extracted),
                firmware.files.len()
            ),
            (_, true) => format!(
                "{} do pobrania, {} na nośniku · plików: {}",
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
                        i.model_id == car.id && car.versions.contains(&i.firmware_version)
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

    /// Jedyny podłączony nośnik przejmuje rolę celu.
    ///
    /// Dzieje się to, gdy celu jeszcze nie ma albo gdy dotychczasowy okazał się
    /// katalogiem z gotowym oprogramowaniem — taki katalog jest źródłem do
    /// skopiowania, nie miejscem, w które cokolwiek pobierzemy. Ręcznie
    /// wybranego katalogu bez gotowych plików nikt nie podmienia.
    fn adopt_single_medium(&mut self) {
        if self.media.len() != 1 || self.media[0].mount_point.is_none() {
            return;
        }

        let target_is_source = self
            .selected
            .map(|i| &self.entries[i].car.id)
            .and_then(|id| self.store.download_for(id))
            .is_some_and(|d| Some(d.path.as_path()) == self.output_dir.as_deref());

        if self.output_dir.is_none() || target_is_source {
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
    fn ready_copy(&self) -> Option<PathBuf> {
        let index = self.selected?;
        let car = &self.entries[index].car;
        let record = self.store.download_for(&car.id)?;

        // Interesuje nas tylko to, co zgadza się z bieżącym wydaniem.
        if !car.versions.contains(&record.version) {
            return None;
        }

        // Kopiowanie katalogu w to samo miejsce nie ma sensu.
        if self.output_dir.as_deref() == Some(record.path.as_path()) {
            return None;
        }

        let installed = mapstick_media::read_installed(&record.path).ok().flatten()?;

        (installed.model_id == car.id && installed.firmware_version == record.version)
            .then(|| record.path.clone())
    }

    fn push_copy_state(&self) {
        let shown = self
            .ready_copy()
            .map(|p| p.display().to_string())
            .unwrap_or_default();

        tracing::info!(
            "gotowe dane na dysku: {}",
            if shown.is_empty() { "brak" } else { &shown }
        );
        self.with_ui(move |ui| ui.set_copy_source(shown.into()));
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
            self.set_status("Nie ma tam plików poprzedniego wydania.", false);
            return;
        }

        let bytes = stale.bytes;
        let count = stale::remove(&mount, &stale);

        self.set_status(
            format!(
                "Usunięto {count} plików poprzedniego wydania — odzyskane {}.",
                format_size(bytes)
            ),
            false,
        );
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
            Ok(()) => self.set_status("Pozostałości archiwów usunięte.", false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.set_status("Nie ma czego usuwać.", false)
            }
            Err(e) => self.set_status(format!("Nie udało się posprzątać: {e}"), true),
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
                format!(
                    "Pobrane pliki wymagają rozpakowania i nośnik wymienny musi pomieścić \
                     naraz archiwa i wynik ich rozpakowywania: ok. {}. Zapis na kość pamięci \
                     jest przez to podwojony. Używaj tylko, jeśli masz duży nośnik i nie masz \
                     dość wolnego miejsca na dysku twardym.",
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
                        format!(
                            "Pobrane archiwa ({}) trafią tymczasowo na dysk twardy, a na nośnik \
                             wymienny trafi tylko wynik ich rozpakowywania ({}).",
                            format_size(download),
                            format_size(extracted)
                        )
                    } else {
                        format!(
                            "Pobrane archiwa zostaną rozpakowane do wybranego katalogu \
                             (potrzeba {}). Po zakończeniu pobierania podłącz urządzenie \
                             przenośne i kliknij „Odśwież” celem skopiowania lub przeniesienia \
                             danych na podpięty nośnik.",
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
                    _ if m.mount_point.is_none() => ("niezamontowany".to_owned(), false),
                    _ if !m.is_exfat() => ("wymaga exFAT".to_owned(), false),
                    // Ta sama wersja: nudl pominie wszystkie pliki, więc
                    // pobieranie skończy się natychmiast i niczego nie zmieni.
                    _ if same_version => ("na nośniku jest już bieżąca wersja".to_owned(), true),
                    _ if needed > 0 && !m.fits(needed) => ("za mało miejsca".to_owned(), false),
                    // Prawie wszystkie pliki mają stałe ścieżki, więc nowe
                    // wydanie nadpisze je w miejscu; osierocone zostaje głównie
                    // archiwum map, bo jego nazwa niesie numer bazy. Auto go nie
                    // zainstaluje — czyta manifest — więc to kwestia miejsca,
                    // nie poprawności.
                    _ if other_version => (
                        "Na nośniku wymiennym znajduje się inna wersja oprogramowania \
                         i zostanie usunięta po pobraniu aktualnej"
                            .to_owned(),
                        true,
                    ),
                    _ => ("gotowy".to_owned(), true),
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

                let details = format!(
                    "{} · wolne {} z {}{}",
                    m.filesystem.as_deref().unwrap_or("brak systemu plików"),
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
                    label: m.label.as_str().into(),
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
        self.with_ui(|ui| ui.set_media_busy(true));

        match mapstick_media::list_media().await {
            Ok(media) => {
                let count = media.len();
                self.media = media;
                self.learn_installed_from_media();
                self.push_media_rows();

                if count == 0 {
                    self.set_status(
                        "Nie znaleziono nośnika wymiennego. Dyski wewnętrzne i systemowe są pomijane.",
                        false,
                    );
                } else {
                    self.set_status(format!("Znaleziono nośników: {count}."), false);

                    // Jeden podłączony nośnik to jedyny sensowny cel. Bez tego
                    // „Pobierz” zostaje wyszarzony, a jedyną podpowiedzią jest
                    // kliknięcie w wiersz, który wcale nie wygląda na klikalny.
                    self.adopt_single_medium();
                }
            }
            Err(e) => {
                self.media.clear();
                self.push_media_rows();
                self.set_status(format!("Nie udało się wykryć nośników: {e}"), true);
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
                "Ten nośnik nie jest zamontowany — podłącz go ponownie albo zamontuj.",
                true,
            );
            return;
        };

        let shown = mount.display().to_string();
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
            medium.label,
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
        self.set_status(format!("Formatowanie {} do exFAT…", medium.id), false);

        let result = mapstick_media::format_exfat(&medium, "MAPSTICK").await;

        self.with_ui(|ui| {
            ui.set_formatting(false);
            ui.set_format_dialog(false);
        });

        match result {
            Ok(()) => {
                self.set_status(format!("{} sformatowany do exFAT.", medium.id), false);
                self.refresh_media().await;
            }
            Err(e) => self.set_status(format!("Formatowanie nie powiodło się: {e}"), true),
        }
    }

    async fn pick_output_dir(&mut self) {
        let Some(dir) = rfd::AsyncFileDialog::new()
            .set_title("Katalog docelowy")
            .pick_folder()
            .await
        else {
            return;
        };

        let path = dir.path().to_path_buf();
        let shown = path.display().to_string();
        self.output_dir = Some(path);

        self.with_ui(move |ui| ui.set_output_dir(shown.into()));
        self.push_target_state();

        // Lista nośników jest już wykryta, więc jeśli wskazany katalog okazał
        // się gotowym źródłem, propozycja skopiowania ma się pokazać od razu —
        // bez naciskania „Odśwież”.
        self.adopt_single_medium();
    }

    /// Ustawia cel bez okna wyboru — pod testy na maszynie bez myszy.
    fn use_directory(&mut self, path: PathBuf) {
        let shown = path.display().to_string();
        self.output_dir = Some(path);

        self.with_ui(move |ui| ui.set_output_dir(shown.into()));
        self.push_target_state();
    }

    fn start_download(&mut self) -> Result<()> {
        let (Some(index), Some(firmware)) = (self.selected, self.firmware.clone()) else {
            return Err(anyhow!("Najpierw wybierz model i poczekaj na rozmiar."));
        };
        let output = self
            .output_dir
            .clone()
            .ok_or_else(|| anyhow!("Nie wybrano katalogu docelowego."))?;

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
            ui.set_progress(0.0);
            ui.set_progress_text("Rozpoczynanie…".into());
        });

        self.download = Some(tokio::spawn(async move {
            let handle = downloader.download();
            tokio::pin!(handle);

            let (mut dl_done, mut dl_total) = (0u64, 0u64);
            let (mut pp_done, mut pp_total) = (0u64, 0u64);
            let mut last_paint = Instant::now() - UI_REFRESH;

            let result = loop {
                tokio::select! {
                    r = &mut handle => break r,
                    msg = progress_rx.recv() => {
                        let Some(msg) = msg else { continue };

                        match msg {
                            ProgressMessage::TotalDownload(b) => dl_total = b,
                            ProgressMessage::TotalPostProcess(b) => pp_total = b,
                            ProgressMessage::Download(b) => dl_done += b,
                            ProgressMessage::PostProcess(b) => pp_done += b,
                        }

                        if last_paint.elapsed() < UI_REFRESH {
                            continue;
                        }
                        last_paint = Instant::now();

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
                        let text = format!("{} z {}", format_size(done), format_size(total));

                        let _ = ui.upgrade_in_event_loop(move |ui| {
                            ui.set_progress(fraction);
                            ui.set_progress_text(text.into());
                        });
                    }
                }
            };

            let succeeded = result.is_ok();

            let message = match result {
                Ok(()) => "Pobieranie zakończone. Sumy kontrolne sprawdzone.".to_owned(),
                Err(e) => format!("Pobieranie nie powiodło się: {e}"),
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
            anyhow!("Na dysku nie ma gotowego oprogramowania dla tego modelu.")
        })?;
        let target = self
            .output_dir
            .clone()
            .ok_or_else(|| anyhow!("Nie wybrano nośnika."))?;
        let index = self
            .selected
            .ok_or_else(|| anyhow!("Nie wybrano modelu."))?;

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
            return Err(anyhow!(
                "Na nośniku jest {} wolnego, a dane zajmują {}.",
                format_size(space.free),
                format_size(total)
            ));
        }

        let cancel = Arc::new(AtomicBool::new(false));
        self.copy_cancel = Some(cancel.clone());

        let delete_source = self.delete_after_copy;
        let ui = self.ui.clone();
        let tx = self.tx.clone();

        self.with_ui(|ui| {
            ui.set_copying(true);
            ui.set_progress(0.0);
            ui.set_progress_text("Kopiowanie…".into());
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
                        .is_some_and(|i| i.model_id == model_id && i.firmware_version == version);

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
                        None => ("Skopiowane na nośnik.".to_owned(), false),
                        Some(true) => (
                            "Skopiowane na nośnik, kopia z dysku usunięta.".to_owned(),
                            false,
                        ),
                        Some(false) => (
                            "Skopiowane na nośnik, ale kopii z dysku nie usunąłem — \
                             nie potwierdziłem zgodności. Sprawdź nośnik i usuń ją ręcznie."
                                .to_owned(),
                            true,
                        ),
                    }
                }
                Ok(Ok(CopyEnd::Cancelled)) => (
                    "Kopiowanie przerwane. Na nośniku nie ma kompletnych danych.".to_owned(),
                    true,
                ),
                Ok(Err(e)) => (format!("Kopiowanie nie powiodło się: {e}"), true),
                Err(e) => (format!("Kopiowanie przerwane: {e}"), true),
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
        // Kopiowanie jest blokujące — `abort()` by go nie zatrzymał, więc
        // podnosimy flagę i czekamy, aż zauważy ją sama pętla kopiująca.
        if let Some(flag) = self.copy_cancel.take() {
            flag.store(true, Ordering::Relaxed);
            self.set_status("Przerywam kopiowanie…", false);
            return;
        }

        if let Some(handle) = self.download.take() {
            handle.abort();
        }

        self.with_ui(|ui| {
            ui.set_downloading(false);
            ui.set_status("Pobieranie przerwane. Ponowne uruchomienie wznowi je.".into());
            ui.set_status_is_error(false);
        });
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
                self.set_status(format!("Nie udało się wczytać ulubionych: {e}"), true);
                Store::default()
            }
        };

        self.push_favourites();

        self.spawn_favourite_refresh();

        self.link_table = link::Table::load();
        tracing::info!("Tablica odnośników: {} wpisów", self.link_table.links.len());
        self.spawn_link_refresh(self.combos_in_use());

        if !restore {
            return;
        }

        let Some(last) = self.store.last_selection.clone() else {
            self.set_status(
                "Wybierz markę i region — lista modeli pobierze się sama.",
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
            self.set_status(format!("Nie udało się pobrać listy: {e}"), true);
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
            self.set_status("To nie jest odnośnik aktualizacji.", true);
            return;
        };

        if let Some(target) = self.link_table.find(&url).cloned() {
            self.open_target(target).await;
            return;
        }

        self.set_status("Rozpoznaję odnośnik ze strony producenta…", false);

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
            format!(
                "Nie rozpoznaję tego odnośnika. Sprawdziłem {} w regionach: {}. \
                 Jeśli auto jest z innego regionu, wybierz go z listy i kliknij ponownie.",
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
            self.set_status(format!("Nie udało się pobrać listy: {e}"), true);
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
            self.set_status(format!("Nie udało się pobrać listy: {e}"), true);
        }

        self.set_busy(false);
    }

    async fn run(mut self, mut rx: UnboundedReceiver<Cmd>) {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                Cmd::LoadModels { brand, region } => {
                    self.set_busy(true);
                    self.set_status("Pobieranie listy modeli…", false);

                    if let Err(e) = self.load_models(brand, region).await {
                        self.entries.clear();
                        self.push_rows();
                        self.set_status(format!("Nie udało się pobrać listy: {e}"), true);
                    }

                    self.set_busy(false);
                }
                Cmd::Filter(text) => {
                    self.filter = text;
                    self.push_rows();
                }
                Cmd::Select(i) => {
                    if let Err(e) = self.select(i).await {
                        self.with_ui(|ui| ui.set_detail_size("rozmiar nieznany".into()));
                        self.set_status(format!("Nie udało się pobrać informacji: {e}"), true);
                    }
                }
                Cmd::PickOutputDir => self.pick_output_dir().await,
                Cmd::UseDirectory(path) => self.use_directory(path),
                Cmd::StartDownload => {
                    if let Err(e) = self.start_download() {
                        self.set_status(e.to_string(), true);
                    }
                }
                Cmd::Cancel => self.cancel(),
                Cmd::SetDirectMode(direct) => {
                    self.direct_to_medium = direct;
                    self.push_space_note();
                    self.push_media_rows();
                }
                Cmd::ClearCache => self.clear_cache(),
                Cmd::Startup { restore } => self.startup(restore).await,
                Cmd::ToggleFavourite => self.toggle_favourite(),
                Cmd::OpenFavourite(id) => self.open_favourite(id).await,
                Cmd::FavouritesRefreshed(versions) => self.apply_favourite_versions(versions),
                Cmd::OpenLink(url) => self.open_link(url).await,
                Cmd::LinksCollected(found) => self.apply_links(found),
                Cmd::DownloadFinished(record) => self.record_download(*record),
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

                    self.push_target_state();
                }
                Cmd::GoToMedia => {
                    let summary = match (&self.selected, self.required_bytes()) {
                        (Some(i), needed) if needed > 0 => format!(
                            "{} — do zapisania {}",
                            self.entries[*i].car.name,
                            format_size(needed)
                        ),
                        (Some(i), _) => format!("{} — rozmiar nieznany", self.entries[*i].car.name),
                        _ => "Nie wybrano modelu".to_owned(),
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
            }
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("Nie udało się ustawić dostawcy kryptografii rustls"))?;

    // Uruchomienie z odnośnikiem ze strony producenta. Jeśli okno już stoi
    // otwarte, oddajemy mu odnośnik i kończymy — drugie okno nikomu nie służy.
    let link = std::env::args().nth(1).filter(|a| link::is_link(a));

    if let Some(url) = &link
        && instance::hand_off(url)
    {
        return Ok(());
    }

    let ui = AppWindow::new()?;

    let regions: Vec<SharedString> = REGIONS.iter().map(|(_, label)| (*label).into()).collect();
    ui.set_regions(ModelRc::new(VecModel::from(regions)));
    ui.set_models(ModelRc::new(VecModel::<ModelRow>::default()));
    ui.set_status("Wybierz markę i region — lista modeli pobierze się sama.".into());

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
        move |url| {
            let _ = t.send(Cmd::OpenLink(url));
        }
    });

    // Pomoc dla testów na maszynie bez sterowania myszą: MAPSTICK_DEV=lista
    // wczytuje modele, MAPSTICK_DEV=nosnik dodatkowo wchodzi w widok nośnika.
    let _ = tx.send(Cmd::Startup {
        restore: link.is_none(),
    });

    if let Some(url) = link {
        let _ = tx.send(Cmd::OpenLink(url));
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

            if let Ok(mode) = std::env::var("MAPSTICK_DEV_KOPIUJ").as_deref() {
                let _ = tx.send(Cmd::SetDeleteAfterCopy(mode == "przenies"));
                let _ = tx.send(Cmd::StartCopy);
            }
        }
    }

    wire_callbacks(&ui, tx);

    ui.run()?;
    instance::cleanup();

    Ok(())
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
    ui.on_pick_output_dir(move || {
        let _ = t.send(Cmd::PickOutputDir);
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
