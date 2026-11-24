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

/// Przedrostek wiadomości „pokaż ten model”. Gniazdem chodzą też odnośniki
/// ze strony producenta, a te zaczynają się od własnych schematów URI, więc
/// jedno od drugiego odróżnia się bez zgadywania.
pub const MODEL_PREFIX: &str = "model:";

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
    tracing::info!("Link handed over to the running instance");

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
                    "another instance is already listening",
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
            tracing::warn!("Not listening for links: {e}");
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

/// Nazwany potok — windowsowy odpowiednik gniazda. Potoki są widoczne
/// w całym systemie, stąd nazwa użytkownika: każdy ma mieć własne okno.
#[cfg(windows)]
fn pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    format!(r"\\.\pipe\mapstick-{user}")
}

/// Podaje odnośnik działającej instancji.
///
/// `true` znaczy „przekazane, ten proces nie ma już nic do roboty”.
#[cfg(windows)]
pub fn hand_off(url: &str) -> bool {
    use std::io::Write;

    // Potok otwiera się jak plik; brak potoku = nikt nie nasłuchuje. Po
    // zabitym procesie nie zostaje nic do sprzątania, inaczej niż z gniazdem.
    let Ok(mut pipe) = std::fs::OpenOptions::new().write(true).open(pipe_name()) else {
        return false;
    };

    if writeln!(pipe, "{url}").is_err() {
        return false;
    }

    let _ = pipe.flush();
    tracing::info!("Link handed over to the running instance");

    true
}

/// Zaczyna nasłuch. Każde połączenie niesie jeden odnośnik w jednym wierszu.
#[cfg(windows)]
pub fn listen(on_link: impl Fn(String) + Send + 'static) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::windows::named_pipe::ServerOptions;

    std::thread::Builder::new()
        .name("mapstick-odnosniki".to_owned())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
            else {
                return;
            };

            runtime.block_on(async move {
                let name = pipe_name();
                let options = || {
                    let mut o = ServerOptions::new();
                    // Tylko ten komputer; domyślne uprawnienia potoku i tak
                    // dają zapis wyłącznie właścicielowi i administratorom.
                    o.reject_remote_clients(true);
                    o
                };

                // first_pipe_instance: jeśli potok już jest, nasłuchuje inne
                // okno — nie podbieramy mu odnośników. Ale przez kilka sekund
                // próbujemy dalej: przy przejściu na renderer programowy
                // (main.rs) potok trzyma jeszcze nasz własny, kończący się
                // poprzednik. Bez tego okno nie nasłuchiwałoby wcale —
                // sprawdzone na Windows 11 bez OpenGL 2.
                let mut attempts = 0;
                let mut server = loop {
                    match options().first_pipe_instance(true).create(&name) {
                        Ok(s) => break s,
                        Err(_) if attempts < 30 => {
                            attempts += 1;
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                        Err(e) => {
                            tracing::warn!("Not listening for links: {e}");
                            return;
                        }
                    }
                };

                loop {
                    if server.connect().await.is_err() {
                        continue;
                    }

                    // Następna instancja potoku powstaje przed czytaniem, żeby
                    // kolejny klient nie trafił w chwilę, gdy potoku nie ma.
                    let connected = server;
                    server = match options().create(&name) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!("Stopped listening for links: {e}");
                            return;
                        }
                    };

                    let mut line = String::new();
                    if BufReader::new(connected).read_line(&mut line).await.is_ok() {
                        let url = line.trim().to_owned();

                        if !url.is_empty() {
                            on_link(url);
                        }
                    }
                }
            });
        })
        .ok();
}

/// Potok znika razem z procesem.
#[cfg(windows)]
pub fn cleanup() {}

/// Nazwa mutexu, który instalator sprawdza jako `AppMutex` (mapstick.iss).
#[cfg(windows)]
const RUNNING_MUTEX: &str = "MapStickRunning";

/// Daje instalatorowi znać, że MapStick działa.
///
/// Odinstalowanie przy otwartym oknie usuwało skrót, wpis i zadanie, ale nie
/// `mapstick.exe`: plik był w użyciu, a instalacja bez administratora nie
/// może zlecić usunięcia przy restarcie, więc zostawał na zawsze. Instalator
/// i deinstalator widzą ten mutex i proszą o zamknięcie programu. Uchwytu
/// nie zamykamy — robi to system razem z procesem.
#[cfg(windows)]
pub fn announce_running() {
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = RUNNING_MUTEX.encode_utf16().chain([0]).collect();

    // SAFETY: nazwa jest zakończona zerem i żyje do końca wywołania; bez
    // atrybutów bezpieczeństwa i bez przejmowania mutexu na własność.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };

    if handle.is_null() {
        tracing::warn!(
            "Could not create the {RUNNING_MUTEX} mutex: {}",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(not(windows))]
pub fn announce_running() {}

#[cfg(not(any(unix, windows)))]
pub fn hand_off(_url: &str) -> bool {
    false
}

#[cfg(not(any(unix, windows)))]
pub fn listen(_on_link: impl Fn(String) + Send + 'static) {}

#[cfg(not(any(unix, windows)))]
pub fn cleanup() {}
