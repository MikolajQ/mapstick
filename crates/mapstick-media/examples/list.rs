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
        println!("Brak nośników nadających się na cel.");
        return Ok(());
    }

    for m in &media {
        println!("{} — {}", m.id, m.label);
        println!("  magistrala:   {}", m.connection.as_deref().unwrap_or("?"));
        println!(
            "  system plików:{} {}",
            m.filesystem.as_deref().unwrap_or("brak"),
            if m.is_exfat() {
                "(exFAT OK)"
            } else {
                "(wymaga formatowania)"
            }
        );
        println!(
            "  zamontowany:  {}",
            m.mount_point
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or("nie".into())
        );
        println!(
            "  pojemność:    {} B, wolne {} B",
            m.total_bytes, m.free_bytes
        );
        println!("  zmieści 25 GiB? {}", m.fits(27_377_719_742));
    }

    Ok(())
}
