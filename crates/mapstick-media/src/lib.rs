// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! Wykrywanie nośników wymiennych, sprawdzanie miejsca i formatowanie do exFAT.
//!
//! Cała wiedza o systemie operacyjnym siedzi w tej skrzynce; reszta MapSticka
//! widzi wyłącznie [`Medium`] i [`Space`].

use std::path::{Path, PathBuf};

use nudl::version::VersionInfo;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as backend;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Nie udało się odczytać katalogu {0:?}")]
    ReadDir(PathBuf, #[source] std::io::Error),
    #[error("Nie udało się odczytać pliku {0:?}")]
    ReadFile(PathBuf, #[source] std::io::Error),
    #[error("Katalog {0:?} zawiera {1} plików .ver zamiast jednego")]
    VerFileCount(PathBuf, usize),
    #[error("Niepoprawny plik .ver: {0:?}")]
    InvalidVerFile(PathBuf, #[source] nudl::version::Error),
    #[error("Nie udało się sprawdzić miejsca w {0:?}")]
    Space(PathBuf, #[source] std::io::Error),
    #[error("Nośnik {0} nie jest zamontowany")]
    NotMounted(String),
    #[error("Odmowa: {0} zawiera dane systemu i nie może być celem")]
    SystemDisk(String),
    #[error("Błąd komunikacji z usługą dysków")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("Ta platforma nie jest jeszcze obsługiwana")]
    Unsupported,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Katalogi, w których system montuje nośniki użytkownika. Cokolwiek
/// zamontowanego poza nimi uznajemy za część systemu.
const USER_MEDIA_PREFIXES: &[&str] = &["/run/media/", "/media/", "/mnt/"];

/// Zapas doliczany do wymaganego miejsca. Serwis producenta bywa nierzetelny
/// w podawaniu rozmiarów, a exFAT potrzebuje miejsca na metadane.
pub const SPACE_MARGIN_PERCENT: u64 = 15;

/// Nośnik, na który wolno pisać.
#[derive(Debug, Clone)]
pub struct Medium {
    /// Identyfikator wolumenu — na Linuksie ścieżka urządzenia partycji.
    pub id: String,
    /// Identyfikator całego napędu, do którego należy wolumen.
    pub drive_id: String,
    /// Nazwa czytelna dla człowieka, np. „SanDisk Ultra”.
    pub label: String,
    pub mount_point: Option<PathBuf>,
    pub filesystem: Option<String>,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// Magistrala, np. `usb`. Wyłącznie do pokazania użytkownikowi.
    pub connection: Option<String>,
}

impl Medium {
    /// Producent wymaga exFAT-u; FAT32 dodatkowo nie udźwignąłby plików
    /// większych niż 4 GiB.
    pub fn is_exfat(&self) -> bool {
        self.filesystem
            .as_deref()
            .is_some_and(|fs| fs.eq_ignore_ascii_case("exfat"))
    }

    /// Czy zmieści się `needed` bajtów wraz z zapasem.
    pub fn fits(&self, needed: u64) -> bool {
        self.free_bytes >= with_margin(needed)
    }
}

/// Powiększa wymagany rozmiar o [`SPACE_MARGIN_PERCENT`].
pub fn with_margin(bytes: u64) -> u64 {
    bytes.saturating_add(bytes / 100 * SPACE_MARGIN_PERCENT)
}

/// Miejsce w dowolnym katalogu — również na dysku wewnętrznym, gdzie ląduje
/// pobranie przed przeniesieniem na nośnik.
#[derive(Debug, Clone, Copy)]
pub struct Space {
    pub total: u64,
    pub free: u64,
}

/// Oprogramowanie znalezione na nośniku albo w katalogu pobrań.
///
/// To główne źródło odpowiedzi na pytanie „co jest w aucie”: pendrive, którym
/// użytkownik ostatnio aktualizował nawigację, niesie plik `.ver` z modelem
/// i wersją.
#[derive(Debug, Clone)]
pub struct Installed {
    pub model_id: String,
    pub firmware_version: String,
    pub brand: String,
}

/// Czy ścieżka montowania należy do nośników użytkownika.
fn is_user_media_mount(path: &Path) -> bool {
    let Some(text) = path.to_str() else {
        return false;
    };

    USER_MEDIA_PREFIXES.iter().any(|p| text.starts_with(p))
}

/// GUID partycji systemowej EFI w tablicy GPT.
const EFI_PARTITION_GUID: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";
/// Odpowiednik w tablicy MBR.
const EFI_PARTITION_MBR: &str = "0xef";

/// Fakty o napędzie, na podstawie których zapada decyzja o dopuszczeniu go.
#[derive(Debug, Default, Clone)]
pub struct DriveFacts {
    /// Magistrala z udisks2: `usb`, `sdio`… Puste znaczy „wewnętrzny”.
    pub connection: Option<String>,
    /// Punkty montowania **wszystkich** partycji napędu.
    pub mount_points: Vec<PathBuf>,
    /// Czy którakolwiek partycja jest oznaczona przez system jako systemowa.
    pub has_system_hint: bool,
    /// Czy napęd zawiera partycję EFI — czyli czy ktoś z niego startuje.
    pub has_efi_partition: bool,
}

/// Powód odrzucenia napędu. Trafia do logu, żeby dało się zrozumieć, czemu
/// czegoś nie ma na liście.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// Napęd wewnętrzny — nie jest nośnikiem, który zanosi się do auta.
    Internal,
    /// Któraś partycja trzyma dane systemu.
    SystemMount,
    /// System oznaczył napęd jako swój.
    SystemHint,
    /// Napęd zawiera partycję rozruchową EFI.
    EfiPartition,
}

impl Rejection {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Internal => "napęd wewnętrzny",
            Self::SystemMount => "zamontowany poza katalogami nośników użytkownika",
            Self::SystemHint => "oznaczony przez system jako systemowy",
            Self::EfiPartition => "zawiera partycję rozruchową EFI",
        }
    }
}

/// Czy napęd wolno zaproponować jako cel zapisu.
///
/// Cztery niezależne warunki, każdy wystarczy do odrzucenia. Taka nadmiarowość
/// bierze się stąd, że każdy pojedynczy sygnał okazał się zawodny na maszynie
/// deweloperskiej:
///
/// * **Flaga „wymienny” kłamie.** udisks2 zgłasza `Removable = true` dla dysku,
///   na którym leżą `/`, `/home` i `/boot`, bo system stoi na SSD podpiętym po
///   USB. Dlatego flagi nie używamy w ogóle.
/// * **Samo sprawdzenie montowania nie wystarcza.** Wewnętrzny dysk z Windowsem
///   nie jest zamontowany nigdzie, więc przeszedłby regułę montowania i dałoby
///   się go zaproponować do sformatowania.
///
/// Dopiero razem te warunki odsiewają oba przypadki: dysk wewnętrzny odpada na
/// magistrali, a systemowy dysk USB na montowaniu.
pub fn check_drive(facts: &DriveFacts) -> Result<(), Rejection> {
    // Nośnik do auta jest z definicji podpięty z zewnątrz.
    if facts
        .connection
        .as_deref()
        .is_none_or(|c| c.trim().is_empty())
    {
        return Err(Rejection::Internal);
    }

    if facts.has_system_hint {
        return Err(Rejection::SystemHint);
    }

    if facts.has_efi_partition {
        return Err(Rejection::EfiPartition);
    }

    if !facts.mount_points.iter().all(|m| is_user_media_mount(m)) {
        return Err(Rejection::SystemMount);
    }

    Ok(())
}

/// Czy typ partycji oznacza partycję rozruchową EFI.
pub fn is_efi_partition_type(partition_type: &str) -> bool {
    let t = partition_type.trim().to_ascii_lowercase();

    t == EFI_PARTITION_GUID || t == EFI_PARTITION_MBR
}

/// Lista nośników, na które wolno pisać. Napędy systemowe nie trafiają tu
/// w ogóle — są odfiltrowane, a nie tylko ukryte w interfejsie.
pub async fn list_media() -> Result<Vec<Medium>> {
    #[cfg(target_os = "linux")]
    {
        backend::list_media().await
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(Error::Unsupported)
    }
}

/// Formatuje nośnik do exFAT. **Kasuje wszystkie dane na nim.**
///
/// Wywołanie jest bezpieczne o tyle, że backend powtarza sprawdzenie
/// bezpieczeństwa na świeżo odczytanym stanie systemu. Zgoda użytkownika to
/// jednak sprawa warstwy wyżej — ta funkcja niczego nie pyta.
pub async fn format_exfat(medium: &Medium, label: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        backend::format_exfat(medium, label).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (medium, label);
        Err(Error::Unsupported)
    }
}

/// Wolne i całkowite miejsce w podanym katalogu.
pub fn space_for(path: &Path) -> Result<Space> {
    #[cfg(target_os = "linux")]
    {
        backend::space_for(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Err(Error::Unsupported)
    }
}

/// Odczytuje plik `.ver` z katalogu. `Ok(None)` znaczy „nie ma czego czytać”,
/// a nie „coś poszło źle”.
pub fn read_installed(directory: &Path) -> Result<Option<Installed>> {
    let entries =
        std::fs::read_dir(directory).map_err(|e| Error::ReadDir(directory.to_owned(), e))?;

    let mut ver_files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::ReadDir(directory.to_owned(), e))?;
        let path = entry.path();

        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("ver"))
        {
            ver_files.push(path);
        }
    }

    match ver_files.len() {
        0 => return Ok(None),
        1 => {}
        n => return Err(Error::VerFileCount(directory.to_owned(), n)),
    }

    let path = &ver_files[0];
    let text = std::fs::read_to_string(path).map_err(|e| Error::ReadFile(path.clone(), e))?;
    let info: VersionInfo = text
        .parse()
        .map_err(|e| Error::InvalidVerFile(path.clone(), e))?;

    Ok(Some(Installed {
        model_id: info.header.id,
        firmware_version: info.header.firmware_version,
        brand: info.header.brand.as_code_str().to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb() -> DriveFacts {
        DriveFacts {
            connection: Some("usb".to_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn plain_pendrive_is_accepted() {
        let mut facts = usb();
        facts.mount_points = vec![PathBuf::from("/run/media/miq/MAPSTICK")];

        assert_eq!(check_drive(&facts), Ok(()));
    }

    #[test]
    fn unmounted_pendrive_is_accepted() {
        assert_eq!(check_drive(&usb()), Ok(()));
    }

    /// Dysk systemowy tej maszyny: SSD po USB, na którym stoi cały system.
    /// udisks2 zgłasza go jako wymienny.
    #[test]
    fn usb_attached_system_disk_is_rejected() {
        let mut facts = usb();
        facts.mount_points = vec![
            PathBuf::from("/run/media/miq/fc0ed498"),
            PathBuf::from("/boot"),
            PathBuf::from("/"),
        ];

        assert_eq!(check_drive(&facts), Err(Rejection::SystemMount));
    }

    /// Wewnętrzny dysk z Windowsem: niezamontowany, więc sama reguła
    /// montowania by go przepuściła.
    #[test]
    fn internal_disk_is_rejected_even_when_unmounted() {
        let facts = DriveFacts {
            connection: None,
            ..Default::default()
        };

        assert_eq!(check_drive(&facts), Err(Rejection::Internal));
    }

    /// Zewnętrzny dysk, z którego ktoś startuje system.
    #[test]
    fn external_bootable_disk_is_rejected() {
        let mut facts = usb();
        facts.has_efi_partition = true;

        assert_eq!(check_drive(&facts), Err(Rejection::EfiPartition));
    }

    #[test]
    fn system_hint_alone_rejects() {
        let mut facts = usb();
        facts.has_system_hint = true;

        assert_eq!(check_drive(&facts), Err(Rejection::SystemHint));
    }

    #[test]
    fn efi_partition_types_are_recognised() {
        assert!(is_efi_partition_type(
            "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
        ));
        assert!(is_efi_partition_type("0xef"));
        assert!(!is_efi_partition_type(
            "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7"
        ));
    }

    #[test]
    fn margin_is_added_to_required_space() {
        assert_eq!(with_margin(100), 115);
        assert_eq!(with_margin(0), 0);
    }
}
