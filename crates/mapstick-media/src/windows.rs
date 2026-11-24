// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! Backend Windows oparty na module Storage PowerShella (`Get-Disk`,
//! `Get-Partition`, `Get-Volume`). Odczyt nie wymaga uprawnień
//! administratora; formatowanie tak — idzie przez systemowe okno UAC, tak jak
//! na Linuksie przez okno polkit.
//!
//! PowerShell zamiast WinAPI: te same fakty przez `DeviceIoControl` wymagają
//! otwarcia `\\.\PhysicalDriveN` z prawem odczytu, czyli administratora,
//! a magistrala, flagi rozruchu i typy partycji przychodzą tu gotowe.

use std::{
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde::Deserialize;

use crate::{DriveFacts, Error, Medium, Result, Space, check_drive, is_efi_partition_type};

/// Flaga `CreateProcess`: bez niej każde wywołanie PowerShella z okna
/// aplikacji błyska czarnym oknem konsoli.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Magistrale, którymi podłącza się nośnik zanoszony do auta. Reszta
/// (SATA, NVMe, RAID, SAS, wirtualne…) to dyski wewnętrzne.
const EXTERNAL_BUSES: &[&str] = &["USB", "SD", "MMC"];

/// Jedno zapytanie o wszystkie dyski z partycjami i woluminami.
///
/// Litera dysku przychodzi jako `[char]`, a jej brak jako znak `\0` —
/// zamieniamy go na pusty napis po stronie PowerShella.
const LIST_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$disks = foreach ($d in Get-Disk) {
    $partitions = foreach ($p in @(Get-Partition -DiskNumber $d.Number -ErrorAction SilentlyContinue)) {
        $v = $p | Get-Volume -ErrorAction SilentlyContinue
        $letter = if ($p.DriveLetter -and [int][char]$p.DriveLetter -ne 0) { [string]$p.DriveLetter } else { '' }
        [pscustomobject]@{
            Letter     = $letter
            GptType    = [string]$p.GptType
            MbrType    = [int]$p.MbrType
            IsSystem   = [bool]$p.IsSystem
            IsBoot     = [bool]$p.IsBoot
            FileSystem = [string]$v.FileSystem
            Size       = [uint64]$v.Size
            Free       = [uint64]$v.SizeRemaining
        }
    }
    [pscustomobject]@{
        Number       = $d.Number
        FriendlyName = [string]$d.FriendlyName
        BusType      = [string]$d.BusType
        IsBoot       = [bool]$d.IsBoot
        IsSystem     = [bool]$d.IsSystem
        IsOffline    = [bool]$d.IsOffline
        IsReadOnly   = [bool]$d.IsReadOnly
        Partitions   = @($partitions)
    }
}
ConvertTo-Json -InputObject @($disks) -Depth 4 -Compress
"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Disk {
    number: u32,
    friendly_name: String,
    bus_type: String,
    is_boot: bool,
    is_system: bool,
    is_offline: bool,
    is_read_only: bool,
    partitions: Vec<Partition>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Partition {
    letter: String,
    gpt_type: String,
    mbr_type: u32,
    is_system: bool,
    is_boot: bool,
    file_system: String,
    size: u64,
    free: u64,
}

fn backend<E>(e: E) -> Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    Error::Backend(Box::new(e))
}

/// `-EncodedCommand` przyjmuje UTF-16LE w base64 — bez kłopotów
/// z cudzysłowami w argumentach.
fn encode(script: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | u32::from(b) << (16 - 8 * i));

        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }

    out
}

fn powershell(script: &str) -> Command {
    // Wyjście w UTF-8: nazwy napędów bywają spoza ASCII.
    let script = format!("[Console]::OutputEncoding = [Text.Encoding]::UTF8\n{script}");
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
        ])
        .args(["-EncodedCommand", &encode(&script)])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);
    command
}

fn query_disks() -> Result<Vec<Disk>> {
    let output = powershell(LIST_SCRIPT).output().map_err(backend)?;

    if !output.status.success() {
        return Err(Error::Service(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }

    serde_json::from_slice(&output.stdout).map_err(backend)
}

/// Fakty o całym dysku — te same cztery warunki, co na Linuksie.
///
/// Punktów montowania nie podajemy: reguła katalogów `/run/media` nie ma tu
/// odpowiednika, a jej rolę („coś systemowego jest zamontowane z tego
/// dysku”) pełnią flagi rozruchu i systemu, które Windows ustawia sam.
fn facts(disk: &Disk) -> DriveFacts {
    let external = EXTERNAL_BUSES
        .iter()
        .any(|b| b.eq_ignore_ascii_case(disk.bus_type.trim()));

    DriveFacts {
        connection: external.then(|| disk.bus_type.to_ascii_lowercase()),
        mount_points: Vec::new(),
        has_system_hint: disk.is_boot
            || disk.is_system
            || disk.partitions.iter().any(|p| p.is_boot || p.is_system),
        has_efi_partition: disk.partitions.iter().any(|p| {
            is_efi_partition_type(p.gpt_type.trim_matches(['{', '}']))
                || is_efi_partition_type(&format!("{:#x}", p.mbr_type))
        }),
    }
}

/// Wolumeny, które wolno pokazać, z jednego odczytu dysków.
fn usable(disks: Vec<Disk>) -> Vec<Medium> {
    let mut media = Vec::new();

    for disk in disks {
        if disk.is_offline || disk.is_read_only {
            continue;
        }

        let drive_id = format!("disk{}", disk.number);

        if let Err(reason) = check_drive(&facts(&disk)) {
            tracing::debug!(
                "Skipping {drive_id} ({}): {}",
                disk.friendly_name,
                reason.reason()
            );
            continue;
        }

        let connection = facts(&disk).connection;

        // Wolumen bez litery nie ma dokąd pisać; Windows przydziela litery
        // nośnikom wymiennym sam, także niesformatowanym.
        for partition in disk.partitions.iter().filter(|p| !p.letter.is_empty()) {
            media.push(Medium {
                id: format!("{}:", partition.letter),
                drive_id: drive_id.clone(),
                label: disk.friendly_name.trim().to_owned(),
                mount_point: Some(PathBuf::from(format!("{}:\\", partition.letter))),
                filesystem: Some(partition.file_system.clone()).filter(|f| !f.is_empty()),
                total_bytes: partition.size,
                free_bytes: partition.free,
                connection: connection.clone(),
            });
        }
    }

    media.sort_by(|a, b| a.id.cmp(&b.id));
    media
}

pub async fn list_media() -> Result<Vec<Medium>> {
    let disks = tokio::task::spawn_blocking(query_disks)
        .await
        .map_err(backend)??;

    Ok(usable(disks))
}

pub fn space_for(path: &Path) -> Result<Space> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let (mut available, mut total) = (0u64, 0u64);

    // SAFETY: `wide` jest zakończone zerem i żyje przez całe wywołanie;
    // wskaźniki wyjściowe prowadzą do zmiennych na stosie.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            &mut total,
            std::ptr::null_mut(),
        )
    };

    if ok == 0 {
        return Err(Error::Space(
            path.to_owned(),
            std::io::Error::last_os_error(),
        ));
    }

    Ok(Space {
        total,
        free: available,
    })
}

/// Formatuje wolumen do exFAT. **Kasuje wszystkie dane na nim.**
///
/// Jak na Linuksie, sprawdzenie bezpieczeństwa powtarzamy na świeżo
/// odczytanym stanie. Do tego podwyższony skrypt sprawdza dysk jeszcze raz,
/// już z uprawnieniami administratora, tuż przed `Format-Volume` — litera
/// mogła przez chwilę między jednym a drugim trafić do innego napędu.
pub async fn format_exfat(medium: &Medium, label: &str) -> Result<()> {
    let current = list_media().await?;
    if !current.iter().any(|m| m.id == medium.id) {
        return Err(Error::SystemDisk(medium.id.clone()));
    }

    let letter = medium
        .id
        .strip_suffix(':')
        .filter(|l| l.len() == 1 && l.chars().all(|c| c.is_ascii_alphabetic()))
        .ok_or_else(|| Error::NotMounted(medium.id.clone()))?
        .to_owned();

    // Etykieta exFAT: do 11 znaków; tylko bezpieczne znaki, bo trafia do skryptu.
    let label: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(11)
        .collect();

    let id = medium.id.clone();
    tokio::task::spawn_blocking(move || format_elevated(&letter, &label, &id))
        .await
        .map_err(backend)?
}

fn format_elevated(letter: &str, label: &str, id: &str) -> Result<()> {
    let report = std::env::temp_dir().join(format!("mapstick-format-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&report);

    let buses = EXTERNAL_BUSES
        .iter()
        .map(|b| format!("'{b}'"))
        .collect::<Vec<_>>()
        .join(",");

    // Kod wyjścia 3 = odmowa ze względów bezpieczeństwa (4 = anulowane UAC,
    // ustawia go skrypt zewnętrzny); treść błędu ląduje
    // w pliku, bo podwyższony proces nie ma jak oddać nam swojego stderr.
    let inner = format!(
        r#"
$ErrorActionPreference = 'Stop'
try {{
    $p = Get-Partition -DriveLetter {letter}
    $d = $p | Get-Disk
    if ([string]$d.BusType -notin @({buses}) -or $d.IsBoot -or $d.IsSystem -or $p.IsBoot -or $p.IsSystem) {{
        exit 3
    }}
    Format-Volume -Partition $p -FileSystem exFAT -NewFileSystemLabel '{label}' -Force -Confirm:$false | Out-Null
    exit 0
}} catch {{
    $_.Exception.Message | Set-Content -Encoding utf8 -LiteralPath '{report}'
    exit 1
}}
"#,
        report = report.display()
    );

    // Zewnętrzny PowerShell tylko podnosi uprawnienia i oddaje kod wyjścia.
    //
    // Process.Start z `runas` zamiast Start-Process, bo ten w PowerShellu 5.1
    // przepakowuje błąd w InvalidOperationException i gubi Win32Exception
    // z kodem — anulowane UAC (ERROR_CANCELLED, 1223) dawało się rozpoznać
    // już tylko po przetłumaczonym tekście. Wcześniej, bez `Stop`, gorzej:
    // pusty `$p` i `exit $null` = `exit 0`, anulowanie wyglądało jak udane
    // formatowanie. Oba sprawdzone na żywo.
    let outer = format!(
        "$ErrorActionPreference = 'Stop'\n\
         $i = New-Object System.Diagnostics.ProcessStartInfo 'powershell.exe'\n\
         $i.Verb = 'runas'\n\
         $i.UseShellExecute = $true\n\
         $i.WindowStyle = 'Hidden'\n\
         $i.Arguments = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {}'\n\
         try {{\n\
             $p = [System.Diagnostics.Process]::Start($i)\n\
         }} catch {{\n\
             $e = $_.Exception\n\
             while ($e) {{\n\
                 if ($e -is [ComponentModel.Win32Exception] -and $e.NativeErrorCode -eq 1223) {{ exit 4 }}\n\
                 $e = $e.InnerException\n\
             }}\n\
             [Console]::Error.WriteLine($_.Exception.Message)\n\
             exit 2\n\
         }}\n\
         if ($null -eq $p) {{ exit 2 }}\n\
         $p.WaitForExit()\n\
         exit $p.ExitCode",
        encode(&inner)
    );

    let output = powershell(&outer).output().map_err(backend)?;
    let message = std::fs::read_to_string(&report).unwrap_or_default();
    let _ = std::fs::remove_file(&report);

    match output.status.code() {
        Some(0) => Ok(()),
        Some(3) => Err(Error::SystemDisk(id.to_owned())),
        // ERROR_CANCELLED z okna UAC.
        Some(4) => Err(Error::Cancelled),
        _ => {
            let reason = if message.trim().is_empty() {
                String::from_utf8_lossy(&output.stderr).trim().to_owned()
            } else {
                message.trim().trim_start_matches('\u{feff}').to_owned()
            };
            Err(Error::Service(reason))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(bus: &str, partitions: Vec<Partition>) -> Disk {
        Disk {
            number: 1,
            friendly_name: "SanDisk Ultra".to_owned(),
            bus_type: bus.to_owned(),
            is_boot: false,
            is_system: false,
            is_offline: false,
            is_read_only: false,
            partitions,
        }
    }

    fn partition(letter: &str) -> Partition {
        Partition {
            letter: letter.to_owned(),
            gpt_type: String::new(),
            mbr_type: 7,
            is_system: false,
            is_boot: false,
            file_system: "exFAT".to_owned(),
            size: 64 << 30,
            free: 60 << 30,
        }
    }

    #[test]
    fn a_usb_stick_is_offered() {
        let media = usable(vec![disk("USB", vec![partition("E")])]);

        assert_eq!(media.len(), 1);
        assert_eq!(media[0].id, "E:");
        assert_eq!(media[0].mount_point.as_deref(), Some(Path::new("E:\\")));
        assert_eq!(media[0].connection.as_deref(), Some("usb"));
    }

    #[test]
    fn internal_and_system_disks_are_not() {
        let mut boot = disk("USB", vec![partition("C")]);
        boot.is_boot = true;

        let mut efi = partition("F");
        efi.gpt_type = "{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}".to_owned();

        let mut mbr_efi = partition("G");
        mbr_efi.mbr_type = 0xef;

        let media = usable(vec![
            disk("NVMe", vec![partition("D")]),
            disk("SATA", vec![partition("H")]),
            boot,
            disk("USB", vec![efi]),
            disk("USB", vec![mbr_efi]),
        ]);

        assert!(media.is_empty(), "{media:?}");
    }

    #[test]
    fn the_powershell_output_parses() {
        let json = r#"[{"Number":2,"FriendlyName":"Kingston DataTraveler","BusType":"USB","IsBoot":false,"IsSystem":false,"IsOffline":false,"IsReadOnly":false,"Partitions":[{"Letter":"E","GptType":"","MbrType":7,"IsSystem":false,"IsBoot":false,"FileSystem":"FAT32","Size":15997071360,"Free":15996502016}]}]"#;
        let disks: Vec<Disk> = serde_json::from_str(json).unwrap();

        let media = usable(disks);
        assert_eq!(media[0].label, "Kingston DataTraveler");
        assert!(!media[0].is_exfat());
    }

    #[test]
    fn encoding_matches_powershell() {
        // [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes("exit 0"))
        assert_eq!(encode("exit 0"), "ZQB4AGkAdAAgADAA");
    }
}
