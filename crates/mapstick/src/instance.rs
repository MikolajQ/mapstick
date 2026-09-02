// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! Jedno okno na użytkownika.
//!
//! Kliknięcie w odnośnik na stronie producenta uruchamia aplikację od nowa.
//! Bez przekazania go do już otwartego okna każde kliknięcie zostawiałoby
//! kolejną kopię MapSticka na pulpicie.

#[cfg(unix)]
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
};

#[cfg(unix)]
fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("mapstick.sock")
}

/// Podaje odnośnik działającej instancji.
///
/// `true` znaczy „przekazane, ten proces nie ma już nic do roboty”.
#[cfg(unix)]
pub fn hand_off(url: &str) -> bool {
    let path = socket_path();

    let Ok(mut stream) = UnixStream::connect(&path) else {
        // Gniazdo po procesie, którego już nie ma, zablokowałoby nasłuch.
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        return false;
    };

    if writeln!(stream, "{url}").is_err() {
        return false;
    }

    let _ = stream.flush();
    tracing::info!("Odnośnik przekazany działającej instancji");

    true
}

/// Zajmuje gniazdo, sprzątając po procesie, który nie zdążył tego zrobić sam.
///
/// Plik gniazda przeżywa ubity proces, więc samo „zajęte” nic nie znaczy —
/// dopiero nieudane połączenie dowodzi, że po drugiej stronie nikogo nie ma.
#[cfg(unix)]
fn bind(path: &std::path::Path) -> std::io::Result<UnixListener> {
    match UnixListener::bind(path) {
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "inna instancja już nasłuchuje",
                ));
            }

            std::fs::remove_file(path)?;
            UnixListener::bind(path)
        }
        other => other,
    }
}

/// Zaczyna nasłuch. Każdy odebrany wiersz to jeden odnośnik.
#[cfg(unix)]
pub fn listen(on_link: impl Fn(String) + Send + 'static) {
    let path = socket_path();

    let listener = match bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("Nie nasłuchuję odnośników: {e}");
            return;
        }
    };

    std::thread::Builder::new()
        .name("mapstick-odnosniki".to_owned())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();

                if BufReader::new(stream).read_line(&mut line).is_ok() {
                    let url = line.trim().to_owned();

                    if !url.is_empty() {
                        on_link(url);
                    }
                }
            }
        })
        .ok();
}

/// Sprząta gniazdo. Bez tego kolejne uruchomienie zastałoby martwy plik.
#[cfg(unix)]
pub fn cleanup() {
    let _ = std::fs::remove_file(socket_path());
}

#[cfg(not(unix))]
pub fn hand_off(_url: &str) -> bool {
    false
}

#[cfg(not(unix))]
pub fn listen(_on_link: impl Fn(String) + Send + 'static) {}

#[cfg(not(unix))]
pub fn cleanup() {}
