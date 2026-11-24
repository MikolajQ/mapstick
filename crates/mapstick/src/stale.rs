// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! Pliki poprzedniego wydania, których nowe nie nadpisze.
//!
//! Prawie wszystkie pliki firmware'u mają stałe ścieżki (`HU/images/iasImage`,
//! `HU/firmware/update.tar.gz`), więc nowe pobranie po prostu je zastępuje.
//! Wyjątkiem jest archiwum map, którego nazwa niesie numer bazy
//! (`S5W_MAP_ALL_EUR_18_52_69_012_622_5.zip`) — i to ono zostaje na nośniku
//! jako sierota, mimo że waży trzy czwarte całości.
//!
//! Auto tego nie zainstaluje: czyta `<model>.ver`, a ten opisuje wyłącznie
//! jedno wydanie i jest nadpisywany razem z plikami. Sierota jest więc
//! wyłącznie zajętym miejscem — ale miejsca na nośniku bywa mało.
//!
//! Kasujemy tylko to, co wymienia stary manifest. Cudzych plików leżących na
//! nośniku nie ruszamy nigdy.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use nudl::version::VersionInfo;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stale {
    /// Ścieżki bezwzględne, gotowe do usunięcia.
    pub paths: Vec<PathBuf>,
    /// Ile miejsca zwolni ich usunięcie, licząc z metadanych, nie z manifestu.
    pub bytes: u64,
}

impl Stale {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// Znajduje pliki starego wydania, których nowe nie zastąpi.
///
/// `keep` to ścieżki względne, które zapisze nowe wydanie — cokolwiek w nich
/// jest, zostanie nadpisane w miejscu i nie jest sierotą.
pub fn find(target: &Path, model_id: &str, keep: &HashSet<String>) -> Stale {
    let manifest = target.join(format!("{model_id}.ver"));

    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return Stale::default();
    };

    let Ok(info) = text.parse::<VersionInfo>() else {
        tracing::warn!(
            "Cannot parse {} — leaving everything in place",
            manifest.display()
        );
        return Stale::default();
    };

    let mut stale = Stale::default();

    for entry in &info.entries {
        let relative = entry.path().into_owned();

        if keep.contains(&relative) {
            continue;
        }

        let path = target.join(&relative);

        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };

        // Manifest wymienia pliki; katalog pod tą ścieżką znaczy, że mamy do
        // czynienia z czymś innym, niż myślimy.
        if !metadata.is_file() {
            continue;
        }

        stale.bytes += metadata.len();
        stale.paths.push(path);
    }

    stale
}

/// Usuwa znalezione sieroty i puste katalogi, które po nich zostaną.
///
/// Zwraca liczbę faktycznie usuniętych plików.
pub fn remove(target: &Path, stale: &Stale) -> usize {
    let mut removed = 0;

    for path in &stale.paths {
        match std::fs::remove_file(path) {
            Ok(()) => {
                removed += 1;
                prune_empty(target, path.parent());
            }
            Err(e) => tracing::warn!("Could not remove {}: {e}", path.display()),
        }
    }

    removed
}

/// Zwija puste katalogi w górę, ale nigdy powyżej katalogu docelowego.
fn prune_empty(target: &Path, mut directory: Option<&Path>) {
    while let Some(path) = directory {
        if path == target || !path.starts_with(target) {
            return;
        }

        let empty = std::fs::read_dir(path).is_ok_and(|mut d| d.next().is_none());
        if !empty || std::fs::remove_dir(path).is_err() {
            return;
        }

        directory = path.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manifest w kształcie tego, co pisze nudl: nagłówek plus po linijce na plik.
    fn manifest(target: &Path, model: &str, version: &str, files: &[(&str, u64)]) {
        let mut text = format!("+|21260|{version}|HM|{model}|BC3|1\n");

        for (path, size) in files {
            let (directory, name) = match path.rsplit_once('/') {
                Some((d, n)) => (format!("\\{}", d.replace('/', "\\")), n.to_string()),
                None => (String::new(), path.to_string()),
            };
            text.push_str(&format!("{model}{directory}|{name}|1|0|{size}|1\n"));

            let full = target.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, vec![0u8; *size as usize]).unwrap();
        }

        std::fs::write(target.join(format!("{model}.ver")), text).unwrap();
    }

    #[test]
    fn only_files_the_new_release_will_not_overwrite_are_listed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path();

        manifest(
            target,
            "Bayon_EU",
            "stara",
            &[
                ("HU/images/iasImage", 100),
                ("HU/images/navi_eu/S5W_MAP_ALL_EUR_18_52.zip", 900),
            ],
        );

        // Nowe wydanie ma tę samą ścieżkę obrazu, ale inne archiwum map.
        let keep = HashSet::from([
            "HU/images/iasImage".to_owned(),
            "HU/images/navi_eu/S5W_MAP_ALL_EUR_19_00.zip".to_owned(),
        ]);

        let stale = find(target, "Bayon_EU", &keep);

        assert_eq!(stale.bytes, 900);
        assert_eq!(
            stale.paths,
            vec![target.join("HU/images/navi_eu/S5W_MAP_ALL_EUR_18_52.zip")]
        );
    }

    #[test]
    fn removing_takes_the_emptied_directories_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path();

        manifest(
            target,
            "Bayon_EU",
            "stara",
            &[("HU/images/navi_eu/mapy.zip", 500)],
        );

        let stale = find(target, "Bayon_EU", &HashSet::new());
        assert_eq!(remove(target, &stale), 1);

        assert!(!target.join("HU").exists(), "puste katalogi mają zniknąć");
        assert!(target.join("Bayon_EU.ver").exists(), "manifest zostaje");
    }

    /// Nośnik bez manifestu to nośnik, o którym nic nie wiemy — nie ruszamy go.
    #[test]
    fn a_medium_without_a_manifest_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cudzy-plik.txt"), b"moje").unwrap();

        assert_eq!(
            find(dir.path(), "Bayon_EU", &HashSet::new()),
            Stale::default()
        );
    }

    /// Uszkodzony manifest też nie jest podstawą do kasowania.
    #[test]
    fn a_manifest_we_cannot_parse_stops_us() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Bayon_EU.ver"), b"cokolwiek").unwrap();

        assert!(find(dir.path(), "Bayon_EU", &HashSet::new()).is_empty());
    }

    /// Pliki spoza manifestu zostają, nawet gdy leżą w tych samych katalogach.
    #[test]
    fn files_outside_the_manifest_are_never_touched() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path();

        manifest(target, "Bayon_EU", "stara", &[("HU/images/stary.bin", 10)]);
        std::fs::write(target.join("HU/images/moje-zdjecie.jpg"), b"x").unwrap();

        let stale = find(target, "Bayon_EU", &HashSet::new());
        remove(target, &stale);

        assert!(target.join("HU/images/moje-zdjecie.jpg").exists());
        assert!(!target.join("HU/images/stary.bin").exists());
    }
}
