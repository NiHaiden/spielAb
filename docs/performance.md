# Performance measurements

## Screen preprocessing

Screen capture now drops excess source frames before pixel processing and uses
`videoconvertscale` to convert and resize together. This avoids processing frames
that would be discarded and avoids the full-resolution converted intermediate
image. The combined element is part of GStreamer Base Plug-ins; GStreamer 1.22 or
newer is required. See the [GStreamer 1.22 release notes](https://gstreamer.freedesktop.org/releases/1.22/)
and [element documentation](https://gstreamer.freedesktop.org/documentation/videoconvertscale/videoconvertscale.html).

Measured locally with GStreamer 1.28.7 on 2026-09-16: 240 synthetic 3840×2160 BGRx
frames at 60 fps, converted to 1280×720 I420 at 30 fps. Each pipeline ran three
times, without clock synchronization. Measurements are child-process user plus
system CPU time, including test-image generation and pipeline startup.

| Pipeline | Median CPU time |
| --- | ---: |
| Previous: convert → scale → rate | 4.057 s |
| Rate → convert → scale | 2.273 s |
| Current: rate → combined convert/scale | 1.716 s |

The current pipeline used approximately **58% less CPU time** in this synthetic
preprocessing workload. This is not a measurement of the complete application:
it excludes PipeWire capture, H.264 encoding, encryption, network delivery and
display. Gains depend on source dimensions, refresh rate and hardware. Run the
following on the target machine to reproduce the comparison:

```sh
python3 - <<'PY'
import resource
import statistics
import subprocess

source = [
    'gst-launch-1.0', '-q', 'videotestsrc', 'num-buffers=240',
    'pattern=black', '!',
    'video/x-raw,format=BGRx,width=3840,height=2160,framerate=60/1', '!',
]
target = [
    'video/x-raw,format=I420,width=1280,height=720,framerate=30/1',
    '!', 'fakesink', 'sync=false',
]
pipelines = {
    'previous': [
        'videoconvert', '!', 'videoscale', 'add-borders=true', '!',
        'videorate', '!',
    ],
    'rate_first': [
        'videorate', '!', 'video/x-raw,framerate=30/1', '!',
        'videoconvert', '!', 'videoscale', 'add-borders=true', '!',
    ],
    'current': [
        'videorate', '!', 'video/x-raw,framerate=30/1', '!',
        'videoconvertscale', 'add-borders=true', '!',
    ],
}
samples = {name: [] for name in pipelines}
for repeat in range(3):
    for name, filters in pipelines.items():
        before = resource.getrusage(resource.RUSAGE_CHILDREN)
        subprocess.run(source + filters + target, check=True, timeout=60)
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        cpu = (after.ru_utime + after.ru_stime
               - before.ru_utime - before.ru_stime)
        samples[name].append(cpu)
        print(f'{name} run={repeat + 1}: {cpu:.3f} CPU seconds', flush=True)
for name, values in samples.items():
    print(f'{name} median: {statistics.median(values):.3f} CPU seconds')
PY
```

The GStreamer regression test compares the real previous and current pipelines
on solid-color frames at 60→30, 30→60 and 60→60 fps, including a 4:3 source scaled
into 96×54 output. It checks byte-identical output, I420 format, dimensions,
pixel aspect ratio and frame cadence:

```sh
cargo test --offline --no-default-features \
  media::tests::screen_preprocessing_preserves_format_cadence_and_aspect_ratio \
  -- --ignored
```

Encoded video now retains one owned packet and borrows its NAL payloads instead
of allocating and copying each NAL. Packet metadata also reuses its line buffer
and parses fields without allocating a temporary collection. These changes are
covered by parser tests and the real FFmpeg encode/decode tests; the table above
does not include their effects.

## Packet processing and memory

Video assembly now writes into a reusable wire buffer and encrypts the payload
in place. Audio packs samples a byte at a time, encrypts in place, and recycles
the oldest packet buffer after the 512-packet retransmission history fills.
Retransmission requests index consecutive sequence numbers directly, including
across their 16-bit wrap, and reuse one response buffer per request.

The following comparison used the original helpers from commit `d77ce22` and the
updated helpers in a standalone `rustc -O` harness on 2026-09-16, linked to the
same optimized cryptography dependencies. Results are medians of 11 alternating
old/new batches after warmup; video inputs contain four NAL units. Buffers in the
new path were warmed and reused. These are illustrative component measurements,
excluding capture, H.264 encoding, network I/O and history queue operations.

| Work per packet | Before | After | Time reduction |
| --- | ---: | ---: | ---: |
| ALAC encoding | 1.731 µs | 0.717 µs | 59% |
| Audio encoding, assembly and encryption | 4.680 µs | 3.543 µs | 24% |
| Video assembly and encryption, 4 KiB | 4.708 µs | 4.485 µs | 5% |
| Video assembly and encryption, 64 KiB | 66.211 µs | 54.180 µs | 18% |
| Video assembly and encryption, 256 KiB | 310.029 µs | 211.416 µs | 32% |

Batch sizes were respectively 20,000, 8,000, 8,000, 1,000 and 300 iterations,
with quarter-batch warmups. Repeat measurements confirmed the direction of the
improvement; absolute timings vary with compiler settings and system load.

Separate allocator instrumentation found audio assembly/encryption went from
three allocations and two reallocations per packet to none after warmup. The
64 KiB video assembly/encryption workload went from three allocations and four
reallocations to none after warmup. Capture still allocates its incoming frames;
these counts describe the sender's packet assembly only.

The audio wire packet remains 1,452 bytes. Its backing buffer previously grew to
2,888 bytes; it now reserves 1,452. At 512 retained packets, backing-buffer
capacity falls from 1,478,656 to 743,424 bytes, saving approximately **718 KiB**
(49.7%), excluding queue metadata.

The maintained timing diagnostics exercise the current helpers directly:

```sh
cargo test --locked --no-default-features mirror::tests::measure_ \
  -- --ignored --nocapture --test-threads=1
```

Their absolute timings differ from the standalone optimized comparison because
Cargo uses the project's development/test profile. Protocol regression tests
compare ALAC output with the previous bit-packing algorithm for every 16-bit
sample value, decode audio with an independent decoder, verify encrypted packet
bytes and header authentication, and check retransmission lookup across wrap
and eviction.

## UI and validation

The UI receives the first streaming notification instead of identical status
updates every 30 video frames. This removes one or two unnecessary redraws per
second during sharing without changing the displayed state.

Validation passed: 15 normal tests, six ignored unit tests (including the timing
diagnostics), two real software encode/decode integration tests, the VA-API
encode/decode integration test at every quality preset, formatting, and Clippy
for all targets with GUI features and warnings denied. To run the runtime tests
on a machine without VA-API:

```sh
cargo test --locked --no-default-features -- --ignored --test-threads=1 \
  --skip hardware_frames_decode_at_every_quality_and_auto_detection_selects_gpu
```

## Live Apple TV validation

After enabling access to the host network, desktop services and GPU, live tests
passed on an AppleTV14,1 receiver on 2026-09-16. All used VA-API H.264 at 1080p/60
with the 50 ms playback buffer setting:

- A 30-second moving test pattern sustained 60 fps after startup; the viewer
  confirmed smooth playback on the TV.
- A 45-second desktop session exercised the actual sharing portal, PipeWire and
  optimized GStreamer preprocessing. Steady one-second frame counts ranged from
  58 to 62; the viewer confirmed smooth window movement. Peak queue and video
  send times were 203 ms and 217 ms, and the slowest control reply was 815 ms.
  The session reported no errors.
- A final 20-second session sent two generated tones through the normal system
  audio output. The listener confirmed both tones came from the TV. Video
  sustained 60 fps, with maximum queue/send times of 3/1 ms.

Every session exited successfully and cleaned up its capture and encoder
processes. The separate VA-API integration test also decoded all three quality
presets and confirmed automatic GPU selection. These live checks establish
functional playback, not a before/after whole-application CPU or battery-life
measurement.
