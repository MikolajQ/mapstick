// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! Sprawdzanie nowych wydań bez otwierania okna.
//!
//! Uruchamiane z harmonogramu systemu (`mapstick --sprawdz`), robi jedną
//! rzecz i kończy się: pyta o modele oznaczone jako pilnowane, zapisuje wynik
//! i pokazuje powiadomienie, jeśli jest o czym mówić. Między sprawdzeniami nie
//! zostaje w pamięci nic — to najlżejsza usługa, jaka istnieje.
//!
//! Odstęp pilnujemy sami, zamiast ufać wyzwalaczowi. Timer systemd, wpis
//! w cronie i uruchomienie przy logowaniu wołają nas z różną regularnością,
//! a przy trzech oknach terminala dziennie logowanie potrafi zawołać nas trzy
//! razy. Data ostatniego sprawdzenia rozstrzyga to bez względu na to, kto
//! nacisnął spust.

use std::{collections::HashSet, time::Duration};

use anyhow::{Context, Result};
use jiff::{Timestamp, ToSpan};
use mapstick_store::{Favourite, Interval, Store};

use crate::{BRANDS, brand_index, fetch_entries};

/// Ile czasu dajemy powiadomieniu na przycisk „Pokaż”, zanim uznamy, że nikt
/// go nie naciśnie. Bez tego proces potrafiłby stać do końca świata przy
/// pulpicie, który powiadomień nie zamyka sam.
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
        format!("{}: nowe wydanie map", self.label)
    }

    fn body(&self) -> String {
        let released = self
            .released
            .as_deref()
            .map(|r| format!("{r} · "))
            .unwrap_or_default();

        match &self.known {
            Some(known) => format!(
                "{released}{}\nW aucie masz {known}.",
                self.version
            ),
            // Bez wiedzy o aucie powiadomienie i tak ma sens — trzeba tylko
            // powiedzieć wprost, czego nie wiemy, i o co prosimy.
            None => format!(
                "{released}{}\nNie wiem, jaka wersja jest w aucie — zaznacz ją w MapSticku, \
                 żeby wiedzieć, czy to aktualizacja.",
                self.version
            ),
        }
    }
}

/// Jedno sprawdzenie. `force` pomija odstęp — do ręcznego wywołania.
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
    let _lock = mapstick_store::lock(&path).context("Nie udało się zablokować pliku ulubionych")?;
    let mut store = Store::load(&path)?;

    if store.watched().next().is_none() {
        tracing::info!("Żaden model nie jest pilnowany — nie ma czego sprawdzać");
        return Ok(Vec::new());
    }

    if !force && !due(&store, store.check_interval) {
        tracing::info!("Sprawdzano niedawno — odpuszczam");
        return Ok(Vec::new());
    }

    let news = check(&mut store).await;

    // Zapis idzie przed powiadomieniami: to, czego się dowiedzieliśmy, ma
    // przetrwać nawet wtedy, gdy powiadomienia nie ma komu pokazać.
    store.save(&path)?;

    Ok(news)
}

/// Czy od ostatniego sprawdzenia minęło dość czasu.
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

/// Odpytuje serwis i zapisuje, co zastał. Zwraca to, o czym warto powiedzieć.
async fn check(store: &mut Store) -> Vec<News> {
    let mut combos: Vec<(String, String)> = store
        .watched()
        .map(|f| (f.brand.clone(), f.region.clone()))
        .collect();
    combos.sort();
    combos.dedup();

    let watched: HashSet<String> = store.watched().map(|f| f.model_id.clone()).collect();
    let mut news = Vec::new();

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
            let Some(version) = entry.car.versions.into_iter().next() else {
                continue;
            };

            favourite.latest_seen_version = Some(version.clone());
            favourite.latest_checked_at = Some(mapstick_store::now());
            if !entry.released.is_empty() {
                favourite.latest_released = Some(entry.released.clone());
            }

            if let Some(item) = worth_saying(favourite, &version) {
                favourite.notified_version = Some(version);
                news.push(item);
            }
        }
    }

    news
}

/// Czy to wydanie zasługuje na powiadomienie.
fn worth_saying(favourite: &Favourite, version: &str) -> Option<News> {
    // O tym wydaniu już mówiliśmy. Powtarzanie co tydzień tego samego uczy
    // ludzi wyłączać powiadomienia.
    if favourite.notified_version.as_deref() == Some(version) {
        return None;
    }

    // Auto ma to, co wystawia producent.
    if favourite.known_version() == Some(version) {
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

/// Pokazuje powiadomienie systemowe.
fn announce(news: &News) -> Result<()> {
    let mut notification = notify_rust::Notification::new();
    notification
        .summary(&news.summary())
        .body(&news.body())
        .appname("MapStick")
        .timeout(notify_rust::Timeout::Milliseconds(20_000));

    show(notification, &news.model_id)
}

#[cfg(target_os = "linux")]
fn show(mut notification: notify_rust::Notification, model_id: &str) -> Result<()> {
    let handle = notification.action("pokaz", "Pokaż").show()?;

    // Czekanie na kliknięcie odbywa się w osobnym wątku, żeby pulpit, który
    // powiadomień nie zamyka, nie zatrzymał nas na zawsze.
    let (tx, rx) = std::sync::mpsc::channel();
    let model_id = model_id.to_owned();

    std::thread::spawn(move || {
        handle.wait_for_action(|action| {
            if action == "pokaz" {
                open_app(&model_id);
            }
        });
        let _ = tx.send(());
    });

    let _ = rx.recv_timeout(ACTION_WAIT);

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn show(mut notification: notify_rust::Notification, _model_id: &str) -> Result<()> {
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
            latest_seen_version: None,
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
        }
    }

    /// Nie wiemy, co w aucie — mówimy o wydaniu i prosimy o zaznaczenie wersji.
    #[test]
    fn an_unknown_car_still_gets_told() {
        let news = worth_saying(&favourite(), "NE1.002").unwrap();

        assert_eq!(news.known, None);
        assert!(news.body().contains("Nie wiem, jaka wersja jest w aucie"));
        assert!(news.body().contains("2026.07"));
    }

    #[test]
    fn a_car_with_that_version_is_left_in_peace() {
        let mut f = favourite();
        f.installed_version = Some("NE1.002".to_owned());

        assert_eq!(worth_saying(&f, "NE1.002"), None);
    }

    #[test]
    fn an_older_car_hears_what_it_has_now() {
        let mut f = favourite();
        f.installed_version = Some("NE1.001".to_owned());

        let news = worth_saying(&f, "NE1.002").unwrap();
        assert!(news.body().contains("W aucie masz NE1.001."));
    }

    /// To samo wydanie mówi się raz.
    #[test]
    fn a_release_already_announced_stays_quiet() {
        let mut f = favourite();
        f.notified_version = Some("NE1.002".to_owned());

        assert_eq!(worth_saying(&f, "NE1.002"), None);
        assert!(worth_saying(&f, "NE1.003").is_some());
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
