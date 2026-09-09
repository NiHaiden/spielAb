use spielab::{
    media::{VideoCapture, VideoEncoder},
    settings::{VideoConfig, VideoQuality},
};
use std::{process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

/// Exercises the actual encoder, Annex-B reader and an independent H.264 decoder.
#[tokio::test]
#[ignore = "requires FFmpeg with libx264 installed"]
async fn encoded_access_units_decode_to_three_video_frames() {
    decode_frames(VideoConfig {
        width: 320,
        height: 240,
        ..VideoQuality::Efficient.config()
    })
    .await;
}

#[tokio::test]
#[ignore = "requires FFmpeg with libx264 installed"]
async fn every_quality_preset_encodes_the_requested_resolution_and_frame_rate() {
    for quality in VideoQuality::ALL {
        decode_frames(quality.config()).await;
    }
}

async fn decode_frames(config: VideoConfig) {
    decode_with_encoder(config, &VideoEncoder::Software, 3).await;
}

#[tokio::test]
#[ignore = "requires a working VA-API H.264 encoder on /dev/dri/renderD128"]
async fn hardware_frames_decode_at_every_quality_and_auto_detection_selects_gpu() {
    let encoder = VideoEncoder::Vaapi("/dev/dri/renderD128".into());
    for quality in VideoQuality::ALL {
        encoder.probe(quality.config()).await.unwrap();
        decode_with_encoder(
            quality.config(),
            &encoder,
            quality.config().fps as usize + 2,
        )
        .await;
    }
    assert!(matches!(
        VideoEncoder::detect(VideoQuality::High.config()).await,
        VideoEncoder::Vaapi(_)
    ));
}

async fn decode_with_encoder(config: VideoConfig, encoder: &VideoEncoder, count: usize) {
    let mut capture = VideoCapture::start_with_encoder(None, config, encoder).unwrap();
    let mut stream = vec![];
    for index in 0..count {
        let nals = tokio::time::timeout(Duration::from_secs(5), capture.frame())
            .await
            .unwrap()
            .unwrap();
        if index == 0 {
            assert!(nals.iter().any(|n| n[0] & 31 == 7));
            assert!(nals.iter().any(|n| n[0] & 31 == 8));
        }
        for nal in nals {
            stream.extend_from_slice(&[0, 0, 0, 1]);
            stream.extend_from_slice(&nal);
        }
    }
    drop(capture);
    let mut decoder = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "h264",
            "-i",
            "pipe:0",
            "-f",
            "framehash",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    decoder
        .stdin
        .take()
        .unwrap()
        .write_all(&stream)
        .await
        .unwrap();
    let output = tokio::time::timeout(Duration::from_secs(5), decoder.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let decoded = String::from_utf8(output.stdout).unwrap();
    assert!(
        decoded.contains(&format!(
            "#dimensions 0: {}x{}",
            config.width, config.height
        )),
        "{decoded}"
    );
    assert!(
        decoded.contains(&format!("#tb 0: 1/{}", config.fps)),
        "{decoded}"
    );
    assert_eq!(
        decoded
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
            .count(),
        count
    );
}
