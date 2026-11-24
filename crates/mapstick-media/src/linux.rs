// SPDX-FileCopyrightText: 2026 Mikołaj
// SPDX-License-Identifier: GPL-3.0-only

//! Backend Linuksa oparty na udisks2. Formatowanie idzie przez polkit, więc
//! użytkownik dostaje systemowe okno z pytaniem o hasło zamiast `sudo`.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use zbus::{
    Connection,
    fdo::ObjectManagerProxy,
    zvariant::{OwnedObjectPath, OwnedValue},
};

use crate::{DriveFacts, Error, Medium, Result, Space, check_drive, is_efi_partition_type};

const SERVICE: &str = "org.freedesktop.UDisks2";
const ROOT: &str = "/org/freedesktop/UDisks2";
const BLOCK: &str = "org.freedesktop.UDisks2.Block";
const FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const PARTITION: &str = "org.freedesktop.UDisks2.Partition";

type Props = HashMap<String, OwnedValue>;

#[zbus::proxy(
    interface = "org.freedesktop.UDisks2.Block",
    default_service = "org.freedesktop.UDisks2"
)]
trait Block {
    fn format(
        &self,
        type_: &str,
        options: HashMap<&str, &zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.UDisks2.Filesystem",
    default_service = "org.freedesktop.UDisks2"
)]
trait Filesystem {
    fn mount(&self, options: HashMap<&str, &zbus::zvariant::Value<'_>>) -> zbus::Result<String>;
    fn unmount(&self, options: HashMap<&str, &zbus::zvariant::Value<'_>>) -> zbus::Result<()>;
}

/// Błąd, który usługa odesłała razem z uzasadnieniem, przekazujemy dalej
/// w całości. Nazwa błędu D-Bus (`org.freedesktop.UDisks2.Error.Failed`)
/// nikomu nic nie mówi, ale treść — owszem.
fn service<E: Into<zbus::Error>>(e: E) -> Error {
    match e.into() {
        // Zamknięte okno polkit — decyzja użytkownika, nie awaria usługi.
        zbus::Error::MethodError(name, _, _)
            if name.as_str().ends_with(".Error.NotAuthorizedDismissed") =>
        {
            Error::Cancelled
        }
        zbus::Error::MethodError(_, Some(message), _) => Error::Service(message),
        other => backend(other),
    }
}

/// Ścieżka obiektu udisks2 odpowiadająca urządzeniu blokowemu.
fn block_path(id: &str) -> Result<OwnedObjectPath> {
    OwnedObjectPath::try_from(format!(
        "/org/freedesktop/UDisks2/block_devices/{}",
        id.rsplit('/')
            .next()
            .unwrap_or_default()
            .replace('-', "_2d")
    ))
    .map_err(backend)
}

fn backend<E>(e: E) -> Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    Error::Backend(Box::new(e))
}

fn prop<T: TryFrom<OwnedValue>>(props: &Props, key: &str) -> Option<T> {
    props.get(key).cloned().and_then(|v| T::try_from(v).ok())
}

/// udisks2 podaje ścieżki jako tablice bajtów zakończone zerem.
fn c_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());

    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn mount_points(props: &Props) -> Vec<PathBuf> {
    prop::<Vec<Vec<u8>>>(props, "MountPoints")
        .unwrap_or_default()
        .iter()
        .map(|b| PathBuf::from(c_string(b)))
        .collect()
}

fn drive_label(props: &Props) -> String {
    let vendor = prop::<String>(props, "Vendor").unwrap_or_default();
    let model = prop::<String>(props, "Model").unwrap_or_default();

    match (vendor.trim(), model.trim()) {
        // Podpis zastępczy dokłada interfejs, w swoim języku.
        ("", "") => String::new(),
        ("", m) => m.to_owned(),
        (v, "") => v.to_owned(),
        (v, m) => format!("{v} {m}"),
    }
}

pub fn space_for(path: &Path) -> Result<Space> {
    let stat = rustix::fs::statvfs(path).map_err(|e| Error::Space(path.to_owned(), e.into()))?;

    let unit = stat.f_frsize;

    Ok(Space {
        total: stat.f_blocks.saturating_mul(unit),
        free: stat.f_bavail.saturating_mul(unit),
    })
}

pub async fn list_media() -> Result<Vec<Medium>> {
    let connection = Connection::system().await.map_err(backend)?;
    let manager = ObjectManagerProxy::builder(&connection)
        .destination(SERVICE)
        .map_err(backend)?
        .path(ROOT)
        .map_err(backend)?
        .build()
        .await
        .map_err(backend)?;

    let objects = manager.get_managed_objects().await.map_err(backend)?;

    // Napędy trzymamy osobno: decyzja o bezpieczeństwie zapada na poziomie
    // całego napędu, a nie pojedynczej partycji.
    let mut drives: HashMap<String, &Props> = HashMap::new();
    for (path, interfaces) in &objects {
        if let Some(props) = interfaces.get(DRIVE) {
            drives.insert(path.as_str().to_owned(), props);
        }
    }

    // Fakty zbieramy dla całego napędu naraz: pojedyncza partycja nie wie
    // o tym, że sąsiednia trzyma rozruch systemu.
    let mut facts: HashMap<String, DriveFacts> = HashMap::new();
    for interfaces in objects.values() {
        let Some(block) = interfaces.get(BLOCK) else {
            continue;
        };
        let Some(drive) = prop::<OwnedObjectPath>(block, "Drive") else {
            continue;
        };

        let entry = facts.entry(drive.as_str().to_owned()).or_default();

        if let Some(fs) = interfaces.get(FILESYSTEM) {
            entry.mount_points.extend(mount_points(fs));
        }

        entry.has_system_hint |= prop::<bool>(block, "HintSystem").unwrap_or(false);

        if let Some(partition) = interfaces.get(PARTITION)
            && let Some(kind) = prop::<String>(partition, "Type")
        {
            entry.has_efi_partition |= is_efi_partition_type(&kind);
        }
    }

    for (path, props) in &drives {
        if let Some(entry) = facts.get_mut(path.as_str()) {
            entry.connection = prop::<String>(props, "ConnectionBus");
        }
    }

    let mut media = Vec::new();

    for interfaces in objects.values() {
        let (Some(block), Some(fs)) = (interfaces.get(BLOCK), interfaces.get(FILESYSTEM)) else {
            continue;
        };

        let Some(drive_path) = prop::<OwnedObjectPath>(block, "Drive") else {
            continue;
        };
        let drive_path = drive_path.as_str().to_owned();

        // Urządzenia pętlowe i podobne nie mają prawdziwego napędu.
        let Some(drive_props) = drives.get(drive_path.as_str()) else {
            continue;
        };

        if prop::<bool>(block, "ReadOnly").unwrap_or(false) {
            continue;
        }

        let Some(drive_facts) = facts.get(&drive_path) else {
            continue;
        };
        if let Err(reason) = check_drive(drive_facts) {
            tracing::debug!("Skipping drive {drive_path}: {}", reason.reason());
            continue;
        }

        let device = prop::<Vec<u8>>(block, "Device")
            .map(|b| c_string(&b))
            .unwrap_or_else(|| drive_path.clone());

        let mount_point = mount_points(fs).into_iter().next();
        let filesystem = prop::<String>(block, "IdType").filter(|s| !s.is_empty());
        let total_declared = prop::<u64>(block, "Size").unwrap_or(0);

        // Wolne miejsce zna tylko zamontowany system plików.
        let (total_bytes, free_bytes) = match &mount_point {
            Some(path) => match space_for(path) {
                Ok(space) => (space.total, space.free),
                Err(e) => {
                    tracing::warn!("Could not check the free space on {device}: {e}");
                    (total_declared, 0)
                }
            },
            None => (total_declared, 0),
        };

        media.push(Medium {
            id: device,
            drive_id: drive_path,
            label: drive_label(drive_props),
            mount_point,
            filesystem,
            total_bytes,
            free_bytes,
            connection: prop::<String>(drive_props, "ConnectionBus").filter(|s| !s.is_empty()),
        });
    }

    media.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(media)
}

/// Formatuje wolumen do exFAT. **Kasuje wszystkie dane na nim.**
///
/// Sprawdzenie bezpieczeństwa powtarzamy tuż przed wywołaniem, na świeżo
/// odczytanym stanie — lista w interfejsie mogła się zestarzeć, a użytkownik
/// mógł w międzyczasie zamontować coś systemowego.
pub async fn format_exfat(medium: &Medium, label: &str) -> Result<()> {
    let current = list_media().await?;
    if !current.iter().any(|m| m.id == medium.id) {
        return Err(Error::SystemDisk(medium.id.clone()));
    }

    let connection = Connection::system().await.map_err(backend)?;
    let path = block_path(&medium.id)?;

    let filesystem = FilesystemProxy::builder(&connection)
        .path(path.clone())
        .map_err(backend)?
        .build()
        .await
        .map_err(backend)?;

    // udisks2 nie odmontowuje przed formatowaniem i nie robi tego za nas nikt
    // inny: na zamontowanym wolumenie `Format` kończy się „Failed to open the
    // device: Device or resource busy”.
    unmount(&filesystem).await?;

    let proxy = BlockProxy::builder(&connection)
        .path(path)
        .map_err(backend)?
        .build()
        .await
        .map_err(backend)?;

    let label = zbus::zvariant::Value::from(label);
    let update = zbus::zvariant::Value::from(true);
    let options = HashMap::from([("label", &label), ("update-partition-type", &update)]);

    proxy.format("exfat", options).await.map_err(service)?;

    remount(&filesystem).await;

    Ok(())
}

/// Montuje nośnik, którego nie zamontował pulpit, i zwraca punkt montowania.
///
/// Na lekkich pulpitach (sam menedżer okien, bez automontowania) podłączony
/// pendrive stoi niezamontowany, a bez tego jedyną radą było „zamontuj go
/// sam”. udisks2 montuje nośniki wymienne zalogowanemu użytkownikowi bez
/// hasła — tak samo, jak robi to menedżer plików. Montujemy wyłącznie to, co
/// przeszło sprawdzenie bezpieczeństwa na świeżej liście.
pub async fn mount(medium: &Medium) -> Result<PathBuf> {
    let current = list_media().await?;
    let Some(fresh) = current.iter().find(|m| m.id == medium.id) else {
        return Err(Error::SystemDisk(medium.id.clone()));
    };

    if let Some(point) = &fresh.mount_point {
        return Ok(point.clone());
    }

    let connection = Connection::system().await.map_err(backend)?;
    let filesystem = FilesystemProxy::builder(&connection)
        .path(block_path(&medium.id)?)
        .map_err(backend)?
        .build()
        .await
        .map_err(backend)?;

    let point = filesystem.mount(HashMap::new()).await.map_err(service)?;
    tracing::info!("Mounted {} at {point}", medium.id);

    Ok(PathBuf::from(point))
}

/// Odmontowuje, godząc się z tym, że nośnik mógł nie być zamontowany.
async fn unmount(filesystem: &FilesystemProxy<'_>) -> Result<()> {
    match filesystem.unmount(HashMap::new()).await {
        Ok(()) => Ok(()),
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str().ends_with(".Error.NotMounted") =>
        {
            Ok(())
        }
        Err(e) => Err(service(e)),
    }
}

/// Montuje świeżo sformatowany nośnik z powrotem.
///
/// Bez tego zostawiamy użytkownika z pustym, niezamontowanym urządzeniem
/// i jedyną radą „wypnij i wepnij”. Nie jest to jednak powód, żeby uznać samo
/// formatowanie za nieudane — stąd ostrzeżenie zamiast błędu.
///
/// Interfejs systemu plików powstaje na nowo dopiero po tym, jak udisks2
/// zauważy zmianę, więc pierwsza próba potrafi trafić w pustkę.
async fn remount(filesystem: &FilesystemProxy<'_>) {
    for attempt in 0..5 {
        match filesystem.mount(HashMap::new()).await {
            Ok(point) => {
                tracing::info!("Medium mounted at {point}");
                return;
            }
            Err(e) => {
                tracing::debug!("Mounting after formatting, attempt {}: {e}", attempt + 1);
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
        }
    }

    tracing::warn!("Medium formatted but not mounted");
}
