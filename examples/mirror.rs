//! Run the real mirroring path. --test uses a moving test card in place of a desktop.
use airplay_discovery::{Discovery, ServiceBrowser};
use anyhow::Context;
use spielab::{
    capture::ScreenCapture,
    media::{AudioCapture, VideoCapture, VideoEncoder},
    mirror::MirrorSession,
    pairing::{ControlSession, CredentialStore},
    settings::{CaptureMode, LatencyMode, VideoQuality},
};
use std::{path::PathBuf, time::Duration};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let name = args.get(1).context(
        "Usage: mirror 'Receiver name' [--test | --extend] [--quality=720p30|1080p30|1080p60] [--latency=50|100|500]",
    )?;
    let state = std::env::var_os("SPIELAB_STATE_DIR")
        .map(PathBuf::from)
        .context("Set SPIELAB_STATE_DIR")?;
    let store = CredentialStore::new(state)?;
    let browser = ServiceBrowser::new()?;
    let device = browser
        .scan(Duration::from_secs(5))
        .await?
        .into_iter()
        .find(|d| &d.name == name)
        .context("Receiver not found")?;
    let credentials = store.load(&device)?.context("Pair this receiver first")?;
    anyhow::ensure!(
        !(args.iter().any(|a| a == "--test") && args.iter().any(|a| a == "--extend")),
        "Use --test or --extend, not both"
    );
    let quality = match args.iter().find_map(|arg| arg.strip_prefix("--quality=")) {
        None | Some("1080p30") => VideoQuality::High,
        Some("720p30") => VideoQuality::Efficient,
        Some("1080p60") => VideoQuality::Smooth,
        Some(value) => anyhow::bail!("Unknown quality: {value}"),
    };
    let config = quality.config();
    let encoder = VideoEncoder::detect(config).await;
    println!("Encoder: {}", encoder.label());
    let latency = match args.iter().find_map(|arg| arg.strip_prefix("--latency=")) {
        None | Some("50") => LatencyMode::Interactive,
        Some("100") => LatencyMode::Responsive,
        Some("500") => LatencyMode::Buffered,
        Some(value) => anyhow::bail!("Unknown latency: {value}"),
    };
    let mode = if args.iter().any(|a| a == "--extend") {
        CaptureMode::Extend
    } else {
        CaptureMode::Mirror
    };
    let screen = if args.iter().any(|a| a == "--test") {
        None
    } else {
        Some(ScreenCapture::select_mode(mode).await?)
    };
    println!("Capture: {} · {}", mode.label(), quality.label());
    let mut control = ControlSession::open(device).await?;
    control.verify(&credentials).await?;
    println!("Pair verification succeeded; setting up mirroring");
    let result = async {
        let mut mirror =
            MirrorSession::setup_with_latency(&mut control, config.width, config.height, latency)
                .await?;
        let video = VideoCapture::start_with_encoder(screen.as_ref(), config, &encoder)?;
        let audio = AudioCapture::start()?;
        println!("Video and screen-audio SETUP accepted");
        let duration = std::env::var("SPIELAB_TEST_SECONDS")
            .unwrap_or("30".into())
            .parse::<u64>()?;
        match tokio::time::timeout(
            Duration::from_secs(duration),
            mirror.run(&mut control, video, audio, |frames| {
                println!("Sent {frames} video frames")
            }),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Ok(()),
        }
    }
    .await;
    let _ = control.stop_mirroring().await;
    let _ = control.transport.close().await;
    if let Some(screen) = screen {
        let _ = screen.close().await;
    }
    result
}
