// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! Sprawdzanie nowych wydań bez otwierania okna.
//!
//! Uruchamiane z harmonogramu systemu (`mapstick --sprawdz`), robi jedną
//! rzecz i kończy się: pyta o modele oznaczone jako pilnowane, zapisuje wynik
//! i pokazuje powiadomienie, jeśli jest o czym mówić. Między sprawdzeniami nie
//! zostaje w pamięci nic — to najlżejsza usługa, jaka istnieje.
//!
//! Dwa rytmy, jeden proces. System budzi nas **codziennie**, ale producenta
//! pytamy wedle odstępu z ustawień (domyślnie raz w tygodniu) — reszta budzeń
//! to sam rzut oka na plik. Za to o wydaniu, którego auto jeszcze nie ma,
//! przypominamy **co dzień**, aż ktoś skopiuje je na nośnik albo zaznaczy,
//! że auto już je ma. Jedno powiadomienie w gąszczu innych ginie; codzienne
//! nie zginie, a kończy się dokładnie wtedy, gdy przestaje być potrzebne.
//!
//! Oba odstępy pilnujemy sami, z dat w pliku, zamiast ufać wyzwalaczowi:
//! timer systemd, wpis w cronie i uruchomienie przy logowaniu wołają nas
//! z różną regularnością, a logowanie potrafi zawołać trzy razy dziennie.

use std::collections::HashSet;
#[cfg(target_os = "linux")]
use std::time::Duration;

use anyhow::{Context, Result};
use jiff::{Timestamp, ToSpan};
use mapstick_store::{Favourite, Interval, Store};
#[cfg(target_os = "linux")]
use mapstick_store::InstalledSource;

use crate::{BRANDS, brand_index, fetch_entries, tr};

/// Ile czasu dajemy powiadomieniu na przyciski, zanim uznamy, że nikt ich
/// nie naciśnie. Bez tego proces potrafiłby stać do końca świata przy
/// pulpicie, który powiadomień nie zamyka sam.
///
/// Przyciski w powiadomieniu ma na razie tylko Linux — `notify-rust` opiera
/// je na specyfikacji D-Bus, której Windows i macOS nie mają.
#[cfg(target_os = "linux")]
const ACTION_WAIT: Duration = Duration::from_secs(30);

/// O czym trzeba powiedzieć użytkownikowi.
#[derive(Debug, PartialEq, Eq)]
struct News {
    model_id: String,
    label: String,
    version: String,
    released: Option<String>,
    /// Wersja w aucie, o ile w ogóle ją znamy.
    known: Option<String>,
}

impl News {
    fn summary(&self) -> String {
        tr!("{}: new map release", self.label)
    }

    fn body(&self) -> String {
        let released = self
            .released
            .as_deref()
            .map(|r| format!("{r} · "))
            .unwrap_or_default();

        match &self.known {
            Some(known) => tr!(
                "{}{}\nThe car has {}. I will remind you tomorrow, unless you copy the new \
                 files onto the medium or mark the car as having it.",
                released,
                self.version,
                known
            ),
            // Bez wiedzy o aucie powiadomienie i tak ma sens — trzeba tylko
            // powiedzieć wprost, czego nie wiemy, i o co prosimy.
            None => tr!(
                "{}{}\nI don’t know what version is in the car — mark it, so I can tell \
                 whether this is an update. Until then, I will remind you every day.",
                released,
                self.version
            ),
        }
    }
}

/// Jedno sprawdzenie. `force` pomija oba odstępy — do ręcznego wywołania.
///
/// Pytanie do serwisu jest asynchroniczne, powiadomienie nie: `notify-rust`
/// czeka na odpowiedź magistrali po swojemu i wewnątrz środowiska tokio kończy
/// się to „Cannot start a runtime from within a runtime”. Środowisko żyje więc
/// tylko tak długo, jak trwa pytanie.
pub fn run(force: bool) -> Result<()> {
    let news = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(gather(force))?;

    for item in news {
        if let Err(e) = announce(&item) {
            tracing::warn!("Nie udało się pokazać powiadomienia: {e}");
        }
    }

    Ok(())
}

async fn gather(force: bool) -> Result<Vec<News>> {
    let path = mapstick_store::default_path();

    // Blokada obejmuje cały cykl odczyt-zmiana-zapis: w tym czasie otwarte
    // okno nie podmieni pliku pod nami.
    let _lock = mapstick_store::lock(&path).with_context(|| tr!("Could not lock the favourites file"))?;
    let mut store = Store::load(&path)?;

    if store.watched().next().is_none() {
        tracing::info!("Żaden model nie jest pilnowany — nie ma czego sprawdzać");
        return Ok(Vec::new());
    }

    if force || due(&store, store.check_interval) {
        check(&mut store).await;
    } else {
        tracing::info!("Producenta pytano niedawno — tylko przypomnienia");
    }

    let news = reminders(&mut store, force, Timestamp::now());

    // Zapis idzie przed powiadomieniami: to, czego się dowiedzieliśmy, ma
    // przetrwać nawet wtedy, gdy powiadomienia nie ma komu pokazać.
    store.save(&path)?;

    Ok(news)
}

/// Czy od ostatniego pytania do producenta minęło dość czasu.
///
/// Patrzymy na najświeższe sprawdzenie wśród pilnowanych modeli — także to
/// zrobione przez samą aplikację przy uruchomieniu. Jeśli okno było otwarte
/// godzinę temu, użytkownik widział to samo, co mielibyśmy mu powiedzieć.
fn due(store: &Store, interval: Interval) -> bool {
    let newest = store
        .watched()
        .filter_map(|f| f.latest_checked_at.as_deref())
        .filter_map(|t| t.parse::<Timestamp>().ok())
        .max();

    let Some(newest) = newest else {
        return true;
    };

    // W godzinach, nie w dniach: znacznik czasu jest punktem na osi, a nie
    // datą w kalendarzu, i jiff słusznie nie pozwala dodawać do niego dni.
    let span = match interval {
        Interval::Daily => 24.hours(),
        Interval::Weekly => (7 * 24).hours(),
        Interval::Monthly => (30 * 24).hours(),
    };

    // Margines, żeby sprawdzenie o tej samej porze dnia nie wypadało raz na
    // dwa razy przez kilka sekund poślizgu harmonogramu.
    newest + span - 1.hour() <= Timestamp::now()
}

/// Odpytuje serwis i zapisuje, co zastał.
async fn check(store: &mut Store) {
    let mut combos: Vec<(String, String)> = store
        .watched()
        .map(|f| (f.brand.clone(), f.region.clone()))
        .collect();
    combos.sort();
    combos.dedup();

    let watched: HashSet<String> = store.watched().map(|f| f.model_id.clone()).collect();

    for (brand, region) in combos {
        let code = BRANDS[brand_index(&brand)].as_code_str();

        let entries = match fetch_entries(code, &region).await {
            Ok(entries) => entries,
            // Brak sieci o czwartej nad ranem to nie jest powód do alarmu.
            // Następne sprawdzenie zastanie ten sam stan i zapyta jeszcze raz.
            Err(e) => {
                tracing::warn!("Nie udało się sprawdzić {brand}/{region}: {e:#}");
                continue;
            }
        };

        for entry in entries {
            if !watched.contains(&entry.car.id) {
                continue;
            }

            let Some(favourite) = store.get_mut(&entry.car.id) else {
                continue;
            };

            // Model bywa wystawiony w dwóch odmianach głowicy naraz; auto ma
            // jedną z nich i tylko ona świadczy o tym, czy coś się zmieniło.
            let Some(version) = favourite.matching(&entry.car.versions).cloned() else {
                continue;
            };

            favourite.latest_seen_version = Some(version);
            favourite.latest_checked_at = Some(mapstick_store::now());
            if !entry.released.is_empty() {
                favourite.latest_released = Some(entry.released.clone());
            }
        }
    }
}

/// Wydania, o których trzeba dziś przypomnieć. Zapisuje, że przypomniano.
fn reminders(store: &mut Store, force: bool, now: Timestamp) -> Vec<News> {
    let mut news = Vec::new();

    for favourite in store.items.iter_mut().filter(|f| f.watch) {
        let Some(item) = worth_saying(favourite, force, now) else {
            continue;
        };

        favourite.notified_version = Some(item.version.clone());
        favourite.notified_at = Some(now.to_string());
        news.push(item);
    }

    news
}

/// Czy dziś należy się przypomnienie o wydaniu wystawionym przez producenta.
///
/// Sprawa jest otwarta, dopóki auto nie ma tego numeru — czy to dlatego, że
/// ma starszy, czy dlatego, że nie wiemy, co ma. Zamyka ją zapis na nośnik
/// (aplikacja odczytuje wtedy numer z pliku `.ver`) albo ręczne zaznaczenie.
/// Otwarta sprawa przypomina o sobie raz na dobę.
fn worth_saying(favourite: &Favourite, force: bool, now: Timestamp) -> Option<News> {
    let version = favourite.latest_seen_version.as_deref()?;

    // Auto ma to, co wystawia producent — nie ma o czym mówić.
    if favourite.known_version() == Some(version) {
        return None;
    }

    // Nowszy numer niż ten, o którym mówiliśmy, zasługuje na głos od razu.
    // Ten sam — dopiero po dobie.
    let same = favourite.notified_version.as_deref() == Some(version);
    let recent = favourite
        .notified_at
        .as_deref()
        .and_then(|t| t.parse::<Timestamp>().ok())
        .is_some_and(|t| t + 24.hours() - 1.hour() > now);

    if same && recent && !force {
        return None;
    }

    Some(News {
        model_id: favourite.model_id.clone(),
        label: favourite.label.clone(),
        version: version.to_owned(),
        released: favourite.latest_released.clone(),
        known: favourite.known_version().map(str::to_owned),
    })
}

/// Zapisuje, że auto ma już to wydanie — z przycisku w powiadomieniu.
///
/// Osobny proces, osobna blokada: okno aplikacji może być w tym czasie otwarte
/// i sięgać po ten sam plik. Wołane tylko z linuksowego `show` — gdzie indziej
/// powiadomienie nie ma przycisków, którymi dałoby się to wywołać.
#[cfg(target_os = "linux")]
fn mark_installed(model_id: &str, version: &str) -> Result<()> {
    let path = mapstick_store::default_path();
    let _lock = mapstick_store::lock(&path)?;
    let mut store = Store::load(&path)?;

    let Some(favourite) = store.get_mut(model_id) else {
        return Ok(());
    };

    favourite.installed_version = Some(version.to_owned());
    favourite.installed_source = Some(InstalledSource::Manual);
    favourite.installed_confirmed = true;

    store.save(&path)?;
    tracing::info!("{model_id}: zaznaczone, że auto ma {version}");

    Ok(())
}

/// Pokazuje powiadomienie systemowe.
fn announce(news: &News) -> Result<()> {
    let mut notification = notify_rust::Notification::new();
    notification
        .summary(&news.summary())
        .body(&news.body())
        .appname("MapStick")
        .timeout(notify_rust::Timeout::Milliseconds(20_000));

    show(notification, news)
}

#[cfg(target_os = "linux")]
fn show(mut notification: notify_rust::Notification, news: &News) -> Result<()> {
    let handle = notification
        .action("pokaz", &tr!("Show"))
        .action("mam", &tr!("The car already has it"))
        .show()?;

    // Czekanie na kliknięcie odbywa się w osobnym wątku, żeby pulpit, który
    // powiadomień nie zamyka, nie zatrzymał nas na zawsze.
    let (tx, rx) = std::sync::mpsc::channel();
    let (model_id, version) = (news.model_id.clone(), news.version.clone());

    std::thread::spawn(move || {
        handle.wait_for_action(|action| match action {
            "pokaz" => open_app(&model_id),
            "mam" => {
                if let Err(e) = mark_installed(&model_id, &version) {
                    tracing::warn!("Nie udało się zapisać wersji w aucie: {e:#}");
                }
            }
            _ => {}
        });
        let _ = tx.send(());
    });

    let _ = rx.recv_timeout(ACTION_WAIT);

    Ok(())
}

/// Identyfikator aplikacji (AppUserModelID), pod którym Windows pokazuje
/// powiadomienia. Bez niego notify-rust podpisuje je „Windows PowerShell”.
#[cfg(target_os = "windows")]
const APP_ID: &str = "MapStick";

/// Windows przyjmuje powiadomienie od aplikacji spoza Sklepu tylko z
/// identyfikatorem, który zna — wpis w rejestrze użytkownika wystarczy
/// i nie wymaga instalatora ani uprawnień administratora. Powtarzane przy
/// każdym powiadomieniu, bo kosztuje ułamek sekundy, a przenosi się razem
/// z aplikacją.
#[cfg(target_os = "windows")]
fn register_app_id() {
    use std::os::windows::process::CommandExt;

    let key = format!(r"HKCU\Software\Classes\AppUserModelId\{APP_ID}");
    let status = std::process::Command::new("reg")
        .args([
            "add",
            &key,
            "/v",
            "DisplayName",
            "/t",
            "REG_SZ",
            "/d",
            "MapStick",
            "/f",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(crate::schedule::CREATE_NO_WINDOW)
        .status();

    if !matches!(status, Ok(s) if s.success()) {
        tracing::warn!("Nie udało się zarejestrować {APP_ID} dla powiadomień: {status:?}");
    }
}

#[cfg(target_os = "windows")]
fn show(mut notification: notify_rust::Notification, _news: &News) -> Result<()> {
    register_app_id();
    notification.app_id(APP_ID);
    notification.show()?;
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn show(notification: notify_rust::Notification, _news: &News) -> Result<()> {
    notification.show()?;
    Ok(())
}

/// Uruchamia aplikację na wskazanym modelu.
#[cfg(target_os = "linux")]
fn open_app(model_id: &str) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };

    use std::os::unix::process::CommandExt;

    // Własna grupa procesów i cisza na wyjściu: okno ma przeżyć koniec
    // sprawdzania (systemd sprząta całą grupę, gdy usługa się kończy)
    // i nie ma mieszać swoich meldunków do dziennika sprawdzania.
    match std::process::Command::new(exe)
        .arg(format!("{}{model_id}", crate::instance::MODEL_PREFIX))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
    {
        Ok(_) => tracing::info!("Otwieram {model_id}"),
        Err(e) => tracing::warn!("Nie udało się otworzyć aplikacji: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn favourite() -> Favourite {
        Favourite {
            label: "New IONIQ 5".to_owned(),
            brand: "hyundai".to_owned(),
            region: "EU".to_owned(),
            model_id: "New_IONIQ5_EUR".to_owned(),
            model_name: "IONIQ 5".to_owned(),
            latest_seen_version: Some("NE1.002".to_owned()),
            latest_checked_at: None,
            latest_released: Some("2026.07".to_owned()),
            downloaded_version: None,
            downloaded_at: None,
            installed_version: None,
            installed_source: None,
            installed_confirmed: false,
            vin: None,
            watch: true,
            notified_version: None,
            notified_at: None,
        }
    }

    fn now() -> Timestamp {
        Timestamp::now()
    }

    /// Nie wiemy, co w aucie — mówimy o wydaniu i prosimy o zaznaczenie wersji.
    #[test]
    fn an_unknown_car_still_gets_told() {
        let news = worth_saying(&favourite(), false, now()).unwrap();

        assert_eq!(news.known, None);
        assert!(news.body().contains("I don’t know what version is in the car"));
        assert!(news.body().contains("2026.07"));
    }

    #[test]
    fn a_car_with_that_version_is_left_in_peace() {
        let mut f = favourite();
        f.installed_version = Some("NE1.002".to_owned());

        assert_eq!(worth_saying(&f, false, now()), None);
    }

    #[test]
    fn an_older_car_hears_what_it_has_now() {
        let mut f = favourite();
        f.installed_version = Some("NE1.001".to_owned());

        let news = worth_saying(&f, false, now()).unwrap();
        assert!(news.body().contains("The car has NE1.001."));
    }

    /// Sedno: przypomnienie wraca po dobie, nie po godzinie.
    #[test]
    fn the_same_release_is_repeated_daily_but_not_hourly() {
        let mut f = favourite();
        f.notified_version = Some("NE1.002".to_owned());

        f.notified_at = Some((now() - 2.hours()).to_string());
        assert_eq!(worth_saying(&f, false, now()), None, "dwie godziny to za wcześnie");

        f.notified_at = Some((now() - 25.hours()).to_string());
        assert!(worth_saying(&f, false, now()).is_some(), "po dobie należy się");
    }

    /// Nowszy numer nie czeka na dobę — to inna wiadomość.
    #[test]
    fn a_newer_release_is_announced_at_once() {
        let mut f = favourite();
        f.notified_version = Some("NE1.001".to_owned());
        f.notified_at = Some(now().to_string());

        assert!(worth_saying(&f, false, now()).is_some());
    }

    /// Koniec przypominania: auto ma wersję — czy to z nośnika, czy z ręki.
    #[test]
    fn reminders_stop_once_the_car_has_it() {
        let mut f = favourite();
        f.notified_version = Some("NE1.002".to_owned());
        f.notified_at = Some((now() - 48.hours()).to_string());
        f.installed_version = Some("NE1.002".to_owned());

        assert_eq!(worth_saying(&f, false, now()), None);
    }

    /// Wymuszone sprawdzenie mówi od razu, choćby mówiło przed chwilą.
    #[test]
    fn a_forced_run_repeats_regardless() {
        let mut f = favourite();
        f.notified_version = Some("NE1.002".to_owned());
        f.notified_at = Some(now().to_string());

        assert!(worth_saying(&f, true, now()).is_some());
    }

    #[test]
    fn reminders_are_written_down_when_given() {
        let mut store = Store::default();
        store.upsert(favourite());

        let news = reminders(&mut store, false, now());
        assert_eq!(news.len(), 1);

        let f = store.find("New_IONIQ5_EUR").unwrap();
        assert_eq!(f.notified_version.as_deref(), Some("NE1.002"));
        assert!(f.notified_at.is_some());

        // Druga runda tego samego dnia milczy.
        assert!(reminders(&mut store, false, now()).is_empty());
    }

    #[test]
    fn a_store_nobody_checked_yet_is_due() {
        let mut store = Store::default();
        store.upsert(favourite());

        assert!(due(&store, Interval::Weekly));
    }

    #[test]
    fn a_fresh_check_holds_the_next_one_off() {
        let mut store = Store::default();
        let mut f = favourite();
        f.latest_checked_at = Some(mapstick_store::now());
        store.upsert(f);

        assert!(!due(&store, Interval::Weekly));
        assert!(!due(&store, Interval::Daily));
    }

    #[test]
    fn a_check_from_last_month_is_due_again() {
        let mut store = Store::default();
        let mut f = favourite();
        f.latest_checked_at = Some((Timestamp::now() - (31 * 24).hours()).to_string());
        store.upsert(f);

        assert!(due(&store, Interval::Monthly));
    }

    /// Modele bez pilnowania nie wpływają na odstęp — także wtedy, gdy
    /// aplikacja odświeżyła je przed chwilą.
    #[test]
    fn unwatched_models_do_not_hold_the_check_off() {
        let mut store = Store::default();
        let mut watched = favourite();
        watched.latest_checked_at = Some((Timestamp::now() - (31 * 24).hours()).to_string());
        store.upsert(watched);

        let mut other = favourite();
        other.model_id = "Bayon_EU".to_owned();
        other.watch = false;
        other.latest_checked_at = Some(mapstick_store::now());
        store.upsert(other);

        assert!(due(&store, Interval::Weekly));
    }
}
