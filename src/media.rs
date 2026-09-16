//! PipeWire capture and low-latency H.264 encoding. Commands use argv, never a shell.
use crate::capture::ScreenCapture;
use crate::settings::{VideoConfig, VideoQuality};
use anyhow::{Context, Result, ensure};
use std::{
    ops::Range,
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::Stdio,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::{Child, ChildStdout, Command},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VideoEncoder {
    Software,
    Vaapi(std::path::PathBuf),
}

impl VideoEncoder {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Software => "Software H.264",
            Self::Vaapi(_) => "GPU H.264 · VA-API",
        }
    }

    /// Test real encoded access units, not just FFmpeg's compiled encoder list.
    pub async fn probe(&self, config: VideoConfig) -> Result<()> {
        let mut capture = VideoCapture::start_with_encoder(None, config, self)?;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            for index in 0..3 {
                let frame = capture.frame_packet().await?;
                ensure!(
                    frame.iter().any(|n| matches!(n[0] & 31, 1 | 5)),
                    "Encoder returned no picture"
                );
                if index == 0 {
                    for kind in [7, 8, 5] {
                        ensure!(
                            frame.iter().any(|n| n[0] & 31 == kind),
                            "Encoder omitted initial SPS, PPS or keyframe"
                        );
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("Hardware encoder probe timed out")?
    }

    pub async fn detect(config: VideoConfig) -> Self {
        let mut devices: Vec<_> = std::fs::read_dir("/dev/dri")
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("renderD"))
            .map(|entry| entry.path())
            .collect();
        devices.sort();
        Self::select_from(devices, config).await
    }

    async fn select_from(devices: Vec<std::path::PathBuf>, config: VideoConfig) -> Self {
        for device in devices {
            let encoder = Self::Vaapi(device);
            match encoder.probe(config).await {
                Ok(()) => return encoder,
                Err(error) => eprintln!("Hardware encoder unavailable ({encoder:?}): {error:#}"),
            }
        }
        Self::Software
    }
}

// An abrupt app exit must not leave a PipeWire screen/audio capture running.
// kill_on_drop covers Rust cleanup; Linux's parent-death signal covers crashes
// and SIGTERM too. Check the parent again after prctl to close the fork race.
fn terminate_with_parent(command: &mut std::process::Command) {
    #[cfg(target_os = "linux")]
    {
        let parent = std::process::id() as libc::pid_t;
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(std::io::Error::other("Capture owner exited during startup"));
                }
                Ok(())
            });
        }
    }
}

pub struct VideoCapture {
    encoder: Child,
    source: Option<std::process::Child>,
    output: ChildStdout,
    packet_sizes: BufReader<tokio::net::UnixStream>,
    packet_metadata: String,
    pub width: u32,
    pub height: u32,
}

/// Keep the encoder packet intact while exposing its NAL units without copying.
pub(crate) struct VideoFrame {
    packet: Vec<u8>,
    nals: Vec<Range<usize>>,
}

impl VideoFrame {
    fn from_packet(packet: Vec<u8>) -> Result<Self> {
        let mut cursor = start_code(&packet, 0).context("Encoder returned non-Annex-B video")?;
        ensure!(cursor.0 == 0, "Unexpected data before H.264 start code");
        let mut nals = Vec::new();
        loop {
            let begin = cursor.0 + cursor.1;
            let next = start_code(&packet, begin);
            let end = next.map_or(packet.len(), |(offset, _)| offset);
            ensure!(end > begin, "Empty H.264 NAL unit");
            if packet[begin] & 31 != 9 {
                nals.push(begin..end);
            }
            match next {
                Some(next) => cursor = next,
                None => break,
            }
        }
        ensure!(!nals.is_empty(), "Encoded packet has no video NAL units");
        Ok(Self { packet, nals })
    }

    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = &[u8]> {
        self.nals.iter().map(|range| &self.packet[range.clone()])
    }
}

fn screen_video_filters(config: VideoConfig) -> [String; 8] {
    // Discard surplus compositor refreshes before touching their pixels. Doing
    // conversion and scaling together also avoids a full-size intermediate frame.
    [
        "videorate".into(),
        "!".into(),
        format!("video/x-raw,framerate={}/1", config.fps),
        "!".into(),
        "videoconvertscale".into(),
        "add-borders=true".into(),
        "!".into(),
        format!(
            "video/x-raw,format=I420,width={},height={},framerate={}/1",
            config.width, config.height, config.fps
        ),
    ]
}

impl VideoCapture {
    pub fn start(screen: Option<&ScreenCapture>, width: u32, height: u32) -> Result<Self> {
        Self::start_config(
            screen,
            VideoConfig {
                width,
                height,
                ..VideoQuality::Efficient.config()
            },
        )
    }

    pub fn start_config(screen: Option<&ScreenCapture>, config: VideoConfig) -> Result<Self> {
        Self::start_with_encoder(screen, config, &VideoEncoder::Software)
    }

    pub fn start_with_encoder(
        screen: Option<&ScreenCapture>,
        config: VideoConfig,
        encoder: &VideoEncoder,
    ) -> Result<Self> {
        Self::start_pipeline(screen, config, None, encoder)
    }

    fn start_pipeline(
        screen: Option<&ScreenCapture>,
        config: VideoConfig,
        synthetic_source: Option<&str>,
        encoder: &VideoEncoder,
    ) -> Result<Self> {
        config.validate()?;
        let VideoConfig {
            width, height, fps, ..
        } = config;
        let mut source = None;
        let mut command = Command::new("ffmpeg");
        command.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        let (metadata_reader, metadata_writer) = std::os::unix::net::UnixStream::pair()?;
        metadata_reader.set_nonblocking(true)?;
        let packet_sizes = BufReader::new(tokio::net::UnixStream::from_std(metadata_reader)?);
        let metadata_fd = metadata_writer.as_raw_fd();
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(metadata_fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        if let VideoEncoder::Vaapi(device) = encoder {
            command.arg("-vaapi_device").arg(device);
        }
        if let Some(screen) = screen {
            let fd = screen.remote_fd();
            let mut gst = std::process::Command::new("gst-launch-1.0");
            terminate_with_parent(&mut gst);
            gst.args([
                "-q",
                "pipewiresrc",
                &format!("fd={fd}"),
                &format!("path={}", screen.node_id),
                "do-timestamp=true",
                &format!("keepalive-time={}", 1000 / fps),
                "!",
                "queue",
                "max-size-buffers=1",
                "max-size-bytes=0",
                "max-size-time=0",
                "leaky=downstream",
                "!",
            ])
            .args(screen_video_filters(config))
            .args(["!", "y4menc", "!", "fdsink", "fd=1", "sync=false"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
            // The FD is owned by ScreenCapture throughout this process lifetime.
            // Only the child's descriptor flags change; fcntl is async-signal-safe.
            unsafe {
                gst.pre_exec(move || {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = gst
                .spawn()
                .context("Install GStreamer with pipewiresrc and y4menc")?;
            command
                .args([
                    "-probesize",
                    "32",
                    "-analyzeduration",
                    "1",
                    "-f",
                    "yuv4mpegpipe",
                    "-i",
                    "pipe:0",
                ])
                .stdin(child.stdout.take().context("Capture pipe unavailable")?);
            source = Some(child);
        } else {
            command
                .args([
                    "-re",
                    "-f",
                    "lavfi",
                    "-i",
                    synthetic_source
                        .unwrap_or(&format!("testsrc2=size={width}x{height}:rate={fps}")),
                ])
                .stdin(Stdio::null());
        }
        match encoder {
            VideoEncoder::Software => {
                command.args([
                    "-c:v",
                    "libx264",
                    "-preset",
                    config.preset,
                    "-tune",
                    "zerolatency",
                    "-pix_fmt",
                    "yuv420p",
                    "-crf",
                    &config.crf.to_string(),
                    "-x264-params",
                    "aud=1:repeat-headers=1:scenecut=0",
                ]);
            }
            VideoEncoder::Vaapi(_) => {
                command.args([
                    "-vf",
                    "format=nv12,hwupload",
                    "-c:v",
                    "h264_vaapi",
                    "-rc_mode",
                    "VBR",
                    "-b:v",
                    &format!("{}k", config.max_kbps * 3 / 4),
                    "-async_depth",
                    "1",
                    "-aud",
                    "1",
                ]);
            }
        }
        command
            .args([
                "-an",
                "-profile:v",
                "main",
                "-maxrate",
                &format!("{}k", config.max_kbps),
                "-bufsize",
                &format!("{}k", config.buffer_kbps),
                "-g",
                &fps.to_string(),
                "-bf",
                "0",
                "-map",
                "0:v:0",
                "-f",
                "tee",
                &format!(
                    "[f=framecrc:flush_packets=1]pipe:{metadata_fd}|[f=h264:flush_packets=1]pipe:1"
                ),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        terminate_with_parent(command.as_std_mut());
        let mut encoder = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                if let Some(mut child) = source {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                return Err(error).context("Could not start FFmpeg video encoder");
            }
        };
        drop(metadata_writer);
        let output = encoder.stdout.take().context("Encoder pipe unavailable")?;
        Ok(Self {
            encoder,
            source,
            output,
            packet_sizes,
            packet_metadata: String::with_capacity(128),
            width,
            height,
        })
    }

    /// FFmpeg provides packet lengths on a separate pipe before writing each
    /// encoded packet. This preserves frame boundaries without waiting for the
    /// next frame's AUD, including when the desktop stops changing.
    pub async fn frame(&mut self) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .frame_packet()
            .await?
            .iter()
            .map(<[u8]>::to_vec)
            .collect())
    }

    pub(crate) async fn frame_packet(&mut self) -> Result<VideoFrame> {
        loop {
            self.packet_metadata.clear();
            let read = self
                .packet_sizes
                .read_line(&mut self.packet_metadata)
                .await?;
            ensure!(
                read != 0,
                "Video encoder stopped before providing the next packet"
            );
            let Some(size) = packet_size(&self.packet_metadata)? else {
                continue;
            };
            let mut packet = vec![0; size];
            self.output
                .read_exact(&mut packet)
                .await
                .context("Truncated encoded video packet")?;
            return VideoFrame::from_packet(packet);
        }
    }
}

fn packet_size(line: &str) -> Result<Option<usize>> {
    if line.starts_with('#') || line.trim().is_empty() {
        return Ok(None);
    }
    let mut fields = line.split(',').map(str::trim);
    ensure!(
        fields.next() == Some("0"),
        "Invalid encoder packet metadata"
    );
    let size: usize = fields
        .nth(3)
        .context("Invalid encoder packet metadata")?
        .parse()
        .context("Invalid encoded packet size")?;
    ensure!(fields.next().is_some(), "Invalid encoder packet metadata");
    ensure!(
        (1..=16 * 1024 * 1024).contains(&size),
        "Encoded packet exceeds size limit"
    );
    Ok(Some(size))
}

fn start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    (from..data.len().saturating_sub(2)).find_map(|index| {
        if data.get(index..index + 4) == Some(&[0, 0, 0, 1]) {
            Some((index, 4))
        } else if data.get(index..index + 3) == Some(&[0, 0, 1]) {
            Some((index, 3))
        } else {
            None
        }
    })
}

impl Drop for VideoCapture {
    fn drop(&mut self) {
        let _ = self.encoder.start_kill();
        if let Some(mut source) = self.source.take() {
            let _ = source.kill();
            let _ = source.wait();
        }
    }
}

pub struct AudioCapture {
    _child: Child,
    output: ChildStdout,
}

impl AudioCapture {
    pub fn start() -> Result<Self> {
        let mut command = Command::new("pw-cat");
        terminate_with_parent(command.as_std_mut());
        let mut child = command
            .args([
                "--record",
                "--raw",
                "--rate",
                "44100",
                "--channels",
                "2",
                "--format",
                "s16",
                "--latency",
                "20ms",
                "--properties",
                "{ stream.capture.sink = true node.name = spielab-audio }",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("Install PipeWire's pw-cat to capture system audio")?;
        let output = child.stdout.take().context("Audio pipe unavailable")?;
        Ok(Self {
            _child: child,
            output,
        })
    }

    pub async fn frame(&mut self) -> Result<Vec<u8>> {
        let mut pcm = vec![0; 352 * 2 * 2];
        self.output
            .read_exact(&mut pcm)
            .await
            .context("System audio capture stopped")?;
        Ok(pcm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_metadata_rejects_invalid_lengths_and_accepts_side_data() {
        assert_eq!(packet_size("#tb 0: 1/60").unwrap(), None);
        assert_eq!(packet_size("0, 0, 0, 1, 42, 0x0, S=1").unwrap(), Some(42));
        for line in [
            "0,0,0,1,0,0x0",
            "0,0,0,1,16777217,0x0",
            "0,0,0,1,-1,0x0",
            "1,0,0,1,4,0x0",
            "0,0,0,1,42",
            "broken",
        ] {
            assert!(packet_size(line).is_err());
        }
    }

    #[test]
    fn video_frame_borrows_nals_from_the_original_packet_and_skips_aud() {
        let packet = vec![
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x67, 5, 0, 0, 0, 1, 0x68, 8, 0, 0, 1, 0x65, 0, 0, 3,
            1, 42,
        ];
        let original = packet.as_ptr();
        let frame = VideoFrame::from_packet(packet).unwrap();
        assert_eq!(frame.packet.as_ptr(), original);
        assert_eq!(
            frame.iter().collect::<Vec<_>>(),
            vec![&[0x67, 5][..], &[0x68, 8][..], &[0x65, 0, 0, 3, 1, 42][..]]
        );
        assert_eq!(
            frame.iter().next().unwrap().as_ptr(),
            frame.packet[9..].as_ptr()
        );
    }

    #[test]
    fn video_frame_rejects_missing_start_codes_and_empty_nals() {
        for packet in [
            vec![],
            vec![0x65, 1, 2],
            vec![1, 0, 0, 1, 0x65],
            vec![0, 0, 1],
            vec![0, 0, 1, 0, 0, 1, 0x65],
            vec![0, 0, 1, 0x09, 0xf0],
            vec![0, 0, 1, 0x65, 0, 0, 1],
        ] {
            assert!(VideoFrame::from_packet(packet).is_err());
        }
    }

    #[test]
    #[ignore = "requires GStreamer with videoconvertscale, videorate and y4menc"]
    fn screen_preprocessing_preserves_format_cadence_and_aspect_ratio() {
        // Compare actual output against the previous transform sequence. Include
        // both frame dropping and duplication, and a non-widescreen source.
        for (input_fps, output_fps) in [(60, 30), (30, 60), (60, 60)] {
            let config = VideoConfig {
                width: 96,
                height: 54,
                fps: output_fps,
                ..VideoQuality::Efficient.config()
            };
            let run = |filters: &[String]| {
                let output = std::process::Command::new("gst-launch-1.0")
                    .args([
                        "-q",
                        "videotestsrc",
                        &format!("num-buffers={input_fps}"),
                        "pattern=white",
                        "!",
                        &format!(
                            "video/x-raw,format=BGRx,width=64,height=48,framerate={input_fps}/1"
                        ),
                        "!",
                    ])
                    .args(filters)
                    .args(["!", "y4menc", "!", "fdsink", "fd=1", "sync=false"])
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                output.stdout
            };
            let filters = screen_video_filters(config);
            let actual = run(&filters);
            let previous = run(&[
                "videoconvert".into(),
                "!".into(),
                "videoscale".into(),
                "add-borders=true".into(),
                "!".into(),
                "videorate".into(),
                "!".into(),
                filters.last().unwrap().clone(),
            ]);
            assert_eq!(
                actual, previous,
                "Output changed for {input_fps} -> {output_fps} fps"
            );
            let header_end = actual.iter().position(|&byte| byte == b'\n').unwrap();
            let header = std::str::from_utf8(&actual[..header_end]).unwrap();
            for field in [
                "C420jpeg",
                "W96",
                "H54",
                "A3:4",
                &format!("F{output_fps}:1"),
            ] {
                assert!(
                    header.split_whitespace().any(|value| value == field),
                    "{header}"
                );
            }
            let frames = &actual[header_end + 1..];
            let frame_size = 6 + 96 * 54 * 3 / 2;
            assert_eq!(frames.len() % frame_size, 0);
            assert!(
                (output_fps as usize..=output_fps as usize + 1)
                    .contains(&(frames.len() / frame_size))
            );
            assert!(
                frames
                    .chunks_exact(frame_size)
                    .all(|frame| frame.starts_with(b"FRAME\n"))
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires FFmpeg with libx264 installed"]
    async fn single_frame_is_delivered_without_any_following_frame() {
        let config = VideoConfig {
            width: 320,
            height: 240,
            ..VideoQuality::Efficient.config()
        };
        let mut capture = VideoCapture::start_pipeline(
            None,
            config,
            Some("color=c=black:size=320x240:rate=30,trim=end_frame=1"),
            &VideoEncoder::Software,
        )
        .unwrap();
        let nals = tokio::time::timeout(std::time::Duration::from_secs(3), capture.frame())
            .await
            .unwrap()
            .unwrap();
        for kind in [7, 8, 5] {
            assert!(nals.iter().any(|n| n[0] & 31 == kind));
        }
    }

    #[tokio::test]
    #[ignore = "requires FFmpeg"]
    async fn broken_hardware_falls_back_to_working_software() {
        let directory = tempfile::tempdir().unwrap();
        let encoder = VideoEncoder::select_from(
            vec![directory.path().join("missing-render-node")],
            VideoQuality::Efficient.config(),
        )
        .await;
        assert_eq!(encoder, VideoEncoder::Software);
        encoder
            .probe(VideoQuality::Efficient.config())
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires FFmpeg with libx264 installed"]
    async fn static_frames_arrive_without_filling_the_output_buffer() {
        let config = VideoConfig {
            width: 320,
            height: 240,
            ..VideoQuality::Efficient.config()
        };
        let mut capture = VideoCapture::start_pipeline(
            None,
            config,
            Some("color=c=black:size=320x240:rate=30"),
            &VideoEncoder::Software,
        )
        .unwrap();
        // Tiny unchanged frames expose output buffering that moving test cards
        // hide. Require continuous delivery, including interframes, in real time.
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            for _ in 0..30 {
                let nals = capture.frame().await.unwrap();
                assert!(nals.iter().any(|n| matches!(n[0] & 31, 1 | 5)));
            }
        })
        .await
        .expect("Static frames were held in the encoder output buffer");
    }
    #[test]
    fn annex_b_supports_both_prefix_lengths() {
        let data = [0, 0, 0, 1, 0x67, 5, 0, 0, 1, 0x68];
        assert_eq!(start_code(&data, 0), Some((0, 4)));
        assert_eq!(start_code(&data, 4), Some((6, 3)));
        assert_eq!(start_code(&data[..3], 0), None);
    }
}
