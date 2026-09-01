// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! MapStick — graficzna nakładka na nudl.

use std::{path::PathBuf, time::Duration};

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
    fn required_bytes(&self) -> u64 {
        self.firmware
            .as_ref()
            .map(|f| firmware_totals(f).1)
            .unwrap_or(0)
    }

    fn push_media_rows(&self) {
        let needed = self.required_bytes();

        let rows: Vec<MediumRow> = self
            .media
            .iter()
            .map(|m| {
                let (status, usable) = match () {
                    _ if m.mount_point.is_none() => ("niezamontowany", false),
                    _ if !m.is_exfat() => ("wymaga exFAT", false),
                    _ if needed > 0 && !m.fits(needed) => ("za mało miejsca", false),
                    _ => ("gotowy", true),
                };

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

                // Plik .ver mówi, co użytkownik ostatnio wgrywał tym nośnikiem
                // — czyli najpewniejsze źródło wersji siedzącej w aucie.
                let installed = m
                    .mount_point
                    .as_deref()
                    .and_then(|p| mapstick_media::read_installed(p).ok().flatten())
                    .map(|i| format!("{} {}", i.model_id, i.firmware_version))
                    .unwrap_or_default();

                MediumRow {
                    id: m.id.as_str().into(),
                    label: m.label.as_str().into(),
                    details: details.into(),
                    status: status.into(),
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
        let directory = Dir::open_ambient_dir(&output, authority)?;

        let ui = self.ui.clone();
        let client = NuClientBuilder::new().build()?;

        let (downloader, mut progress_rx) = Downloader::new(
            directory,
            client,
            car,
            firmware,
            CONCURRENCY,
            RETRIES,
            false,
        );

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

            let message = match result {
                Ok(()) => "Pobieranie zakończone. Sumy kontrolne sprawdzone.".to_owned(),
                Err(e) => format!("Pobieranie nie powiodło się: {e}"),
            };
            let is_error = message.starts_with("Pobieranie nie");

            let _ = ui.upgrade_in_event_loop(move |ui| {
                ui.set_downloading(false);
                ui.set_status(message.into());
                ui.set_status_is_error(is_error);
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
        let _ = tx.send(Cmd::Select(1));

        if stage == "nosnik" {
            let _ = tx.send(Cmd::GoToMedia);
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

    ui.on_cancel_format(move || {
        let _ = tx.send(Cmd::CancelFormat);
    });
}
