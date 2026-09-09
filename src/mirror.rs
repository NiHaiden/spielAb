//! AirPlay screen session (type 110 video and type 96 screen audio).
//! Wire layout references: Doubletake's protocol implementation and UxPlay.
use crate::{
    media::{AudioCapture, VideoCapture},
    pairing::ControlSession,
    settings::LatencyMode,
};
use airplay_crypto::chacha::ControlCipher;
use airplay_rtsp::{RtspMethod, RtspRequest, RtspResponse};
use anyhow::{Context, Result, ensure};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit,
    aead::{Aead, Payload},
};
use hkdf::Hkdf;
use plist::{Dictionary, Value};
use rand::RngCore;
use sha2::Sha512;
use std::{
    collections::VecDeque,
    io::Cursor,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::mpsc,
    task::JoinHandle,
};

const AUDIO_RATE: u32 = 44100;

fn dict(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Dictionary(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
    )
}
fn number(value: u64) -> Value {
    Value::Integer(value.into())
}
fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_dictionary()?.get(key)
}
fn integer(value: &Value, key: &str) -> Option<u64> {
    get(value, key)?
        .as_unsigned_integer()
        .or_else(|| get(value, key)?.as_signed_integer().map(|n| n as u64))
}
fn binary(value: &Value) -> Result<Vec<u8>> {
    let mut data = vec![];
    value.to_writer_binary(&mut data)?;
    Ok(data)
}
fn parse(data: &[u8]) -> Result<Value> {
    Ok(Value::from_reader(Cursor::new(data))?)
}

#[derive(Clone)]
struct Clock {
    anchor: Instant,
    nanos: u64,
    timeline: u64,
}
impl Clock {
    fn at(&self, time: Instant) -> u64 {
        let delta = time
            .checked_duration_since(self.anchor)
            .map(|d| d.as_nanos() as i128)
            .unwrap_or_else(|| -(self.anchor.duration_since(time).as_nanos() as i128));
        (self.nanos as i128 + delta).max(0) as u64
    }
    fn update(&mut self, response: &RtspResponse) -> Result<()> {
        let received: u64 = response
            .header("X-Apple-RequestReceivedTimestamp")
            .context("Apple TV did not provide a media clock timestamp")?
            .parse()?;
        let processing: u64 = response
            .header("X-Apple-ProcessingTime")
            .unwrap_or("0")
            .parse()?;
        self.nanos = received
            .checked_add(processing)
            .and_then(|v| v.checked_mul(1_000_000))
            .context("Media clock overflow")?;
        self.anchor = Instant::now();
        Ok(())
    }
}
fn fixed_time(nanos: u64) -> u64 {
    ((nanos / 1_000_000_000) << 32)
        | (((nanos % 1_000_000_000) as u128 * (1u128 << 32) / 1_000_000_000) as u64)
}

pub struct MirrorSession {
    video: TcpStream,
    video_cipher: ChaCha20Poly1305,
    video_nonce: u64,
    audio: UdpSocket,
    audio_control: UdpSocket,
    audio_cipher: ChaCha20Poly1305,
    audio_nonce: u64,
    audio_seq: u16,
    audio_rtp: u32,
    audio_origin: Option<(Instant, u32)>,
    ssrc: u32,
    history: VecDeque<(u16, Vec<u8>)>,
    clock: Arc<Mutex<Clock>>,
    tasks: Vec<JoinHandle<()>>,
    codec: Option<Vec<u8>>,
    width: u32,
    height: u32,
    latency: LatencyMode,
}

impl Drop for MirrorSession {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl MirrorSession {
    pub async fn setup(control: &mut ControlSession, width: u32, height: u32) -> Result<Self> {
        Self::setup_with_latency(control, width, height, LatencyMode::default()).await
    }

    pub async fn setup_with_latency(
        control: &mut ControlSession,
        width: u32,
        height: u32,
        latency: LatencyMode,
    ) -> Result<Self> {
        let secret = control
            .shared_secret
            .context("Pair verification is required before mirroring")?;
        let local = control
            .transport
            .local_addr()
            .context("Control socket has no local address")?
            .ip();
        let remote = control.transport.addr().ip();
        let video_id = rand::random::<u64>() & i64::MAX as u64;
        let audio_id = rand::random::<u64>() & i64::MAX as u64;
        let uri = format!("rtsp://{remote}/{audio_id}");
        let video_uri = format!("rtsp://{remote}/{video_id}");
        let audio = UdpSocket::bind(SocketAddr::new(local, 0)).await?;
        let audio_control = UdpSocket::bind(SocketAddr::new(local, 0)).await?;
        let peer = dict([
            ("ID", uuid::Uuid::new_v4().to_string().into()),
            ("SupportsClockPortMatchingOverride", true.into()),
            ("DeviceType", number(0)),
            ("Addresses", Value::Array(vec![local.to_string().into()])),
        ]);
        let device_id = uuid::Uuid::new_v4().simple().to_string()[..12].to_owned();
        let request = dict([
            ("deviceID", device_id.clone().into()),
            ("macAddress", device_id.into()),
            ("sessionUUID", uuid::Uuid::new_v4().to_string().into()),
            ("sourceVersion", "980.71.1".into()),
            ("isScreenMirroringSession", true.into()),
            ("timingProtocol", "PTP".into()),
            ("osBuildVersion", "13F69".into()),
            ("model", "Linux".into()),
            ("name", "Spielab".into()),
            ("timingPeerInfo", peer.clone()),
            ("timingPeerList", Value::Array(vec![peer])),
            ("updateSessionRequest", false.into()),
            ("combinedGetInfoWithControlSetup", true.into()),
        ]);
        let response = control
            .request_response(RtspRequest::setup(&uri, binary(&request)?))
            .await
            .context("Mirroring control SETUP")?;
        control.mirror_uri = Some(uri.clone());
        let body = parse(response.body.as_deref().unwrap_or_default())?;
        let timeline = get(&body, "timingPeerInfo")
            .and_then(|v| integer(v, "ClockID"))
            .context("Receiver did not negotiate PTP timeline")?;
        let mut clock = Clock {
            anchor: Instant::now(),
            nanos: 0,
            timeline,
        };
        clock.update(&response)?;
        let clock = Arc::new(Mutex::new(clock));
        let mut tasks = vec![];
        if let Some(port) = integer(&body, "eventPort").filter(|p| *p > 0 && *p <= 65535) {
            let socket = tokio::time::timeout(
                Duration::from_secs(5),
                TcpStream::connect(SocketAddr::new(remote, port as u16)),
            )
            .await??;
            let event_clock = clock.clone();
            tasks.push(tokio::spawn(async move {
                if let Err(error) = serve_events(socket, secret, event_clock).await {
                    eprintln!("AirPlay event channel: {error:#}");
                }
            }));
        }
        // Guard task lifetimes even if one of the later SETUP phases fails.
        let mut guard = TaskGuard(tasks);
        if get(&body, "skipRecord").and_then(Value::as_boolean) != Some(true) {
            control.request(RtspRequest::record(&uri)).await?;
        }
        let mut audio_key = [0; 32];
        rand::thread_rng().fill_bytes(&mut audio_key);
        let mut audio_desc = Dictionary::new();
        for (name, value) in [
            ("type", 96),
            ("streamConnectionID", audio_id),
            ("ct", 2),
            ("spf", 352),
            ("sr", 44100),
            ("audioFormat", 0x40000),
            ("latencyMin", 0),
            ("latencyMax", latency.audio_samples(AUDIO_RATE) as u64),
        ] {
            audio_desc.insert(name.into(), number(value));
        }
        audio_desc.insert("audioMode".into(), "default".into());
        audio_desc.insert("usingScreen".into(), true.into());
        audio_desc.insert("shk".into(), Value::Data(audio_key.to_vec()));
        if control.device.features.raw() & (1 << 59) != 0 {
            audio_desc.insert("isMedia".into(), false.into());
            audio_desc.insert("supportsDynamicStreamID".into(), true.into());
            audio_desc.insert(
                "streamConnections".into(),
                dict([
                    (
                        "streamConnectionTypeRTP",
                        dict([("streamConnectionKeyUseStreamEncryptionKey", true.into())]),
                    ),
                    (
                        "streamConnectionTypeRTCP",
                        dict([(
                            "streamConnectionKeyPort",
                            number(audio_control.local_addr()?.port() as u64),
                        )]),
                    ),
                ]),
            );
        } else {
            audio_desc.insert(
                "controlPort".into(),
                number(audio_control.local_addr()?.port() as u64),
            );
        }
        let audio_response = control
            .request(RtspRequest::setup(
                &uri,
                binary(&dict([(
                    "streams",
                    Value::Array(vec![Value::Dictionary(audio_desc)]),
                )]))?,
            ))
            .await
            .context("Screen audio SETUP")?;
        let audio_response = parse(&audio_response)?;
        let audio_stream = stream(&audio_response, 96)?;
        let data_port = stream_port(audio_stream, "dataPort", "streamConnectionTypeRTP")?;
        let control_port = stream_port(audio_stream, "controlPort", "streamConnectionTypeRTCP")?;
        audio.connect(SocketAddr::new(remote, data_port)).await?;
        audio_control
            .connect(SocketAddr::new(remote, control_port))
            .await?;
        let video_desc = dict([
            ("type", number(110)),
            ("streamConnectionID", number(video_id)),
            ("latencyMs", number(latency.millis())),
            ("shk", Value::Data(control.stream_key.to_vec())),
            ("shiv", Value::Data(control.stream_iv.to_vec())),
            (
                "timestampInfo",
                Value::Array(
                    ["SubSu", "BePxT", "AfPxT", "BefEn", "EmEnc"]
                        .map(|name| dict([("name", name.into())]))
                        .to_vec(),
                ),
            ),
        ]);
        let video_response = control
            .request(RtspRequest::setup(
                &video_uri,
                binary(&dict([("streams", Value::Array(vec![video_desc]))]))?,
            ))
            .await
            .context("Screen video SETUP")?;
        let video_response = parse(&video_response)?;
        let port = integer(stream(&video_response, 110)?, "dataPort")
            .context("Receiver omitted video port")?;
        ensure!(port > 0 && port <= 65535, "Invalid video port");
        let video = tokio::time::timeout(
            Duration::from_secs(5),
            TcpStream::connect(SocketAddr::new(remote, port as u16)),
        )
        .await??;
        video.set_nodelay(true)?;
        let video_key = derive_key(
            &secret,
            format!("DataStream-Salt{video_id}").as_bytes(),
            b"DataStream-Output-Encryption-Key",
        );
        control
            .request(RtspRequest::set_parameter_text(
                &uri,
                b"volume: 0.000000\r\n".to_vec(),
            ))
            .await?;
        Ok(Self {
            video,
            video_cipher: ChaCha20Poly1305::new(&video_key.into()),
            video_nonce: 0,
            audio,
            audio_control,
            audio_cipher: ChaCha20Poly1305::new(&audio_key.into()),
            audio_nonce: 0,
            audio_seq: 0,
            audio_rtp: rand::random(),
            audio_origin: None,
            ssrc: rand::random(),
            history: VecDeque::with_capacity(512),
            clock,
            tasks: std::mem::take(&mut guard.0),
            codec: None,
            width,
            height,
            latency,
        })
    }

    pub async fn run(
        &mut self,
        control: &mut ControlSession,
        mut video: VideoCapture,
        mut audio: AudioCapture,
        progress: impl Fn(u64) + Send,
    ) -> Result<()> {
        enum Media {
            Video(Instant, Vec<Vec<u8>>),
            Audio(Instant, Vec<u8>),
            Error(anyhow::Error),
        }
        let (send, mut frames) = mpsc::channel(2);
        let video_send = send.clone();
        self.tasks.push(tokio::spawn(async move {
            loop {
                let frame = match video.frame().await {
                    Ok(frame) => Media::Video(Instant::now(), frame),
                    Err(e) => Media::Error(e),
                };
                let stop = matches!(frame, Media::Error(_));
                if video_send.send(frame).await.is_err() || stop {
                    break;
                }
            }
        }));
        self.tasks.push(tokio::spawn(async move {
            loop {
                let frame = match audio.frame().await {
                    Ok(frame) => Media::Audio(Instant::now(), frame),
                    Err(e) => Media::Error(e),
                };
                let stop = matches!(frame, Media::Error(_));
                if send.send(frame).await.is_err() || stop {
                    break;
                }
            }
        }));
        // Poll control traffic alongside media. A delayed RTSP reply must not
        // stop capture consumption or video/audio delivery.
        let feedback_clock = self.clock.clone();
        let feedback = async {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let started = Instant::now();
                let response = control
                    .request_response(RtspRequest::new(RtspMethod::Post, "/feedback").body(vec![]))
                    .await?;
                feedback_clock.lock().unwrap().update(&response)?;
                eprintln!("Stream control: reply={}ms", started.elapsed().as_millis());
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        };
        tokio::pin!(feedback);
        let mut max_queue = Duration::ZERO;
        let mut max_send = Duration::ZERO;
        let mut previous_frames = 0;
        let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut video_frames = 0;
        let started = Instant::now();
        let mut control_packet = [0; 2048];
        loop {
            tokio::select! {
                result = &mut feedback => { result?; anyhow::bail!("Control feedback stopped"); },
                frame = frames.recv() => match frame.context("Capture tasks stopped")? {
                    Media::Video(time, nals) => { max_queue = max_queue.max(time.elapsed()); let send_started = Instant::now(); self.send_video(time, nals).await?; max_send = max_send.max(send_started.elapsed()); video_frames += 1; if video_frames == 1 || video_frames % 30 == 0 { progress(video_frames); } },
                    Media::Audio(time, pcm) => self.send_audio(time, &pcm).await?,
                    Media::Error(error) => return Err(error),
                },
                _ = heartbeat.tick() => {
                    ensure!(video_frames > 0 || started.elapsed() < Duration::from_secs(15), "No video frames arrived from the selected screen; check the sharing permission and capture pipeline");
                    ensure!(self.audio_origin.is_some() || started.elapsed() < Duration::from_secs(15), "No system audio frames arrived from PipeWire");
                    if video_frames > 0 {
                        let mut packet = [0;128]; packet[4] = 2; packet[6] = 0x1e;
                        self.write_video(&packet).await?;
                    }
                    eprintln!("Stream sender: frames={} max_queue={}ms max_send={}ms lead={}ms",
                        video_frames - previous_frames, max_queue.as_millis(), max_send.as_millis(), self.latency.millis());
                    previous_frames = video_frames;
                    max_queue = Duration::ZERO;
                    max_send = Duration::ZERO;
                    if let Some((origin, rtp)) = self.audio_origin {
                        let time = Instant::now();
                        let current = rtp.wrapping_add((time.duration_since(origin).as_secs_f64() * AUDIO_RATE as f64) as u32);
                        self.send_sync(time, current, false).await?;
                    }
                },
                size = self.audio_control.recv(&mut control_packet) => {
                    self.retransmit(&control_packet[..size?]).await?;
                }
            }
        }
    }

    async fn write_video(&mut self, bytes: &[u8]) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(2), self.video.write_all(bytes))
            .await
            .context("Apple TV stopped reading video")??;
        Ok(())
    }

    async fn send_video(&mut self, time: Instant, nals: Vec<Vec<u8>>) -> Result<()> {
        let sps = nals.iter().find(|n| n[0] & 31 == 7);
        let pps = nals.iter().find(|n| n[0] & 31 == 8);
        let clock = self.clock.lock().unwrap().clone();
        let timestamp = fixed_time(clock.at(time + self.latency.lead()));
        if let (Some(sps), Some(pps)) = (sps, pps) {
            let config = avcc_config(sps, pps)?;
            if self.codec.as_ref() != Some(&config) {
                let mut header = video_header(config.len(), 1, timestamp, 0)?;
                header[6] = 0x16;
                header[7] = 1;
                for offset in [16, 40, 56] {
                    header[offset..offset + 4].copy_from_slice(&(self.width as f32).to_le_bytes());
                    header[offset + 4..offset + 8]
                        .copy_from_slice(&(self.height as f32).to_le_bytes());
                }
                let mut packet = header.to_vec();
                packet.extend_from_slice(&config);
                self.write_video(&packet).await?;
                self.codec = Some(config);
            }
        }
        ensure!(
            self.codec.is_some(),
            "Encoder did not provide H.264 SPS/PPS"
        );
        let mut data = vec![];
        let mut keyframe = false;
        for nal in nals {
            if [7, 8, 9].contains(&(nal[0] & 31)) {
                continue;
            }
            keyframe |= nal[0] & 31 == 5;
            data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            data.extend_from_slice(&nal);
        }
        if data.is_empty() {
            return Ok(());
        }
        let mut header = video_header(data.len() + 16, 0, timestamp, clock.timeline)?;
        if keyframe {
            header[5] = 0x10;
        }
        let packet = seal_video(&self.video_cipher, &header, self.video_nonce, &data)?;
        self.video_nonce = self
            .video_nonce
            .checked_add(1)
            .context("Video nonce exhausted")?;
        self.write_video(&packet).await
    }

    async fn send_audio(&mut self, time: Instant, pcm: &[u8]) -> Result<()> {
        if self.audio_origin.is_none() {
            self.audio_origin = Some((time, self.audio_rtp));
            self.send_sync(time, self.audio_rtp, true).await?;
        }
        let payload = alac_verbatim(pcm)?;
        let mut header = [0u8; 12];
        header[0] = 0x80;
        header[1] = 96;
        header[2..4].copy_from_slice(&self.audio_seq.to_be_bytes());
        header[4..8].copy_from_slice(&self.audio_rtp.to_be_bytes());
        header[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        let nonce = nonce(self.audio_nonce);
        let sealed = self
            .audio_cipher
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: &payload,
                    aad: &header[4..12],
                },
            )
            .map_err(|_| anyhow::anyhow!("Audio encryption failed"))?;
        let mut packet = header.to_vec();
        packet.extend_from_slice(&sealed);
        packet.extend_from_slice(&self.audio_nonce.to_le_bytes());
        self.audio.send(&packet).await?;
        if self.history.len() == 512 {
            self.history.pop_front();
        }
        self.history.push_back((self.audio_seq, packet));
        self.audio_nonce = self
            .audio_nonce
            .checked_add(1)
            .context("Audio nonce exhausted")?;
        self.audio_seq = self.audio_seq.wrapping_add(1);
        self.audio_rtp = self.audio_rtp.wrapping_add(352);
        Ok(())
    }

    async fn send_sync(&self, time: Instant, rtp: u32, first: bool) -> Result<()> {
        let clock = self.clock.lock().unwrap().clone();
        let mut packet = [0u8; 28];
        packet[0] = if first { 0x90 } else { 0x80 };
        packet[1] = 0xd7;
        packet[3] = 4;
        packet[4..8].copy_from_slice(
            &rtp.wrapping_sub(self.latency.audio_samples(AUDIO_RATE))
                .to_be_bytes(),
        );
        packet[8..16].copy_from_slice(&clock.at(time).to_be_bytes());
        packet[16..20].copy_from_slice(&rtp.to_be_bytes());
        packet[20..28].copy_from_slice(&clock.timeline.to_be_bytes());
        self.audio_control.send(&packet).await?;
        Ok(())
    }

    async fn retransmit(&self, request: &[u8]) -> Result<()> {
        if request.len() != 8 || request[..2] != [0x80, 0xd5] {
            return Ok(());
        }
        let first = u16::from_be_bytes([request[4], request[5]]);
        let count = u16::from_be_bytes([request[6], request[7]]).min(512);
        for offset in 0..count {
            let seq = first.wrapping_add(offset);
            if let Some((_, original)) = self.history.iter().find(|(number, _)| *number == seq) {
                let mut response = vec![0x80, 0xd6, request[2], request[3]];
                response.extend_from_slice(original);
                self.audio_control.send(&response).await?;
            } else {
                break;
            }
        }
        Ok(())
    }
}

struct TaskGuard(Vec<JoinHandle<()>>);
impl Drop for TaskGuard {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

fn stream(response: &Value, kind: u64) -> Result<&Value> {
    get(response, "streams")
        .and_then(Value::as_array)
        .and_then(|streams| streams.iter().find(|s| integer(s, "type") == Some(kind)))
        .context("SETUP omitted expected stream")
}
fn stream_port(stream: &Value, old: &str, modern: &str) -> Result<u16> {
    let port = integer(stream, old)
        .or_else(|| {
            get(stream, "streamConnections")
                .and_then(|v| get(v, modern))
                .and_then(|v| integer(v, "streamConnectionKeyPort"))
        })
        .context("SETUP omitted audio port")?;
    ensure!(port > 0 && port <= 65535, "Invalid audio port");
    Ok(port as u16)
}
fn derive_key(secret: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let mut key = [0; 32];
    Hkdf::<Sha512>::new(Some(salt), secret)
        .expand(info, &mut key)
        .expect("32 byte HKDF output");
    key
}
fn nonce(counter: u64) -> [u8; 12] {
    let mut n = [0; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    n
}
fn video_header(length: usize, kind: u8, time: u64, timeline: u64) -> Result<[u8; 128]> {
    ensure!(length <= 16 * 1024 * 1024, "Video frame too large");
    let mut h = [0; 128];
    h[..4].copy_from_slice(&(length as u32).to_le_bytes());
    h[4] = kind;
    h[8..16].copy_from_slice(&time.to_le_bytes());
    h[40..48].copy_from_slice(&timeline.to_le_bytes());
    Ok(h)
}
fn seal_video(
    cipher: &ChaCha20Poly1305,
    header: &[u8; 128],
    counter: u64,
    data: &[u8],
) -> Result<Vec<u8>> {
    let mut packet = header.to_vec();
    packet.extend_from_slice(
        &cipher
            .encrypt(
                (&nonce(counter)).into(),
                Payload {
                    msg: data,
                    aad: header,
                },
            )
            .map_err(|_| anyhow::anyhow!("Video encryption failed"))?,
    );
    Ok(packet)
}
fn avcc_config(sps: &[u8], pps: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        sps.len() >= 4 && sps.len() <= 65535 && !pps.is_empty() && pps.len() <= 65535,
        "Invalid H.264 parameter sets"
    );
    let mut config = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    config.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    config.extend_from_slice(sps);
    config.push(1);
    config.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    config.extend_from_slice(pps);
    Ok(config)
}

fn alac_verbatim(pcm: &[u8]) -> Result<Vec<u8>> {
    ensure!(pcm.len() == 352 * 4, "Expected 352 stereo PCM frames");
    let mut output = vec![0; (55 + pcm.len() * 8 + 3).div_ceil(8)];
    let mut bit = 0;
    let mut write = |value: u32, count: usize| {
        for shift in (0..count).rev() {
            output[bit / 8] |= (((value >> shift) & 1) as u8) << (7 - bit % 8);
            bit += 1;
        }
    };
    write(1, 3);
    write(0, 4);
    write(0, 12);
    write(1, 1);
    write(0, 2);
    write(1, 1);
    write(352, 32);
    for sample in pcm.as_chunks::<2>().0 {
        write(u16::from_le_bytes([sample[0], sample[1]]) as u32, 16);
    }
    write(7, 3);
    Ok(output)
}

async fn serve_events(
    mut socket: TcpStream,
    secret: [u8; 32],
    clock: Arc<Mutex<Clock>>,
) -> Result<()> {
    let mut cipher = ControlCipher::new(
        derive_key(&secret, b"Events-Salt", b"Events-Read-Encryption-Key"),
        derive_key(&secret, b"Events-Salt", b"Events-Write-Encryption-Key"),
    );
    let mut pending = vec![];
    loop {
        let length = socket.read_u16_le().await?;
        ensure!(length > 0 && length <= 1024, "Invalid event frame length");
        let mut frame = vec![0; length as usize + 16];
        socket.read_exact(&mut frame).await?;
        pending.extend_from_slice(&cipher.decrypt_block(&frame, length)?);
        ensure!(pending.len() <= 1024 * 1024, "Event request exceeds limit");
        while let Some(boundary) = pending.windows(4).position(|s| s == b"\r\n\r\n") {
            ensure!(boundary <= 16384, "Event headers exceed limit");
            let headers = std::str::from_utf8(&pending[..boundary])?;
            let mut size = 0;
            let mut cseq = None;
            for line in headers.lines().skip(1) {
                if let Some((key, value)) = line.split_once(':') {
                    if key.eq_ignore_ascii_case("Content-Length") {
                        size = value.trim().parse::<usize>()?;
                    }
                    if key.eq_ignore_ascii_case("CSeq") {
                        cseq = Some(value.trim().parse::<u32>()?);
                    }
                }
            }
            ensure!(size <= 1024 * 1024 - 16388, "Event body exceeds limit");
            if pending.len() < boundary + 4 + size {
                break;
            }
            let cseq = cseq.context("Event omitted CSeq")?;
            if size > 0
                && let Ok(command) = parse(&pending[boundary + 4..boundary + 4 + size])
                && get(&command, "type").and_then(Value::as_string) == Some("updateTimingPeerInfo")
                && let Some(id) = get(&command, "value")
                    .and_then(|v| integer(v, "ClockID"))
                    .filter(|v| *v != 0)
            {
                clock.lock().unwrap().timeline = id;
            }
            pending.drain(..boundary + 4 + size);
            let response = format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Length: 0\r\n\r\n");
            socket
                .write_all(&cipher.encrypt(response.as_bytes())?)
                .await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "manual packet-encryption timing diagnostic"]
    fn measure_video_packet_encryption() {
        let cipher = ChaCha20Poly1305::new(&[7; 32].into());
        let payload = vec![42; 65536];
        let header = video_header(payload.len() + 16, 0, 123, 456).unwrap();
        let started = Instant::now();
        for nonce in 0..100 {
            std::hint::black_box(seal_video(&cipher, &header, nonce, &payload).unwrap());
        }
        eprintln!(
            "64 KiB video encryption: {:.3} ms/packet",
            started.elapsed().as_secs_f64() * 10.0
        );
    }

    #[test]
    fn playback_lead_matches_audio_sample_offset_for_both_modes() {
        let clock = Clock {
            anchor: Instant::now(),
            nanos: 2_000_000_000,
            timeline: 1,
        };
        for (mode, samples, nanos) in [
            (LatencyMode::Interactive, 2205, 50_000_000),
            (LatencyMode::Responsive, 4410, 100_000_000),
            (LatencyMode::Buffered, 22050, 500_000_000),
        ] {
            assert_eq!(mode.audio_samples(AUDIO_RATE), samples);
            assert_eq!(clock.at(clock.anchor + mode.lead()) - clock.nanos, nanos);
            // RTP offset arithmetic must also work across its 32-bit wrap.
            let rtp = 5u32;
            assert_eq!(rtp.wrapping_sub(samples).wrapping_add(samples), rtp);
        }
    }
    #[test]
    fn encrypted_video_authenticates_header_and_uses_distinct_nonces() {
        let key = derive_key(
            &[7; 32],
            b"DataStream-Salt42",
            b"DataStream-Output-Encryption-Key",
        );
        let cipher = ChaCha20Poly1305::new(&key.into());
        let header = video_header(19, 0, 123, 456).unwrap();
        let packet = seal_video(&cipher, &header, 0, b"abc").unwrap();
        assert_eq!(packet.len(), 147);
        assert_eq!(
            cipher
                .decrypt(
                    (&nonce(0)).into(),
                    Payload {
                        msg: &packet[128..],
                        aad: &header
                    }
                )
                .unwrap(),
            b"abc"
        );
        let mut changed = header;
        changed[8] ^= 1;
        assert!(
            cipher
                .decrypt(
                    (&nonce(0)).into(),
                    Payload {
                        msg: &packet[128..],
                        aad: &changed
                    }
                )
                .is_err()
        );
        assert_ne!(packet, seal_video(&cipher, &header, 1, b"abc").unwrap());
    }
    #[test]
    fn uncompressed_alac_has_expected_bit_length_and_stereo_tag() {
        let packet = alac_verbatim(&vec![0; 1408]).unwrap();
        assert_eq!(packet.len(), 1416);
        assert_eq!(packet[0], 0x20);
        assert!(alac_verbatim(&[0; 10]).is_err());
    }
    #[test]
    fn fixed_point_timestamps_preserve_half_seconds() {
        assert_eq!(fixed_time(1_500_000_000), 0x1_8000_0000);
    }

    #[test]
    fn signed_binary_plist_clock_ids_retain_all_64_bits() {
        let id = -4253718895002124280i64;
        let wire = binary(&dict([("ClockID", Value::Integer(id.into()))])).unwrap();
        assert_eq!(integer(&parse(&wire).unwrap(), "ClockID"), Some(id as u64));
    }

    #[test]
    fn independent_alac_decoder_recovers_pcm_exactly() {
        use symphonia_codec_alac::AlacDecoder;
        use symphonia_core::{
            audio::SampleBuffer,
            codecs::{CODEC_TYPE_ALAC, CodecParameters, Decoder, DecoderOptions},
            formats::Packet,
        };
        let samples: Vec<i16> = (0..704)
            .map(|i| (i as i16).wrapping_mul(173).wrapping_sub(32768u16 as i16))
            .collect();
        let pcm: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let encoded = alac_verbatim(&pcm).unwrap();
        let mut cookie = vec![];
        cookie.extend_from_slice(&352u32.to_be_bytes());
        cookie.extend_from_slice(&[0, 16, 40, 10, 14, 2]);
        cookie.extend_from_slice(&255u16.to_be_bytes());
        cookie.extend_from_slice(&0u32.to_be_bytes());
        cookie.extend_from_slice(&0u32.to_be_bytes());
        cookie.extend_from_slice(&44100u32.to_be_bytes());
        let mut parameters = CodecParameters::new();
        parameters.codec = CODEC_TYPE_ALAC;
        parameters.extra_data = Some(cookie.into_boxed_slice());
        let mut decoder = AlacDecoder::try_new(&parameters, &DecoderOptions::default()).unwrap();
        let decoded = decoder
            .decode(&Packet::new_from_slice(0, 0, 352, &encoded))
            .unwrap();
        let mut output = SampleBuffer::<i16>::new(decoded.capacity() as u64, *decoded.spec());
        output.copy_interleaved_ref(decoded);
        assert_eq!(output.samples(), samples);
    }
}
