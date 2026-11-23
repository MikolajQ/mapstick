// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! Katalog na archiwa pobierane przed rozpakowaniem.
//!
//! Nie leży na nośniku, bo o to właśnie chodzi: nośnik ma przyjąć wyłącznie
//! rozpakowany wynik. Po udanym pobraniu archiwa znikają same — zostają
//! wyłącznie po pobraniu przerwanym, i tylko po to, żeby dało się je wznowić
//! bez ściągania wszystkiego od nowa.
//!
//! Każde pobranie ma własny adres `<model>/<wersja>`. Bez tego resztki
//! z różnych aut mieszały się w jednym drzewie (`HU/images/…`) i o żadnej nie
//! dało się powiedzieć, czy jeszcze się przyda.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// Po tylu dniach bez ruchu resztki idą do kosza, choćby wersja wciąż była
/// aktualna. Pół roku — tyle, ile podał użytkownik.
pub const MAX_AGE: Duration = Duration::from_secs(182 * 24 * 60 * 60);

/// Znacznik nowego układu katalogów. Jego brak znaczy, że w środku leży
/// jeszcze płaskie drzewo sprzed podziału na model i wersję.
const LAYOUT_MARKER: &str = ".uklad-2";

pub fn base() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);

    let directory = base.join("mapstick");
    let path = directory.join("archives");

    // Do wersji 0.1 katalog nazywał się po polsku. Leżą w nim przerwane
    // pobrania — szkoda je zgubić przy zmianie nazwy.
    mapstick_store::adopt_legacy(&directory.join("archiwa"), &path);

    path
}

/// Katalog roboczy jednego pobrania.
pub fn for_download(model_id: &str, version: &str) -> PathBuf {
    base().join(safe(model_id)).join(safe(version))
}

/// Nazwa nadająca się na katalog. Identyfikatory producenta są łagodne, ale
/// nie chcemy, żeby cokolwiek z sieci decydowało o kształcie ścieżki.
fn safe(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();

    let trimmed = cleaned.trim_matches('.');

    if trimmed.is_empty() {
        "_".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Sumaryczny rozmiar katalogu, licząc podkatalogi.
pub fn directory_size(path: &Path) -> u64 {
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

/// Najświeższy czas zapisu w całym drzewie.
///
/// Sam katalog nadrzędny nie wystarcza: wznowione pobranie dopisuje pliki
/// w głębi, nie ruszając wierzchołka.
fn newest_mtime(path: &Path) -> Option<SystemTime> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    let mut newest = metadata.modified().ok();

    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).ok()?.flatten() {
            if let Some(deeper) = newest_mtime(&entry.path()) {
                newest = Some(newest.map_or(deeper, |n| n.max(deeper)));
            }
        }
    }

    newest
}

/// Jednorazowe sprzątnięcie po układzie sprzed podziału na model i wersję.
///
/// Tamtych resztek nie da się przypisać do auta ani do wydania, więc nie ma
/// czego ratować — a leżą tam wyłącznie przerwane pobrania.
pub fn migrate() -> u64 {
    migrate_in(&base())
}

fn migrate_in(base: &Path) -> u64 {
    let marker = base.join(LAYOUT_MARKER);

    // Znacznik zakładamy nawet na pustym miejscu: inaczej po ręcznym
    // „Wyczyść” katalog wracałby bez niego i następny start uznałby świeże
    // archiwa za pozostałość starego układu.
    if !base.exists() {
        if let Err(e) = std::fs::create_dir_all(base).and_then(|()| std::fs::write(&marker, b"")) {
            tracing::warn!("Nie udało się zapisać znacznika układu: {e}");
        }
        return 0;
    }

    if marker.exists() {
        return 0;
    }

    let freed = directory_size(base);

    if let Err(e) = std::fs::remove_dir_all(base) {
        tracing::warn!("Nie udało się usunąć starego układu archiwów: {e}");
        return 0;
    }

    if let Err(e) = std::fs::create_dir_all(base).and_then(|()| std::fs::write(&marker, b"")) {
        tracing::warn!("Nie udało się zapisać znacznika układu: {e}");
    }

    freed
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub directories: usize,
    pub bytes: u64,
}

/// Usuwa resztki, które nie mogą się już przydać.
///
/// `known` mapuje model na wersje wystawiane teraz przez producenta. Archiwum
/// wersji, której już nie ma w tym spisie, jest martwe: nudl wznawia wyłącznie
/// to samo wydanie, więc nikt tych plików nie dokończy.
pub fn sweep(known: &HashMap<String, Vec<String>>, max_age: Duration) -> Swept {
    sweep_in(&base(), known, max_age)
}

fn sweep_in(base: &Path, known: &HashMap<String, Vec<String>>, max_age: Duration) -> Swept {
    let now = SystemTime::now();
    let mut swept = Swept::default();

    let Ok(models) = std::fs::read_dir(base) else {
        return swept;
    };

    for model in models.flatten() {
        let model_path = model.path();
        if !model_path.is_dir() {
            continue;
        }

        let model_name = model.file_name().to_string_lossy().into_owned();
        let current = known
            .iter()
            .find(|(id, _)| safe(id) == model_name)
            .map(|(_, versions)| versions);

        let Ok(versions) = std::fs::read_dir(&model_path) else {
            continue;
        };

        for version in versions.flatten() {
            let path = version.path();
            if !path.is_dir() {
                continue;
            }

            let name = version.file_name().to_string_lossy().into_owned();

            let superseded =
                current.is_some_and(|versions| !versions.iter().any(|v| safe(v) == name));

            let stale = newest_mtime(&path)
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age > max_age);

            if !superseded && !stale {
                continue;
            }

            let size = directory_size(&path);

            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    tracing::info!(
                        "Usunięto archiwa {}/{} ({}) — {}",
                        model_name,
                        name,
                        size,
                        if superseded {
                            "wersja już nieaktualna"
                        } else {
                            "leżą od pół roku"
                        }
                    );
                    swept.directories += 1;
                    swept.bytes += size;
                }
                Err(e) => tracing::warn!("Nie udało się usunąć {}: {e}", path.display()),
            }
        }

        // Model bez żadnych archiwów nie ma po co zostawać.
        if std::fs::read_dir(&model_path).is_ok_and(|mut d| d.next().is_none()) {
            let _ = std::fs::remove_dir(&model_path);
        }
    }

    swept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_with_separators_cannot_escape_the_directory() {
        assert_eq!(safe("../../etc"), "_.._etc");
        assert_eq!(safe("HU/images"), "HU_images");
        assert_eq!(safe(".."), "_");
        assert_eq!(safe("New_IONIQ5_EUR"), "New_IONIQ5_EUR");
        assert_eq!(
            safe("NE1.ERT.ccNC_Lite.001.002.260312"),
            "NE1.ERT.ccNC_Lite.001.002.260312"
        );
    }

    fn cache(root: &Path, model: &str, version: &str, bytes: usize) {
        let dir = root.join(model).join(version);
        std::fs::create_dir_all(dir.join("HU/images")).unwrap();
        std::fs::write(dir.join("HU/images/plik"), vec![0u8; bytes]).unwrap();
    }

    /// Kluczowy przypadek: wyszło nowsze wydanie, więc starych archiwów już
    /// nikt nie dokończy.
    #[test]
    fn superseded_versions_are_removed_and_current_ones_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cache(root, "Bayon_EU", "stara", 100);
        cache(root, "Bayon_EU", "biezaca", 200);

        let known = HashMap::from([("Bayon_EU".to_owned(), vec!["biezaca".to_owned()])]);
        let swept = sweep_in(root, &known, MAX_AGE);

        assert_eq!(swept.directories, 1);
        assert_eq!(swept.bytes, 100);
        assert!(!root.join("Bayon_EU/stara").exists());
        assert!(root.join("Bayon_EU/biezaca").exists());
    }

    /// Model, o którego wersjach nic nie wiemy, zostaje nietknięty.
    #[test]
    fn unknown_models_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cache(root, "Cokolwiek", "jakas", 50);

        assert_eq!(sweep_in(root, &HashMap::new(), MAX_AGE), Swept::default());
        assert!(root.join("Cokolwiek/jakas").exists());
    }

    #[test]
    fn old_leftovers_go_even_when_the_version_is_still_current() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cache(root, "Bayon_EU", "biezaca", 100);

        let known = HashMap::from([("Bayon_EU".to_owned(), vec!["biezaca".to_owned()])]);
        let swept = sweep_in(root, &known, Duration::from_secs(0));

        assert_eq!(swept.directories, 1);
        assert!(!root.join("Bayon_EU").exists(), "pusty model też znika");
    }

    /// Po ręcznym „Wyczyść” katalog znika razem ze znacznikiem — kolejny
    /// start nie może wziąć świeżych archiwów za stary układ.
    #[test]
    fn a_missing_directory_gets_the_marker_without_wiping_anything() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("archiwa");

        assert_eq!(migrate_in(&root), 0);
        assert!(root.join(LAYOUT_MARKER).exists());

        cache(&root, "Bayon_EU", "biezaca", 100);
        assert_eq!(migrate_in(&root), 0);
        assert!(root.join("Bayon_EU/biezaca").exists());
    }

    #[test]
    fn the_flat_layout_is_wiped_once_and_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("HU/images")).unwrap();
        std::fs::write(root.join("HU/images/stare"), vec![0u8; 300]).unwrap();

        assert_eq!(migrate_in(root), 300);
        assert!(!root.join("HU").exists());

        // Drugie uruchomienie nie ma już nic do roboty.
        cache(root, "Bayon_EU", "biezaca", 100);
        assert_eq!(migrate_in(root), 0);
        assert!(root.join("Bayon_EU/biezaca").exists());
    }
}
