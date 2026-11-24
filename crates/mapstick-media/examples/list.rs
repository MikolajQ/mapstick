//! Wypisuje nośniki, które MapStick uznaje za dopuszczalny cel zapisu.
//!
//! Uruchomienie na maszynie z systemem na dysku USB jest najlepszym testem
//! reguły bezpieczeństwa: dysk systemowy nie może się tu pojawić.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let runtime = tokio::runtime::Runtime::new()?;
    let media = runtime.block_on(mapstick_media::list_media())?;

    if media.is_empty() {
        println!("No media fit to be a target.");
        return Ok(());
    }

    for m in &media {
        println!("{} — {}", m.id, m.label);
        println!("  bus:          {}", m.connection.as_deref().unwrap_or("?"));
        println!(
            "  file system:  {} {}",
            m.filesystem.as_deref().unwrap_or("none"),
            if m.is_exfat() {
                "(exFAT OK)"
            } else {
                "(needs formatting)"
            }
        );
        println!(
            "  mounted:      {}",
            m.mount_point
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or("no".into())
        );
        println!(
            "  capacity:     {} B, {} B free",
            m.total_bytes, m.free_bytes
        );
        println!("  fits 25 GiB? {}", m.fits(27_377_719_742));

        if let Some(mount) = &m.mount_point {
            match mapstick_media::read_installed(mount) {
                Ok(Some(i)) => println!(
                    "  firmware found: {} {} (brand {})",
                    i.model_id, i.firmware_version, i.brand
                ),
                Ok(None) => println!("  firmware found: no .ver file"),
                Err(e) => println!("  firmware found: read error — {e}"),
            }
        }
    }

    Ok(())
}
