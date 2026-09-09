# Spielab

A Rust + GPUI AirPlay sender for a Linux Wayland desktop. Discover Apple TV, pair with its on-screen code, choose a monitor or window using the desktop's screen-sharing dialog, and stream it with system audio.

The media mode is H.264 with selectable **720p/30**, **1080p/30** (default), or **1080p/60**, with stereo **44.1 kHz ALAC** audio. **Interactive** uses a 50 ms playback lead by default; **Responsive** offers 100 ms and **Buffered** restores the previous 500 ms lead for connections that stutter. This is a requested playback buffer, not a measurement of total input-to-display latency. It targets modern Apple TVs with HomeKit pairing and PTP screen-stream timing. Legacy FairPlay-only receivers and NTP-only receivers are not supported yet.

## Run

```sh
./scripts/run.sh
```

Use **Connection** to select your Apple TV and enter its code if prompted. Once authorization succeeds, Spielab opens **Sharing**, where you choose a screen/window or extended display, adjust stream quality, and click **Start sharing**. Sharing controls remain unavailable before authorization. The prominent **Disconnect** / **Cancel** button in the top-right corner, or Escape, stops the session. The initial pairing is saved for subsequent connections.

**Settings → Appearance → Dark mode** switches between light and dark colors immediately. The preference is saved in `preferences.json` alongside the application's state and restored on the next launch. Settings remain accessible while connected or sharing.

In the desktop picker, **single-click exactly one source, then press Share**. Avoid double-clicking: KDE portal 6.7.4 can abort when a source card accepts the dialog from its own click handler. Spielab requests checkbox selection to avoid that path; the workaround still needs interactive verification on this host.

Choose quality before starting. The presets cap video bitrate at 8, 20, and 30 Mbps respectively. The 1080p presets preserve more detail; 60 fps needs more CPU and network capacity. Capture is scaled with the original aspect ratio preserved.

GPU H.264 encoding is selected automatically through [FFmpeg's VA-API encoder](https://ffmpeg.org/ffmpeg-codecs.html#VAAPI-encoders) when available. Before sharing, Spielab tests actual encoded frames at the selected resolution and frame rate on accessible DRM render devices. If none pass, it uses software H.264. The command-line mirror example reports the selected encoder. AMD Radeon 890M hardware encoding has been verified on this host; other VA-API drivers are selected only if their probe passes. NVENC and other hardware APIs are not implemented.

The GPU path uses VBR at a target of 75% of the selected bitrate cap, no B-frames, and an async depth of one. Software uses the existing CRF presets. Encoding runs on the GPU; desktop capture, scaling and transfer to GPU memory still involve the CPU. A device failure after the startup probe stops sharing with an error; it does not switch codecs or encoders in a running session.

For responsiveness, start with **Interactive** and **1080p/60** if the host can encode it in real time. Try **720p/30** if encoding cannot keep up. Latency mode updates video SETUP, video presentation timestamps, audio SETUP, and audio synchronization together. Capture starts after receiver setup, the raw capture queue retains only the newest waiting frame, and encoded packets flush immediately. A separate packet-length pipe identifies completed encoded frames immediately, avoiding the previous wait for the next frame. Control feedback runs concurrently with media delivery. Network transport, encoding, decoding, and the TV's own display processing still add delay. Actual end-to-end latency requires measurement on the receiver.

**Create extended display** requests a separate virtual monitor when the desktop portal advertises support. Select it and press Share, then start sharing. Move windows onto it using your desktop's usual display controls. Disconnecting closes the portal session and removes the temporary monitor. This feature depends on the compositor and portal; creating the monitor has not yet been verified after the KDE crash workaround.

Required on the Linux host:

- A Wayland session, Vulkan-capable GPU drivers, and a working ScreenCast desktop portal (KDE, GNOME, or an appropriate compositor backend).
- Rust and the GPUI native build dependencies: a C linker, pkg-config, FreeType, Fontconfig, libxkbcommon and libxkbcommon-x11.
- `gst-launch-1.0` with `pipewiresrc`, `queue`, `videoconvert`, `videoscale`, `videorate`, `y4menc`, and `fdsink`. The PipeWire plugin must support `keepalive-time`.
- `ffmpeg` with the `libx264` encoder, and PipeWire's `pw-cat` utility. Hardware encoding additionally needs `h264_vaapi`, a VA-API driver with H.264 encoding support, and access to `/dev/dri/renderD*`.
- Apple TV on the same reachable network with AirPlay enabled.

Plain `cargo run` and `cargo build --locked` work on this host. For native Linux GUI builds, `build.rs` supplies a build-local xkbcommon-x11 linker symlink when the compiler can find the runtime library but its development symlink is missing. No system files are changed. If neither library is installed, install the distribution’s xkbcommon-x11 development package. Cross-compilation requires the target development libraries.

The development profile optimizes Spielab's packet handling and ChaCha20-Poly1305 dependencies because the development runner is used for live sharing. On this host the isolated 64 KiB video-packet encryption diagnostic fell from approximately 7.8 ms to 0.057 ms per packet. This microbenchmark does not measure display latency.

## How it works

- GPUI renders directly through its Wayland backend; the X11 backend is disabled.
- A separate Tokio worker handles discovery, pairing, network requests, and cancellation.
- The XDG ScreenCast portal grants a PipeWire node and remote FD. Both the portal session and FD remain owned until sharing ends; cancelling closes the session.
- GStreamer reads the authorized PipeWire source. FFmpeg encodes H.264 with [immediate packet flushing](https://ffmpeg.org/ffmpeg-formats.html); Rust assembles access units, sends the decoder configuration, and encrypts each video packet with ChaCha20-Poly1305 using the complete header as authenticated data.
- `pw-cat` captures the default output's monitor, not the microphone. Rust encodes PCM into ALAC, encrypts RTP audio, answers retransmission requests from a bounded history, and sends timing announcements on the negotiated control port.
- Video and audio share the receiver's PTP timeline, anchored using Apple TV's response timestamps. Encrypted event-channel commands, feedback, and video heartbeats keep the session alive.

Pairing identities are saved with mode 0600 in a directory with mode 0700. The default is `$XDG_STATE_HOME/spielab` or `~/.local/state/spielab`; `SPIELAB_STATE_DIR` overrides it. The development runner reuses this checkout's `.state` directory if it exists. Identity files are excluded from version control.

## Validation

```sh
cargo test --no-default-features
cargo test --no-default-features --test media_runtime -- --ignored
cargo test --no-default-features --lib -- --ignored
cargo check --all-targets
```

Tests cover authenticated video framing and nonce separation, signed 64-bit clock IDs, audio/video playback lead agreement, PIN validation, private identity replacement, ALAC decoding through an independent decoder, and real H.264 encode/parse/decode round trips for every quality preset, including decoded dimensions and frame rate. An additional timed test checks that tiny, static frames arrive continuously without waiting for the output buffer to fill. The ignored runtime tests require FFmpeg.

The ignored hardware test requires VA-API H.264 on `/dev/dri/renderD128`. It checks automatic GPU selection and independently decodes more than one GOP at every quality preset. The ignored library tests also verify that an unavailable render device falls back to a working software encoder. On hosts without VA-API, run the software integration tests individually instead of all ignored integration tests.

Hardware smoke tests:

```sh
cargo run --no-default-features --example discover
SPIELAB_STATE_DIR=.state cargo run --no-default-features --example pair -- 'Living Room'
SPIELAB_STATE_DIR=.state cargo run --no-default-features --example mirror -- 'Living Room' --test
SPIELAB_STATE_DIR=.state cargo run --no-default-features --example mirror -- 'Living Room'
SPIELAB_STATE_DIR=.state cargo run --no-default-features --example mirror -- 'Living Room' --extend --quality=1080p60 --latency=50
```

The mirror example stops after 30 seconds; set `SPIELAB_TEST_SECONDS` to change that. Its `--test` mode sends a moving test card with the same audio/session path. Without that flag it opens the screen picker.

During development, an AppleTV14,1 accepted saved-identity pair verification, encrypted control/video/audio setup, and a 20-second, 600-frame test stream. Live Wayland capture and output-monitor audio have also been connected through the GPUI app. Visible picture, audible sound, and A/V synchronization still need receiver-side confirmation; sending packets alone does not prove playback.

The responsive-mode smoke test also completed: Apple TV accepted the 100 ms setup and the sender transmitted 600 frames at 1080p/60 over ten seconds. This validates negotiation and transmission, not measured input latency or receiver-side A/V synchronization.

The same ten-second, 600-frame Apple TV test passed with Radeon 890M VA-API hardware H.264 encoding. All quality presets also passed independent decoding through FFmpeg, and the unavailable-device fallback test passed. End-to-end latency has not been measured.

The 50 ms mode completed a 15-second, 900-frame 1080p/60 GPU test. Packet-length framing also passes a regression test delivering one complete frame with no subsequent frame. The concurrent-feedback implementation kept measured encoded-frame queue wait below 1 ms after startup during this test, while control replies reached 17 ms. These results validate individual sender improvements, not receiver-side presentation timing.

After enabling compiler optimization for the packet path, a further 600-frame test passed. Steady-state one-second intervals reported 60 frames with maximum measured queue wait and encrypt/write duration both below 1 ms.

## Protocol references

[airplay2-rs](https://github.com/lmcgartland/airplay2-rs/tree/a7f019fe6246ebd9701201a8a5c31e9a15243956) supplies the pinned Rust discovery, cryptographic pairing, and RTSP libraries. The mirroring wire layout and stream negotiation were researched against [Doubletake](https://github.com/omarroth/doubletake) and [UxPlay](https://github.com/FDH2/UxPlay). Spielab's session, capture, packetization, and UI code are in this repository.

Sender diagnostics log completed frames per interval, maximum encoded-frame queue time, maximum encrypt/write time, and control-response time. These exclude capture, GPU encoding, network delivery after the socket write, and receiver display time; they are not end-to-end latency measurements.
