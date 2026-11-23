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

use crate::tr;

/// Czym budzi nas system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    // Każdy wariant powstaje tylko w swoim module, więc na każdym systemie
    // poza jednym reszta jest martwym kodem — stąd `allow` na wszystkich
    // czterech, nie tylko na dwóch nowych.
    #[allow(dead_code)]
    Systemd,
    #[allow(dead_code)]
    Cron,
    #[allow(dead_code)]
    Autostart,
    /// Windows: zadanie Harmonogramu zadań (`schtasks`).
    #[allow(dead_code)]
    TaskScheduler,
    /// macOS: agent launchd użytkownika.
    #[allow(dead_code)]
    Launchd,
}

impl Kind {
    /// Angielski klucz tłumaczenia — patrz [`crate::i18n`].
    pub fn label(self) -> &'static str {
        match self {
            Kind::Systemd => "a systemd timer",
            Kind::Cron => "a cron entry",
            Kind::Autostart => "startup at login",
            Kind::TaskScheduler => "a Task Scheduler task",
            Kind::Launchd => "a launchd agent",
        }
    }
}

/// Nazwa własna wszystkich zakładanych plików i wpisów. Używa jej tylko
/// linuksowa trójka dróg (systemd/cron/autostart) — Windows i macOS mają
/// własne, samodzielne nazwy niżej.
#[cfg(target_os = "linux")]
const NAME: &str = "mapstick-sprawdzanie";

/// Ścieżka do naszej binarki. Harmonogram wymaga bezwzględnej — nie wie nic
/// o katalogu, z którego ktoś uruchomił aplikację.
fn binary() -> Result<PathBuf> {
    std::env::current_exe().with_context(|| tr!("Could not find where the MapStick program is"))
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
            .with_context(|| tr!("Could not run {}", "systemctl"))?;

        if !status.success() {
            return Err(anyhow!(tr!(
                "{} failed: {}",
                format!("systemctl --user {}", args.join(" ")),
                status
            )));
        }

        Ok(())
    }

    /// Napis w postaci, którą systemd przyjmie jako jeden argument
    /// `ExecStart=` bez niespodzianek: w cudzysłowie (spacje w ścieżce),
    /// z podwojonymi `%` (specyfikatory) i `$` (zmienne środowiska).
    pub(super) fn unit_quoted(text: &str) -> String {
        let escaped = text
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$");

        format!("\"{escaped}\"")
    }

    /// Jednostka, którą systemd uruchamia, gdy samo sprawdzanie zawiedzie.
    fn failure_unit() -> String {
        format!("{NAME}-awaria.service")
    }

    /// Treść wszystkich trzech jednostek, z nazwami plików.
    ///
    /// Napisy w jednostkach są po angielsku, jak to w plikach systemd; po
    /// polsku zostaje tylko powiadomienie, które ktoś ma przeczytać.
    fn units() -> Result<[(String, String); 3]> {
        let exe = unit_quoted(&binary()?.to_string_lossy());
        let failure = failure_unit();

        let service = format!(
            "[Unit]\n\
             Description=MapStick: check for new map releases\n\
             Documentation=https://github.com/MikolajQ/mapstick\n\
             # MapStick cannot report a run that never started (program gone,\n\
             # crashed or hung). This unit does it instead.\n\
             OnFailure={failure}\n\
             \n\
             [Service]\n\
             Type=oneshot\n\
             ExecStart={exe} --sprawdz\n\
             # Each request has its own 90 s limit; this is only the safety net.\n\
             TimeoutStartSec=15min\n\
             # A window opened from the notification belongs to the user now,\n\
             # it must not go away when this service ends.\n\
             KillMode=process\n"
        );

        // Powiadomienie wysyła busctl, nie MapStick: ta jednostka ma działać
        // właśnie wtedy, gdy MapSticka nie da się uruchomić. busctl jest
        // częścią systemd, więc jest wszędzie tam, gdzie ten plik ma sens.
        let summary = unit_quoted(&tr!("MapStick: the update check did not run"));
        let body = unit_quoted(&tr!(
            "The background check for new map releases failed. Open MapStick once — \
             it sets the check up again. Details: journalctl --user -u {}.service",
            NAME
        ));
        let alarm = format!(
            "[Unit]\n\
             Description=MapStick: report that the update check failed\n\
             \n\
             [Service]\n\
             Type=oneshot\n\
             ExecStart=busctl --user call org.freedesktop.Notifications \
             /org/freedesktop/Notifications org.freedesktop.Notifications Notify \
             susssasa{{sv}}i MapStick 0 dialog-warning {summary} {body} 0 0 0\n"
        );

        let timer = "[Unit]\n\
             Description=MapStick: daily check for new map releases\n\
             \n\
             [Timer]\n\
             # Daily: MapStick itself decides whether to ask the maker today.\n\
             OnCalendar=daily\n\
             # Catch up on a run missed while the computer was off.\n\
             Persistent=true\n\
             # An hour of slack, so not everyone asks the service at the same minute.\n\
             RandomizedDelaySec=1h\n\
             \n\
             [Install]\n\
             WantedBy=timers.target\n"
            .to_owned();

        Ok([
            (format!("{NAME}.service"), service),
            (failure, alarm),
            (format!("{NAME}.timer"), timer),
        ])
    }

    fn write_units() -> Result<()> {
        let directory = unit_dir();
        std::fs::create_dir_all(&directory)
            .with_context(|| tr!("Could not create {}", directory.display()))?;

        for (name, text) in units()? {
            let path = directory.join(name);
            std::fs::write(&path, text)
                .with_context(|| tr!("Could not write {}", path.display()))?;
        }

        Ok(())
    }

    /// Czy w systemie stoi dokładnie to, co byśmy założyli — wtedy nie ma po
    /// co ruszać ani plików, ani systemctl, ani crontaba. Okno sprawdza to przy
    /// każdym starcie, więc tanio: odczyt trzech małych plików i dwa pytania
    /// do systemctl zamiast ośmiu wywołań.
    fn systemd_in_place() -> bool {
        let Ok(units) = units() else {
            return false;
        };
        let directory = unit_dir();

        let files_match = units.iter().all(|(name, text)| {
            std::fs::read_to_string(directory.join(name)).is_ok_and(|t| t == *text)
        });

        let timer = format!("{NAME}.timer");
        let asks = |what: &str| {
            Command::new("systemctl")
                .args(["--user", what, "--quiet", &timer])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };

        files_match && asks("is-enabled") && asks("is-active")
    }

    fn install_systemd() -> Result<()> {
        write_units()?;
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", "--now", &format!("{NAME}.timer")])
    }

    /// Wiersz crona z naszym znacznikiem — po nim poznajemy swoje wpisy.
    fn cron_line(_interval: Interval) -> Result<String> {
        // Cron oddaje wiersz powłoce, a `%` zamienia na nową linię — stąd
        // apostrofy i `\%`.
        let exe = binary()?.to_string_lossy().replace('\'', "'\\''").replace('%', "\\%");

        Ok(format!("@daily '{exe}' --sprawdz # {NAME}"))
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
            .with_context(|| tr!("Could not run {}", "crontab"))?;

        child
            .stdin
            .as_mut()
            .with_context(|| tr!("{} does not accept input", "crontab"))?
            .write_all(text.as_bytes())?;

        let status = child.wait()?;
        if !status.success() {
            return Err(anyhow!(tr!("{} failed: {}", "crontab", status)));
        }

        Ok(())
    }

    /// Przepisuje crontab bez naszych wpisów, dokładając nowy, jeśli podany.
    fn crontab_set(line: Option<&str>) -> Result<()> {
        let current = crontab_read().with_context(|| tr!("There is no {} command", "crontab"))?;

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

    /// Ścieżka jako jeden argument `Exec=` pliku `.desktop`.
    fn desktop_quoted(text: &str) -> String {
        let escaped = text
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('`', "\\`")
            .replace('$', "\\$")
            .replace('%', "%%");

        format!("\"{escaped}\"")
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
            desktop_quoted(&binary()?.to_string_lossy())
        );

        std::fs::write(&path, desktop)
            .with_context(|| tr!("Could not write {}", path.display()))?;

        Ok(())
    }

    /// Zakłada harmonogram najlepszą dostępną drogą.
    pub fn install(interval: Interval) -> Result<Kind> {
        if systemd_in_place() {
            return Ok(Kind::Systemd);
        }

        // Zaczynamy od czystego stanu: przy zmianie odstępu albo po zmianie
        // dostępnych dróg nie chcemy dwóch wyzwalaczy naraz.
        let _ = remove();

        if systemd_available() {
            match install_systemd() {
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
        let _ = std::fs::remove_file(directory.join(failure_unit()));

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

#[cfg(target_os = "windows")]
pub use windows::{current, install, remove};

/// Flaga `CreateProcess`, bez której każde wywołanie narzędzia konsolowego
/// (`schtasks`, `reg`) z okna aplikacji błyska czarnym oknem konsoli.
#[cfg(target_os = "windows")]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(target_os = "windows")]
mod windows {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    /// Nazwa zadania w Harmonogramie zadań.
    const TASK_NAME: &str = "MapStick sprawdzanie";

    fn schtasks(args: &[&str]) -> Result<std::process::Output> {
        Command::new("schtasks")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .with_context(|| tr!("Could not run {}", "schtasks"))
    }

    fn xml_escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }

    /// Definicja zadania — odpowiednik timera systemd z Linuksa.
    ///
    /// Przez XML, bo samo `schtasks /Create /SC DAILY` nie ustawi tego, co
    /// tu najważniejsze, a jego domyślne wartości sprawdzone na żywym
    /// Windows 11 psują sprawę:
    /// - bez `StartWhenAvailable` przegapione budzenie przepada, więc
    ///   komputer wyłączany na noc nie sprawdzałby nigdy (`Persistent=true`),
    /// - `DisallowStartIfOnBatteries` jest domyślnie włączone — laptop na
    ///   baterii też nie sprawdzałby nigdy,
    /// - `RandomDelay` rozkłada pytania do serwisu na godzinę
    ///   (`RandomizedDelaySec=1h`), zamiast losować godzinę raz na zawsze.
    ///
    /// Brak `UserId` w `Principal` znaczy: bieżący użytkownik, tylko gdy jest
    /// zalogowany — powiadomienie i tak nie ma komu się pokazać inaczej.
    fn task_xml(exe: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>MapStick — sprawdzanie nowych wydań map</Description>
  </RegistrationInfo>
  <Triggers>
    <CalendarTrigger>
      <StartBoundary>2026-01-01T00:00:00</StartBoundary>
      <RandomDelay>PT1H</RandomDelay>
      <ScheduleByDay>
        <DaysInterval>1</DaysInterval>
      </ScheduleByDay>
    </CalendarTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <ExecutionTimeLimit>PT1H</ExecutionTimeLimit>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>"{}"</Command>
      <Arguments>--sprawdz</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
            xml_escape(exe)
        )
    }

    pub fn install(_interval: Interval) -> Result<Kind> {
        let exe = binary()?;
        let xml = task_xml(&exe.to_string_lossy());

        // schtasks czyta XML z pliku; deklaracja mówi UTF-16, więc tak
        // zapisujemy — z BOM-em, jak eksportuje sam Harmonogram.
        let path = std::env::temp_dir().join("mapstick-zadanie.xml");
        let bytes: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain(xml.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        std::fs::write(&path, bytes)
            .with_context(|| tr!("Could not write {}", path.display()))?;

        let output = schtasks(&[
            "/Create",
            "/TN",
            TASK_NAME,
            "/XML",
            &path.to_string_lossy(),
            "/F",
        ]);
        let _ = std::fs::remove_file(&path);
        let output = output?;

        if !output.status.success() {
            return Err(anyhow!(tr!(
                "{} failed: {}",
                "schtasks /Create",
                format!(
                    "{} — {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                )
            )));
        }

        Ok(Kind::TaskScheduler)
    }

    pub fn remove() -> Result<()> {
        let _ = schtasks(&["/Delete", "/TN", TASK_NAME, "/F"]);
        Ok(())
    }

    pub fn current() -> Option<Kind> {
        schtasks(&["/Query", "/TN", TASK_NAME])
            .ok()
            .filter(|o| o.status.success())
            .map(|_| Kind::TaskScheduler)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_path_is_escaped_and_quoted() {
            let xml = task_xml(r"C:\Users\Jan & Ola\mapstick.exe");
            assert!(xml.contains(r#"<Command>"C:\Users\Jan &amp; Ola\mapstick.exe"</Command>"#));
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::{current, install, remove};

// UWAGA: pisane i przeglądane bez dostępu do macOS — nieprzetestowane na
// żywym systemie. `launchctl load`/`unload` i format property list są
// stabilne od dawna, ale przed wydaniem warto to sprawdzić na prawdziwym
// Maku.
#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::process::{Command, Stdio};

    /// Odwrócona domena jak w każdym identyfikatorze launchd.
    const LABEL: &str = "com.mapstick.sprawdzanie";

    fn agents_dir() -> PathBuf {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("Library")
            .join("LaunchAgents")
    }

    fn plist_path() -> PathBuf {
        agents_dir().join(format!("{LABEL}.plist"))
    }

    fn launchctl(args: &[&str]) -> Result<std::process::Output> {
        Command::new("launchctl")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| tr!("Could not run {}", "launchctl"))
    }

    fn random_hour() -> u32 {
        use std::time::{SystemTime, UNIX_EPOCH};

        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);

        3 + (seed % 4) // 03:00–06:00, z tego samego powodu co na Windows.
    }

    pub fn install(_interval: Interval) -> Result<Kind> {
        let _ = remove();

        let directory = agents_dir();
        std::fs::create_dir_all(&directory)
            .with_context(|| tr!("Could not create {}", directory.display()))?;

        let exe = binary()?;
        let hour = random_hour();

        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \t<key>Label</key>\n\
             \t<string>{LABEL}</string>\n\
             \t<key>ProgramArguments</key>\n\
             \t<array>\n\
             \t\t<string>{exe}</string>\n\
             \t\t<string>--sprawdz</string>\n\
             \t</array>\n\
             \t<key>StartCalendarInterval</key>\n\
             \t<dict>\n\
             \t\t<key>Hour</key>\n\
             \t\t<integer>{hour}</integer>\n\
             \t\t<key>Minute</key>\n\
             \t\t<integer>0</integer>\n\
             \t</dict>\n\
             \t<key>RunAtLoad</key>\n\
             \t<true/>\n\
             \t<key>StandardOutPath</key>\n\
             \t<string>/dev/null</string>\n\
             \t<key>StandardErrorPath</key>\n\
             \t<string>/dev/null</string>\n\
             </dict>\n\
             </plist>\n",
            exe = exe.display(),
        );

        let path = plist_path();
        std::fs::write(&path, plist)
            .with_context(|| tr!("Could not write {}", path.display()))?;

        let output = launchctl(&["load", "-w", &path.to_string_lossy()])?;
        if !output.status.success() {
            return Err(anyhow!(tr!(
                "{} failed: {}",
                "launchctl load",
                format!(
                    "{} — {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                )
            )));
        }

        Ok(Kind::Launchd)
    }

    pub fn remove() -> Result<()> {
        let path = plist_path();

        if path.exists() {
            let _ = launchctl(&["unload", "-w", &path.to_string_lossy()]);
            let _ = std::fs::remove_file(&path);
        }

        Ok(())
    }

    pub fn current() -> Option<Kind> {
        plist_path().exists().then_some(Kind::Launchd)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub use other::{current, install, remove};

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
mod other {
    use super::*;

    pub fn install(_interval: Interval) -> Result<Kind> {
        Err(anyhow!("Automatic checking is not available on this platform"))
    }

    pub fn remove() -> Result<()> {
        Ok(())
    }

    pub fn current() -> Option<Kind> {
        None
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::linux::unit_quoted;

    /// Ścieżka ze spacją, procentem i dolarem przechodzi przez `ExecStart=`
    /// jako jeden, dosłowny argument.
    #[test]
    fn unit_paths_survive_systemd_parsing() {
        assert_eq!(
            unit_quoted("/home/Jan Kowalski/100% $HOME/mapstick"),
            "\"/home/Jan Kowalski/100%% $$HOME/mapstick\""
        );
        assert_eq!(unit_quoted(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}
