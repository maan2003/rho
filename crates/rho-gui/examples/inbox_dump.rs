//! Prints the client-local inbox, for QA runs that need to see what an
//! external source raised without opening the GUI a second time.

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "inbox.redb".to_owned());
    let store = rho_gui::inbox::InboxStore::open(path)?;
    for item in store.items() {
        println!(
            "{:?} | {} | waiting_on={:?} | room={:?} | {:?}",
            item.kind, item.text, item.waiting_on, item.context.room, item.source
        );
    }
    println!("{} item(s)", store.items().len());
    Ok(())
}
