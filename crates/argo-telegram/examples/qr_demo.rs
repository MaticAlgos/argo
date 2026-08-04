//! Prints a scannable QR for a link, used to eyeball terminal rendering.
fn main() {
    let link = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://t.me/BotFather".into());
    match argo_telegram::qr::render(&link) {
        Some(rows) => rows.iter().for_each(|row| println!("{row}")),
        None => println!("payload too large"),
    }
}
