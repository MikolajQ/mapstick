// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! MapStick — graficzna nakładka na nudl.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Result, anyhow};
use cap_std::{ambient_authority, fs::Dir};
use mapstick_media::Medium;
use nudl::{
    cli::Brand,
    client::{CarInfo, FirmwareInfo, NuClient, NuClientBuilder},
    download::{Downloader, ProgressMessage},
};
use slint::{ModelRc, SharedString, VecModel, Weak};
use tokio::{
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
    time::Instant,
};

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

const CONCURRENCY: usize = 4;
const RETRIES: u8 = 3;
const UI_REFRESH: Duration = Duration::from_millis(100);

/// Polecenia płynące z wątku interfejsu do zadania sieciowego.
#[derive(Debug)]
enum Cmd {
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
    ConfirmFormat,
    CancelFormat,
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

/// Katalog na archiwa pobierane przed rozpakowaniem.
///
/// Nie leży na nośniku, bo o to właśnie chodzi: nośnik ma przyjąć wyłącznie
/// rozpakowany wynik.
fn work_directory() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);

    base.join("mapstick").join("archiwa")
}

/// Sumaryczny rozmiar katalogu, licząc podkatalogi.
fn directory_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };

    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => directory_size(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
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

/// Stan zadania sieciowego. Żyje w całości poza wątkiem interfejsu.
struct Worker {
    ui: Weak<AppWindow>,
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
}

impl Worker {
    fn new(ui: Weak<AppWindow>) -> Self {
        Self {
            ui,
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

    async fn client(&self) -> Result<NuClient> {
        Ok(NuClientBuilder::new().build()?)
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
        let client = self.client().await?;

        client.validate_region(code, &self.region).await?;
        let guid = client.get_guid(&self.region).await?;
        let raw = client.get_cars_raw(&self.region, &guid, code).await?;

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

        let count = entries.len();
        self.entries = entries;
        self.filter.clear();
        self.push_rows();

        self.set_status(format!("Znaleziono {count} modeli."), false);
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
        let client = self.client().await?;
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
    fn push_target_state(&self) {
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
        self.with_ui(move |ui| ui.set_target_has_current(current));
    }

    /// Ile miejsca zajmują archiwa pozostałe po wcześniejszych pobraniach.
    fn push_leftovers(&self) {
        let size = directory_size(&work_directory());
        let text = if size > 0 {
            format_size(size)
        } else {
            String::new()
        };

        tracing::info!("pozostałości archiwów: {} B", size);
        self.with_ui(move |ui| ui.set_leftovers(text.into()));
    }

    fn clear_cache(&self) {
        let work = work_directory();

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
                    "Nośnik musi pomieścić naraz archiwa i wynik ich rozpakowywania: ok. {}. \
                     Zapis na kość pamięci jest przez to podwojony. Używaj, jeśli masz duży \
                     nośnik i nie masz miejsca na dysku twardym.",
                    format_size(download + extracted)
                ),
                true,
            )
        } else {
            let work = work_directory();
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
                format!(
                    "Archiwa ({}) trafią na dysk, na nośnik idzie tylko wynik ({}).",
                    format_size(download),
                    format_size(extracted)
                ),
                !enough,
            )
        };

        self.with_ui(move |ui| {
            ui.set_space_note(note.into());
            ui.set_space_warning(warning);
        });
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
                    // Nazwy plików niosą numer wersji, więc stary komplet nie
                    // zostanie nadpisany ani usunięty — zostanie obok nowego.
                    _ if other_version => ("inna wersja — zalecane formatowanie".to_owned(), false),
                    _ => ("gotowy".to_owned(), true),
                };

                let installed = found
                    .map(|i| format!("{} {}", i.model_id, i.firmware_version))
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
                    installed: installed.into(),
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
                self.push_media_rows();

                if count == 0 {
                    self.set_status(
                        "Nie znaleziono nośnika wymiennego. Dyski wewnętrzne i systemowe są pomijane.",
                        false,
                    );
                } else {
                    self.set_status(format!("Znaleziono nośników: {count}."), false);
                }
            }
            Err(e) => {
                self.media.clear();
                self.push_media_rows();
                self.set_status(format!("Nie udało się wykryć nośników: {e}"), true);
            }
        }

        self.with_ui(|ui| ui.set_media_busy(false));
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
        let client = NuClientBuilder::new().build()?;

        // Domyślnie archiwa lądują na dysku, a na nośnik trafia sam wynik.
        // Zapisów na kość pamięci jest wtedy o połowę mniej, a rozpakowywanie
        // nakłada się w czasie z pobieraniem kolejnych archiwów.
        // Po udanym pobraniu archiwa nie są już do niczego potrzebne.
        let cleanup = (!self.direct_to_medium).then(work_directory);

        let (downloader, mut progress_rx) = if self.direct_to_medium {
            Downloader::new(out_dir, client, car, firmware, CONCURRENCY, RETRIES, false)
        } else {
            let work = work_directory();
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
                    let size = directory_size(work);
                    if size > 0 {
                        leftovers = format_size(size);
                    }
                }
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

    /// Przerwanie polega na porzuceniu przyszłości pobierania — nudl zapisuje
    /// postęp na bieżąco, więc wznowienie sprowadza się do ponownego startu.
    fn cancel(&mut self) {
        if let Some(handle) = self.download.take() {
            handle.abort();
        }

        self.with_ui(|ui| {
            ui.set_downloading(false);
            ui.set_status("Pobieranie przerwane. Ponowne uruchomienie wznowi je.".into());
            ui.set_status_is_error(false);
        });
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

    let ui = AppWindow::new()?;

    let regions: Vec<SharedString> = REGIONS.iter().map(|(_, label)| (*label).into()).collect();
    ui.set_regions(ModelRc::new(VecModel::from(regions)));
    ui.set_models(ModelRc::new(VecModel::<ModelRow>::default()));
    ui.set_status("Wybierz markę i region, a potem naciśnij „Pokaż modele”.".into());

    let (tx, rx) = mpsc::unbounded_channel();

    let weak = ui.as_weak();
    std::thread::Builder::new()
        .name("mapstick-net".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("Nie udało się uruchomić środowiska tokio");

            runtime.block_on(Worker::new(weak).run(rx));
        })?;

    // Pomoc dla testów na maszynie bez sterowania myszą: MAPSTICK_DEV=lista
    // wczytuje modele, MAPSTICK_DEV=nosnik dodatkowo wchodzi w widok nośnika.
    if let Ok(stage @ ("lista" | "nosnik")) = std::env::var("MAPSTICK_DEV").as_deref() {
        let _ = tx.send(Cmd::LoadModels {
            brand: 0,
            region: 0,
        });

        if let Ok(filter) = std::env::var("MAPSTICK_DEV_MODEL") {
            let _ = tx.send(Cmd::Filter(filter));
        }

        let _ = tx.send(Cmd::Select(0));

        if std::env::var_os("MAPSTICK_DEV_DIRECT").is_some() {
            let _ = tx.send(Cmd::SetDirectMode(true));
        }

        if stage == "nosnik" {
            let _ = tx.send(Cmd::GoToMedia);
            // Polecenia są przetwarzane po kolei, a GoToMedia czeka na
            // wykrycie nośników, więc lista jest już gotowa.
            let _ = tx.send(Cmd::SelectMedium(0));
        }
    }

    wire_callbacks(&ui, tx);

    ui.run()?;
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

    ui.on_clear_cache(move || {
        let _ = tx.send(Cmd::ClearCache);
    });
}
