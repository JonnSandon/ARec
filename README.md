# ARec

Windows Audio Loopback Recorder that captures the system render (speaker) output using WASAPI loopback and writes MP3 via a pure-Rust encoder.

Author: Jonn Sandon (jonn42@gmail.com)
Development date: 2026-02-02

## What it does

1. Enumerates Windows render devices (speakers / headphones).
2. Captures loopback audio in shared, event-driven mode.
3. Converts raw PCM bytes to i16 samples.
4. Optionally downmixes multi-channel audio to stereo or mono.
5. Encodes to MP3 (shine-rs) and writes the output file.

## Build and run

Build:

```powershell
cargo build --release
```

Run the binary directly (from the project root):

```powershell
.\target\release\ARec.exe list
```

Record for 10 seconds (default):

```powershell
.\target\release\ARec.exe record --out output.mp3 --seconds 10
```

Record until Ctrl+C:

```powershell
.\target\release\ARec.exe record --seconds 0
```

Select device by substring (case-insensitive):

```powershell
.\target\release\ARec.exe record --device "headphones" --out out.mp3
```

Set MP3 bitrate (kbps) and disable downmix:

```powershell
.\target\release\ARec.exe record --kbps 192 --downmix-to-stereo false
```

Using `cargo run` (development):

List devices:

```powershell
cargo run --release -- list
```

Record for 10 seconds (default):

```powershell
cargo run --release -- record --out output.mp3 --seconds 10
```

Record until Ctrl+C:

```powershell
cargo run --release -- record --seconds 0
```

Select device by substring (case-insensitive):

```powershell
cargo run --release -- record --device "headphones" --out out.mp3
```

Set MP3 bitrate (kbps) and disable downmix:

```powershell
cargo run --release -- record --kbps 192 --downmix-to-stereo false
```

## CLI reference

Binary name: `ARec`

Subcommands:

1. `list`
2. `record`

`record` arguments:

1. `--out`, `-o`: output path (default `output.mp3`).
2. `--seconds`, `-t`: recording duration in seconds (0 = until Ctrl+C). Default 10.
3. `--device`, `-d`: substring to match device friendly name. Default is the system default render device.
4. `--kbps`, `-k`: MP3 bitrate in kbps. Must be one of `shine-rs` supported bitrates.
5. `--downmix-to-stereo`: if true, downmixes to 2 channels even if the device has more channels.

## Design overview

This project is a single binary in `src/main.rs`. The design is intentionally linear to minimize latency and allocations during capture.

Key design choices:

1. WASAPI loopback capture in shared event-driven mode to reduce polling and CPU usage.
2. Capture in device mix format and validate against encoder supported sample rates.
3. Reuse buffers to avoid per-packet allocations for long recordings.
4. Downmix in Rust when the device has more than two channels.
5. MP3 encoding via `shine-rs`, which expects interleaved i16 PCM.

Data flow summary:

1. Select a render device (`select_render_device`).
2. Initialize WASAPI capture (`record_loopback_to_mp3`).
3. Wait for event signaling available audio (`h_event.wait_for_event`).
4. Read raw bytes into `raw_buf`.
5. Convert to i16 in `pcm_buf`.
6. Downmix or pass through into `enc_buf`.
7. Encode to MP3 and write to file.
8. On stop, flush encoder tail and print summary.

## Detailed code documentation

This section documents every function and every loop. All code lives in `src/main.rs`.

### `main` function

Purpose:

1. Parse CLI arguments.
2. Initialize COM for WASAPI.
3. Dispatch to `list_devices` or `record_loopback_to_mp3`.

Logic details:

1. `Cli::parse()` uses `clap` to parse command-line arguments into the `Cli` struct.
2. `initialize_mta()` is required for WASAPI and COM on a non-UI thread. If it fails, execution stops.
3. The `match` on `cli.cmd` calls the appropriate subcommand function.

### `list_devices` function

Purpose:

1. Print the default render device.
2. List all render devices with their friendly names and IDs.

Logic details:

1. Create `DeviceEnumerator`.
2. Call `get_default_device` with `Direction::Render` to obtain the default playback device.
3. Print the default device name and ID.
4. Get the render device collection.
5. Loop from `0..count` to fetch each device by index.
6. Print each device with a mark (`*`) if it matches the default ID.

Loop details:

1. `for i in 0..count` enumerates each render device by index.
2. Each iteration reads the device, gets its friendly name and ID, and prints them.

### `record_loopback_to_mp3` function

Purpose:

1. Validate bitrate and sample rate.
2. Initialize loopback capture on the chosen render device.
3. Capture audio, downmix if needed, encode to MP3, and write output.

Logic details:

1. Validate `kbps` against `shine-rs` supported bitrates.
2. Call `select_render_device` to find the device by substring or default.
3. Acquire `IAudioClient` via `get_iaudioclient`.
4. Read the device mix format and sample rate.
5. Validate sample rate against `shine-rs` supported sample rates.
6. Create a `WaveFormat` for 16-bit PCM in the device channel count.
7. Initialize the audio client for loopback capture in shared, event-driven mode.
8. Create `Mp3EncoderConfig` based on mix rate, bitrate, and target channels.
9. Open the output file.
10. Install Ctrl+C handler to request stop.
11. Start the audio stream and enter the main capture loop.
12. On exit, stop the stream, flush the encoder, and print statistics.

Key buffers and their roles:

1. `raw_buf`: raw bytes read from WASAPI.
2. `pcm_buf`: decoded i16 samples in device channel order.
3. `enc_buf`: samples ready for encoding (possibly downmixed).

Loop details:

Main loop (outer loop):

1. Checks for time limit and Ctrl+C stop requests.
2. Updates the status line once per second.
3. Waits for an audio event signaling data availability.
4. Enters the inner loop to drain all available packets.

Inner loop (packet drain loop):

1. Calls `get_next_packet_size` to see if frames are available.
2. Breaks when there are no more frames.
3. Ensures buffers are large enough for the packet.
4. Reads frames into `raw_buf`.
5. Converts bytes to i16 samples in `pcm_buf`.
6. Produces `enc_buf` by copying or downmixing.
7. Encodes `enc_buf` to MP3 frames and writes them to the output file.

Loop details for conversion and downmix:

1. `for chunk in raw_buf[..used_bytes].chunks_exact(2)` converts 2-byte little-endian samples to `i16` and pushes into `pcm_buf`.
2. Downmix loops inside helper functions are described below.

### `stop_requested` function

Purpose:

1. Poll the Ctrl+C channel without blocking.
2. Return `true` if a stop request was sent.

Logic details:

1. Uses `try_recv` to check for a queued stop signal.
2. If a message is present, returns `true`.

### `select_render_device` function

Purpose:

1. Select a render device by name substring.
2. Fallback to the default render device if no substring was provided.

Logic details:

1. If `needle` is provided, convert to lowercase for case-insensitive matching.
2. Enumerate render devices.
3. Loop through devices and compare friendly name strings.
4. Return the first device whose name contains the substring.
5. If no device matches, return an error.
6. If `needle` is not provided, return the default render device.

Loop details:

1. `for i in 0..count` enumerates devices by index and checks for a substring match.

### `downmix_n_to_stereo_into` function

Purpose:

1. Convert N-channel interleaved audio to stereo.
2. Preserve per-channel balance by averaging even and odd channels separately.

Logic details:

1. Compute frame count from `input.len() / channels`.
2. For each frame:
3. Accumulate samples for left (even channels) and right (odd channels).
4. Average each side and clamp to `i16` range.
5. Push left and right samples to `out`.

Loop details:

1. `for f in 0..frames` iterates over frames.
2. `for ch in 0..channels` accumulates per-channel samples for the current frame.

### `downmix_n_to_mono_into` function

Purpose:

1. Convert N-channel interleaved audio to mono.
2. Average all channels per frame and clamp to `i16` range.

Logic details:

1. Compute frame count.
2. For each frame, sum all channel samples.
3. Divide by channel count to get the average.
4. Clamp and push to `out`.

Loop details:

1. `for f in 0..frames` iterates over frames.
2. `for ch in 0..channels` sums channels for the current frame.

### `take_first_two_channels_into` function

Purpose:

1. Select only the first two channels from N-channel input.
2. Preserve left/right ordering when possible.

Logic details:

1. Compute frame count.
2. For each frame, take channel 0 as left and channel 1 as right.
3. If only one channel exists, reuse channel 0 for right.

Loop details:

1. `for f in 0..frames` iterates over frames and pushes two samples per frame.

### `human_bytes` function

Purpose:

1. Convert a byte count into a human-readable string.

Logic details:

1. Define constants for KiB, MiB, GiB.
2. Compare the input size against thresholds and format accordingly.

### `print_status_line` function

Purpose:

1. Update a single-line status indicator in the console without emitting newlines.

Logic details:

1. Print a carriage return to return to the start of the line.
2. Pad the line to overwrite any previous longer text.
3. Flush stdout to ensure immediate display.

## Notes and limitations

1. Works only on Windows due to WASAPI loopback capture.
2. `shine-rs` supports only specific bitrates and sample rates.
3. Large recordings rely on buffer reuse to minimize allocations.

## File layout

1. `src/main.rs`: all code, CLI, capture, downmix, and encoding.
3. `Cargo.toml`: package metadata and dependencies.
4. `Cargo.lock`: dependency lockfile.
