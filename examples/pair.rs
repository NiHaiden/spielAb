//! Interactive pairing smoke test, sharing the GUI's exact protocol implementation.
use airplay_discovery::{Discovery, ServiceBrowser};
use anyhow::Context;
use spielab::pairing::{ControlSession, CredentialStore};
use std::{
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let name = std::env::args()
        .nth(1)
        .context("Usage: cargo run --example pair -- 'Receiver name'")?;
    let state = std::env::var_os("SPIELAB_STATE_DIR")
        .map(PathBuf::from)
        .context("Set SPIELAB_STATE_DIR to the same credential directory as the GUI")?;
    let store = CredentialStore::new(state)?;
    let browser = ServiceBrowser::new()?;
    let device = browser
        .scan(Duration::from_secs(5))
        .await?
        .into_iter()
        .find(|device| device.name == name)
        .context("Receiver not found")?;
    let mut session = ControlSession::open(device).await?;
    if let Some(credentials) = store.load(&session.device)? {
        session.verify(&credentials).await?;
    } else {
        session.request_pin().await?;
        print!("Enter the code shown on Apple TV: ");
        io::stdout().flush()?;
        let mut pin = String::new();
        io::stdin().read_line(&mut pin)?;
        let credentials = session.pair(pin.trim()).await?;
        store.save(&session.device, &credentials)?;
    }
    println!("Authenticated: encrypted OPTIONS round trip succeeded.");
    session.transport.close().await?;
    Ok(())
}
