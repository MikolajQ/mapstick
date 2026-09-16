// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! Zakładanie harmonogramu, który codziennie woła `mapstick --sprawdz`.
//!
//! Codziennie, niezależnie od tego, jak często użytkownik chce pytać
//! producenta: o tym odstępie i o codziennych przypomnieniach decyduje samo
//! sprawdzanie (patrz [`crate::watch`]). Harmonogram ma tylko dawać mu szansę
//! raz na dobę — budzenie, które nie ma nic do roboty, kończy się po rzucie
//! oka na plik.
//!
//! Sami nie budzimy się z niczego — od budzenia jest system. Na Linuksie
//! próbujemy po kolei trzech dróg, bo nie każdy pulpit ma systemd, a i tam,
//! gdzie jest, użytkownik potrafi go nie mieć w sesji:
//!
//! 1. **timer użytkownika systemd** — jedyny, który nadrabia przegapione
//!    uruchomienia (`Persistent=true`) i rozkłada je w czasie
//!    (`RandomizedDelaySec`),
//! 2. **cron** — prosty i wszędzie, ale bez nadrabiania: komputer wyłączony
//!    w niedzielę o północy znaczy tydzień bez sprawdzenia,
//! 3. **uruchomienie przy logowaniu** — najsłabsze, za to działa zawsze.
//!
//! Trzecia droga wystarcza z tego samego powodu: choćby coś wołało nas przy
//! każdym logowaniu, zapytanie do producenta poleci wedle odstępu,
//! a przypomnienie raz na dobę.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use mapstick_store::Interval;

/// Czym budzi nas system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Systemd,
    Cron,
    Autostart,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Systemd => "timer systemd",
            Kind::Cron => "wpis w cronie",
            Kind::Autostart => "uruchomienie przy logowaniu",
        }
    }
}

/// Nazwa własna wszystkich zakładanych plików i wpisów.
const NAME: &str = "mapstick-sprawdzanie";

/// Ścieżka do naszej binarki. Harmonogram wymaga bezwzględnej — nie wie nic
/// o katalogu, z którego ktoś uruchomił aplikację.
fn binary() -> Result<PathBuf> {
    std::env::current_exe().context("Nie wiem, gdzie leży binarka MapSticka")
}

#[cfg(target_os = "linux")]
pub use linux::{current, install, remove};

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    use std::process::{Command, Stdio};

    fn config_home() -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(std::env::temp_dir)
    }

    fn unit_dir() -> PathBuf {
        config_home().join("systemd").join("user")
    }

    fn autostart_file() -> PathBuf {
        config_home()
            .join("autostart")
            .join(format!("{NAME}.desktop"))
    }

    /// Czy w tej sesji w ogóle jest do kogo mówić.
    ///
    /// Sama obecność `systemctl` nic nie znaczy: w sesji bez menedżera
    /// użytkownika (kontener, część pulpitów, `su` na cudze konto) polecenie
    /// jest, a odpowiedzieć nie ma kto.
    fn systemd_available() -> bool {
        Command::new("systemctl")
            .args(["--user", "show", "--property=Version"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn systemctl(args: &[&str]) -> Result<()> {
        let status = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("Nie udało się uruchomić systemctl")?;

        if !status.success() {
            return Err(anyhow!("systemctl --user {} zwróciło {status}", args.join(" ")));
        }

        Ok(())
    }

    fn write_units(interval: Interval) -> Result<()> {
        let directory = unit_dir();
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("Nie udało się utworzyć {}", directory.display()))?;

        let service = format!(
            "[Unit]\n\
             Description=MapStick — sprawdzenie nowych wydań map\n\
             Documentation=https://github.com/MikolajQ/mapstick\n\
             \n\
             [Service]\n\
             Type=oneshot\n\
             ExecStart={} --sprawdz\n\
             # Okno otwarte z powiadomienia należy już do użytkownika, nie do\n\
             # tej usługi — bez tego zniknęłoby razem z jej zakończeniem.\n\
             KillMode=process\n",
            binary()?.display()
        );

        let timer = format!(
            "[Unit]\n\
             Description=MapStick — sprawdzanie nowych wydań map (pytania {}, przypomnienia codziennie)\n\
             \n\
             [Timer]\n\
             # Codziennie: o tym, czy dziś pytać producenta, decyduje sama aplikacja.\n\
             OnCalendar=daily\n\
             # Nadrabia budzenie przegapione przy wyłączonym komputerze.\n\
             Persistent=true\n\
             # Godzina luzu, żeby wszyscy nie pytali serwisu o tej samej minucie.\n\
             RandomizedDelaySec=1h\n\
             \n\
             [Install]\n\
             WantedBy=timers.target\n",
            interval.label()
        );

        std::fs::write(directory.join(format!("{NAME}.service")), service)?;
        std::fs::write(directory.join(format!("{NAME}.timer")), timer)?;

        Ok(())
    }

    fn install_systemd(interval: Interval) -> Result<()> {
        write_units(interval)?;
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", "--now", &format!("{NAME}.timer")])
    }

    /// Wiersz crona z naszym znacznikiem — po nim poznajemy swoje wpisy.
    fn cron_line(_interval: Interval) -> Result<String> {
        Ok(format!("@daily {} --sprawdz # {NAME}", binary()?.display()))
    }

    fn crontab_read() -> Option<String> {
        let output = Command::new("crontab").arg("-l").output().ok()?;

        // Puste crontabo to nie błąd, tylko brak wpisów.
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn crontab_write(text: &str) -> Result<()> {
        use std::io::Write;

        let mut child = Command::new("crontab")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Nie udało się uruchomić crontab")?;

        child
            .stdin
            .as_mut()
            .context("crontab nie przyjmuje danych")?
            .write_all(text.as_bytes())?;

        let status = child.wait()?;
        if !status.success() {
            return Err(anyhow!("crontab zwróciło {status}"));
        }

        Ok(())
    }

    /// Przepisuje crontab bez naszych wpisów, dokładając nowy, jeśli podany.
    fn crontab_set(line: Option<&str>) -> Result<()> {
        let current = crontab_read().context("Nie ma polecenia crontab")?;

        let mut text: String = current
            .lines()
            .filter(|l| !l.contains(NAME))
            .map(|l| format!("{l}\n"))
            .collect();

        if let Some(line) = line {
            text.push_str(line);
            text.push('\n');
        }

        crontab_write(&text)
    }

    fn install_autostart(interval: Interval) -> Result<()> {
        let path = autostart_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let desktop = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=MapStick — sprawdzanie aktualizacji map\n\
             Comment=Pyta producenta {}; o odstępie i przypomnieniach decyduje sama aplikacja\n\
             Exec={} --sprawdz\n\
             Terminal=false\n\
             NoDisplay=true\n\
             X-GNOME-Autostart-enabled=true\n",
            interval.label(),
            binary()?.display()
        );

        std::fs::write(&path, desktop)
            .with_context(|| format!("Nie udało się zapisać {}", path.display()))?;

        Ok(())
    }

    /// Zakłada harmonogram najlepszą dostępną drogą.
    pub fn install(interval: Interval) -> Result<Kind> {
        // Zaczynamy od czystego stanu: przy zmianie odstępu albo po zmianie
        // dostępnych dróg nie chcemy dwóch wyzwalaczy naraz.
        let _ = remove();

        if systemd_available() {
            match install_systemd(interval) {
                Ok(()) => return Ok(Kind::Systemd),
                Err(e) => tracing::warn!("Timer systemd nie wyszedł: {e}"),
            }
        }

        match crontab_set(Some(&cron_line(interval)?)) {
            Ok(()) => return Ok(Kind::Cron),
            Err(e) => tracing::warn!("Wpis w cronie nie wyszedł: {e}"),
        }

        install_autostart(interval).map(|()| Kind::Autostart)
    }

    /// Usuwa wszystko, co mogliśmy założyć — także drogami, których dziś już
    /// nie wybralibyśmy.
    pub fn remove() -> Result<()> {
        if systemd_available() {
            let _ = systemctl(&["disable", "--now", &format!("{NAME}.timer")]);
        }

        let directory = unit_dir();
        let _ = std::fs::remove_file(directory.join(format!("{NAME}.timer")));
        let _ = std::fs::remove_file(directory.join(format!("{NAME}.service")));

        if systemd_available() {
            let _ = systemctl(&["daemon-reload"]);
        }

        let _ = crontab_set(None);
        let _ = std::fs::remove_file(autostart_file());

        Ok(())
    }

    /// Czym budzi nas system teraz — o ile w ogóle.
    pub fn current() -> Option<Kind> {
        if unit_dir().join(format!("{NAME}.timer")).exists() {
            return Some(Kind::Systemd);
        }

        if crontab_read().is_some_and(|t| t.contains(NAME)) {
            return Some(Kind::Cron);
        }

        autostart_file().exists().then_some(Kind::Autostart)
    }
}

#[cfg(not(target_os = "linux"))]
pub use other::{current, install, remove};

#[cfg(not(target_os = "linux"))]
mod other {
    use super::*;

    /// Windows (Harmonogram zadań) i macOS (launchd) czekają na swoją kolej
    /// razem z resztą wsparcia dla tych systemów.
    pub fn install(_interval: Interval) -> Result<Kind> {
        Err(anyhow!(
            "Automatyczne sprawdzanie działa na razie tylko na Linuksie"
        ))
    }

    pub fn remove() -> Result<()> {
        Ok(())
    }

    pub fn current() -> Option<Kind> {
        None
    }
}
