// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! Przeniesienie gotowego oprogramowania z dysku na nośnik.
//!
//! Kopiowanie idzie najpierw do katalogu przejściowego na nośniku, a dopiero
//! na koniec pliki dostają właściwe nazwy — plik `.ver` jako ostatni. Dzięki
//! temu nośnik po przerwanym kopiowaniu nie wygląda na gotowy: auto nie
//! znajdzie na nim wersji, więc nie spróbuje wgrać połowy map.

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result};

use crate::tr;

/// Katalog przejściowy, zakładany na nośniku na czas kopiowania.
pub const STAGING: &str = ".mapstick-niegotowe";

/// Ile bajtów naraz. Duży bufor, bo to strumień na kość pamięci, nie edytor.
const CHUNK: usize = 1 << 20;

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Cancelled,
}

/// Kopiuje `source` do `target`, zostawiając wynik dopiero po komplecie.
///
/// `on_bytes` dostaje przyrosty, nie sumy — licznik trzyma wywołujący.
pub fn stage_and_place(
    source: &Path,
    target: &Path,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
) -> Result<Outcome> {
    let staging = target.join(STAGING);

    // Pozostałość po wcześniejszym przerwanym kopiowaniu jest bezużyteczna:
    // nie wiadomo, które pliki są całe.
    if staging.exists() {
        fs::remove_dir_all(&staging)
            .with_context(|| tr!("Could not remove {}", staging.display()))?;
    }
    fs::create_dir_all(&staging)
        .with_context(|| tr!("Could not create {}", staging.display()))?;

    if copy_dir(source, &staging, cancel, on_bytes)? == Outcome::Cancelled {
        return Ok(Outcome::Cancelled);
    }

    place(&staging, target)?;

    fs::remove_dir_all(&staging)
        .with_context(|| tr!("Could not remove {}", staging.display()))?;

    Ok(Outcome::Done)
}

fn copy_dir(
    from: &Path,
    to: &Path,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
) -> Result<Outcome> {
    let entries =
        fs::read_dir(from).with_context(|| tr!("Could not read {}", from.display()))?;

    for entry in entries {
        if cancel.load(Ordering::Relaxed) {
            return Ok(Outcome::Cancelled);
        }

        let entry = entry.with_context(|| tr!("Could not read {}", from.display()))?;
        let source = entry.path();
        let target = to.join(entry.file_name());

        let kind = entry
            .file_type()
            .with_context(|| tr!("Could not check {}", source.display()))?;

        let outcome = if kind.is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| tr!("Could not create {}", target.display()))?;
            copy_dir(&source, &target, cancel, on_bytes)?
        } else {
            copy_file(&source, &target, cancel, on_bytes)?
        };

        if outcome == Outcome::Cancelled {
            return Ok(Outcome::Cancelled);
        }
    }

    Ok(Outcome::Done)
}

fn copy_file(
    source: &Path,
    target: &Path,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
) -> Result<Outcome> {
    let mut input = fs::File::open(source)
        .with_context(|| tr!("Could not open {}", source.display()))?;
    let mut output = fs::File::create(target)
        .with_context(|| tr!("Could not create {}", target.display()))?;

    let mut buffer = vec![0u8; CHUNK];

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(Outcome::Cancelled);
        }

        let read = input
            .read(&mut buffer)
            .with_context(|| tr!("Could not read {}", source.display()))?;

        if read == 0 {
            break;
        }

        output
            .write_all(&buffer[..read])
            .with_context(|| tr!("Could not write {}", target.display()))?;

        on_bytes(read as u64);
    }

    // Bez tego „skopiowane” znaczyłoby tylko „oddane systemowi”, a nośnik dałoby
    // się wyjąć z niedopisanymi danymi.
    output
        .sync_all()
        .with_context(|| tr!("Could not append to {}", target.display()))?;

    Ok(Outcome::Done)
}

/// Przenosi zawartość katalogu przejściowego na właściwe miejsce.
///
/// Wszystko leży już na tym samym systemie plików, więc to zmiana nazwy —
/// natychmiastowa, bez drugiego przepisywania danych.
fn place(staging: &Path, target: &Path) -> Result<()> {
    let entries = fs::read_dir(staging)
        .with_context(|| tr!("Could not read {}", staging.display()))?;

    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry =
            entry.with_context(|| tr!("Could not read {}", staging.display()))?;
        paths.push(entry.path());
    }

    // Plik `.ver` idzie na końcu: to on mówi autu, że nośnik jest gotowy.
    paths.sort_by_key(|p| is_version_file(p));

    for path in paths {
        let Some(name) = path.file_name() else {
            continue;
        };
        let destination = target.join(name);

        // Zmiana nazwy nie nadpisze istniejącego katalogu, a plik nadpisze
        // tylko czasem — usuwamy jawnie, żeby zachowanie nie zależało od
        // systemu.
        if destination.exists() {
            let previous = fs::symlink_metadata(&destination).with_context(|| {
                tr!("Could not check {}", destination.display())
            })?;

            if previous.is_dir() {
                fs::remove_dir_all(&destination)
            } else {
                fs::remove_file(&destination)
            }
            .with_context(|| tr!("Could not remove {}", destination.display()))?;
        }

        fs::rename(&path, &destination).with_context(|| {
            tr!(
                "Could not move {} to {}",
                path.display(),
                destination.display()
            )
        })?;
    }

    Ok(())
}

fn is_version_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("ver"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drzewo w kształcie tego, co zostaje po rozpakowaniu firmware'u.
    fn sample(root: &Path) {
        fs::create_dir_all(root.join("MAP/DATA")).unwrap();
        fs::write(root.join("MAP/DATA/mapa.bin"), vec![7u8; 3000]).unwrap();
        fs::write(root.join("MAP/spis.txt"), b"lista").unwrap();
        fs::write(root.join("NE1.ver"), b"tresc wersji").unwrap();
    }

    #[test]
    fn copies_the_whole_tree_and_counts_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let (source, target) = (dir.path().join("dysk"), dir.path().join("pen"));
        sample(&source);
        fs::create_dir_all(&target).unwrap();

        let mut copied = 0u64;
        let outcome = stage_and_place(
            &source,
            &target,
            &AtomicBool::new(false),
            &mut |n| copied += n,
        )
        .unwrap();

        assert_eq!(outcome, Outcome::Done);
        assert_eq!(copied, 3000 + 5 + 12);
        assert_eq!(
            fs::read(target.join("MAP/DATA/mapa.bin")).unwrap().len(),
            3000
        );
        assert_eq!(fs::read(target.join("NE1.ver")).unwrap(), b"tresc wersji");
        assert!(!target.join(STAGING).exists(), "katalog przejściowy został");
    }

    /// Sedno całej konstrukcji: przerwane kopiowanie nie może zostawić nośnika
    /// wyglądającego na gotowy.
    #[test]
    fn cancelled_copy_leaves_no_version_file_on_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let (source, target) = (dir.path().join("dysk"), dir.path().join("pen"));
        sample(&source);
        fs::create_dir_all(&target).unwrap();

        let cancel = AtomicBool::new(false);
        let outcome = stage_and_place(&source, &target, &cancel, &mut |_| {
            cancel.store(true, Ordering::Relaxed);
        })
        .unwrap();

        assert_eq!(outcome, Outcome::Cancelled);
        assert!(!target.join("NE1.ver").exists());
        assert!(!target.join("MAP").exists());
        assert!(target.join(STAGING).exists());
    }

    /// Powtórzone kopiowanie zaczyna od czystego katalogu przejściowego.
    #[test]
    fn a_second_run_clears_the_leftover_staging_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (source, target) = (dir.path().join("dysk"), dir.path().join("pen"));
        sample(&source);
        fs::create_dir_all(target.join(STAGING).join("smiec")).unwrap();

        stage_and_place(&source, &target, &AtomicBool::new(false), &mut |_| {}).unwrap();

        assert!(target.join("NE1.ver").exists());
        assert!(!target.join("smiec").exists());
    }

    /// Nośnik z poprzednim wydaniem: stare pliki ustępują nowym.
    #[test]
    fn existing_entries_are_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let (source, target) = (dir.path().join("dysk"), dir.path().join("pen"));
        sample(&source);
        fs::create_dir_all(target.join("MAP/STARE")).unwrap();
        fs::write(target.join("NE1.ver"), b"stara wersja").unwrap();

        stage_and_place(&source, &target, &AtomicBool::new(false), &mut |_| {}).unwrap();

        assert_eq!(fs::read(target.join("NE1.ver")).unwrap(), b"tresc wersji");
        assert!(!target.join("MAP/STARE").exists());
    }

    #[test]
    fn version_file_is_placed_last() {
        let mut paths = [
            PathBuf::from("a/NE1.ver"),
            PathBuf::from("a/MAP"),
            PathBuf::from("a/DATA.bin"),
        ];
        paths.sort_by_key(|p| is_version_file(p));

        assert_eq!(paths.last().unwrap(), &PathBuf::from("a/NE1.ver"));
    }
}
