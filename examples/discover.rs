use airplay_discovery::{Discovery, ServiceBrowser};
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let browser = ServiceBrowser::new()?;
    let devices = browser.scan(Duration::from_secs(6)).await?;
    println!("Found {} AirPlay receivers", devices.len());
    for device in devices {
        println!(
            "{} | {} | {:?} | {}",
            device.name,
            device.model,
            device.socket_addr(),
            device.id.to_mac_string()
        );
    }
    Ok(())
}
