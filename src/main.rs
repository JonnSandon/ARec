use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use crossbeam_channel::{bounded, Receiver};
use shine_rs::{Mp3Encoder, Mp3EncoderConfig, StereoMode, SUPPORTED_BITRATES, SUPPORTED_SAMPLE_RATES};
use std::{
    fs::File,
    io::Write,
    time::{Duration, Instant},
};
use wasapi::{
    initialize_mta, Device, DeviceEnumerator, Direction, SampleType, StreamMode,
    WaveFormat,
};

#[derive(Parser, Debug)]
#[command(name = "win-loopback-to-mp3")]
#[command(about = "Record Windows speaker output (WASAPI loopback) to MP3 (pure Rust encoder).", long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List active playback (render) devices
    List,

    /// Record speaker output to an MP3 file
    Record {
        /// Output mp3 path
        #[arg(short, long, default_value = "output.mp3")]
        out: String,

        /// Record duration seconds (0 = until Ctrl+C)
        #[arg(short = 't', long, default_value_t = 10)]
        seconds: u64,

        /// Select device by substring match on friendly name (case-insensitive).
        /// If omitted, uses default playback device.
        #[arg(short, long)]
        device: Option<String>,

        /// MP3 bitrate in kbps (must be supported by shine_rs)
        #[arg(short, long, default_value_t = 192)]
        kbps: u32,

        /// Force stereo output even if device has >2 channels (downmix).
        #[arg(long, default_value_t = true)]
        downmix_to_stereo: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // WASAPI requires COM; don't do this on a UI thread.
    // wasapi::initialize_mta returns an HRESULT, not a Result.
    let hr = initialize_mta();
    if hr.is_err() {
        bail!("initialize_mta failed: HRESULT={hr:?}");
    }

    match cli.cmd {
        Command::List => list_devices(),
        Command::Record {
            out,
            seconds,
            device,
            kbps,
            downmix_to_stereo,
        } => record_loopback_to_mp3(&out, seconds, device.as_deref(), kbps, downmix_to_stereo),
    }
}

fn list_devices() -> Result<()> {
    let enumerator = DeviceEnumerator::new()?;

    let default = enumerator.get_default_device(&Direction::Render)?;
    let default_id = default.get_id().unwrap_or_default();

    println!("Default render device (will be recorded if you don't pass --device):");
    println!(
        "  {}",
        default
            .get_friendlyname()
            .unwrap_or_else(|_| "<unknown>".to_string())
    );
    println!("  id: {default_id}");
    println!();

    let collection = enumerator.get_device_collection(&Direction::Render)?;
    println!("Render (playback) devices:");
    let count = collection.get_nbr_devices()?;
    for i in 0..count {
        let dev = collection.get_device_at_index(i)?;
        let name = dev.get_friendlyname().unwrap_or_else(|_| "<unknown>".to_string());
        let id = dev.get_id().unwrap_or_else(|_| "<unknown>".to_string());

        let mark = if id == default_id { "*" } else { " " };
        println!("  {mark}[{i}] {name}");
        println!("       id: {id}");
    }

    Ok(())
}




fn record_loopback_to_mp3(
    out_path: &str,
    seconds: u64,
    device_substring: Option<&str>,
    kbps: u32,
    downmix_to_stereo: bool,
) -> Result<()> {
    // Validate requested bitrate vs shine_rs supported list
    if !SUPPORTED_BITRATES.contains(&kbps) {
        bail!(
            "Unsupported bitrate {kbps} kbps for shine_rs. Supported: {:?}",
            SUPPORTED_BITRATES
        );
    }

    let enumerator = DeviceEnumerator::new()?;
    let device = select_render_device(&enumerator, device_substring)?;

    let device_name = device
        .get_friendlyname()
        .unwrap_or_else(|_| "<unknown>".to_string());
    println!("Using device: {device_name}");

    // Activate AudioClient on the chosen render device.
    // wasapi 0.22: get_iaudioclient (not get_audioclient).
    let mut audio_client = device.get_iaudioclient()?;

    // For loopback, the device mix format is always valid in shared mode.
    let mix = audio_client.get_mixformat()?;

    let mix_rate = mix.get_samplespersec() as usize;
    let mix_channels = mix.get_nchannels() as usize;


    // shine_rs only supports certain sample rates; make sure mix_rate is supported.
    if !SUPPORTED_SAMPLE_RATES.contains(&(mix_rate as u32)) {
        bail!(
            "Device mix sample rate {mix_rate} Hz not supported by shine_rs. Supported: {:?}",
            SUPPORTED_SAMPLE_RATES
        );
    }

    // We'll capture as 16-bit PCM interleaved to feed the MP3 encoder.
    // Keep the sample rate the same; optionally downmix to stereo in software.
    let target_channels = if downmix_to_stereo { 2 } else { mix_channels.min(2) };
    let desired = WaveFormat::new(
        16,               // storebits
        16,               // validbits
        &SampleType::Int, // i16
        mix_rate,
        mix_channels, // capture in device channel count; we can downmix later
        None,
    );

    // Shared, event-driven. Autoconvert lets the audio engine convert from endpoint format if needed.
    let buffer_duration_hns = 200_000; // 20ms
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns,
    };

    // Loopback capture: initialize a CAPTURE stream on a RENDER endpoint.
    audio_client
        .initialize_client(&desired, &Direction::Capture, &mode)
        .context("initialize_client (loopback) failed")?;

    let capture = audio_client.get_audiocaptureclient()?;
    let h_event = audio_client.set_get_eventhandle()?;

    // Prepare MP3 encoder
    let stereo_mode = if target_channels == 1 {
        StereoMode::Mono
    } else {
        StereoMode::Stereo
    };

    // shine-rs 0.1.3 fields: sample_rate, bitrate, channels, stereo_mode, ...
    let enc_cfg = Mp3EncoderConfig {
        sample_rate: mix_rate as u32,
        bitrate: kbps, // kbps
        channels: target_channels as u8,
        stereo_mode,
        ..Default::default()
    };

    let mut encoder = Mp3Encoder::new(enc_cfg).map_err(|e| anyhow!("mp3 encoder init: {e:?}"))?;

    let mut out = File::create(out_path).with_context(|| format!("create {out_path}"))?;

    // Ctrl+C handling
    let (stop_tx, stop_rx) = bounded::<()>(1);
    ctrlc::set_handler(move || {
        let _ = stop_tx.try_send(());
    })
    .context("failed to set Ctrl+C handler")?;

    println!(
        "Recording... {}",
        if seconds == 0 {
            "press Ctrl+C to stop".to_string()
        } else {
            format!("for {seconds}s")
        }
    );

    audio_client.start_stream()?;

    let start = Instant::now();

    // Reusable buffers to avoid per-packet allocations (important for long recordings)
    let bytes_per_sample = 2usize; // i16
    let bytes_per_frame = mix_channels * bytes_per_sample;

    // Raw bytes read from WASAPI
    let mut raw_buf: Vec<u8> = Vec::with_capacity(bytes_per_frame * 4096);

    // Decoded i16 samples (mix_channels interleaved)
    let mut pcm_buf: Vec<i16> = Vec::with_capacity(mix_channels * 4096);

    // Final samples given to encoder (target_channels interleaved)
    let mut enc_buf: Vec<i16> = Vec::with_capacity(target_channels * 4096);


    loop {
        if seconds != 0 && start.elapsed() >= Duration::from_secs(seconds) {
            break;
        }
        if stop_requested(&stop_rx) {
            break;
        }

        // Wait for event that indicates data is available
        h_event.wait_for_event(1000)?; // timeout ms

        // Drain all available packets
        loop {
            let next = capture.get_next_packet_size()?;
            let Some(frames_available) = next else { break; };
            if frames_available == 0 {
                break;
            }

            let needed = frames_available as usize * bytes_per_frame;

            // Ensure raw_buf is large enough, then read into it (no new allocation each time)
            if raw_buf.capacity() < needed {
                raw_buf.reserve(needed - raw_buf.capacity());
            }
            raw_buf.clear();
            raw_buf.resize(needed, 0u8);

            let (frames_read, _info) = capture
                .read_from_device(&mut raw_buf)
                .context("read_from_device failed")?;

            if frames_read == 0 {
                break;
            }

            let used_bytes = frames_read as usize * bytes_per_frame;

            // Decode bytes -> i16 into pcm_buf (reuse)
            let sample_count = frames_read as usize * mix_channels;
            if pcm_buf.capacity() < sample_count {
                pcm_buf.reserve(sample_count - pcm_buf.capacity());
            }
            pcm_buf.clear();

            for chunk in raw_buf[..used_bytes].chunks_exact(2) {
                pcm_buf.push(i16::from_le_bytes([chunk[0], chunk[1]]));
            }

            // Prepare encoder input into enc_buf (reuse)
            enc_buf.clear();

            if mix_channels == target_channels {
                // Fast path: no downmix, just copy
                enc_buf.extend_from_slice(&pcm_buf);
            } else if downmix_to_stereo && target_channels == 2 {
                // Downmix into enc_buf without allocating a new Vec each time
                downmix_n_to_stereo_into(&pcm_buf, mix_channels, &mut enc_buf);
            } else if target_channels == 1 {
                downmix_n_to_mono_into(&pcm_buf, mix_channels, &mut enc_buf);
            } else {
                take_first_two_channels_into(&pcm_buf, mix_channels, &mut enc_buf);
            }

            // Encode MP3
            let chunks = encoder
                .encode_interleaved(&enc_buf)
                .map_err(|e| anyhow!("encode error: {e:?}"))?;

            for c in chunks {
                out.write_all(&c)?;
            }
        }

    }

    audio_client.stop_stream()?;

    // Flush encoder tail
    let tail = encoder.finish().map_err(|e| anyhow!("finish error: {e:?}"))?;
    out.write_all(&tail)?;
    out.flush()?;

    println!("Saved: {out_path}");
    Ok(())
}

fn stop_requested(rx: &Receiver<()>) -> bool {
    rx.try_recv().is_ok()
}

fn select_render_device(enumerator: &DeviceEnumerator, needle: Option<&str>) -> Result<Device> {
    if let Some(needle) = needle {
        let needle = needle.to_lowercase();
        let collection = enumerator.get_device_collection(&Direction::Render)?;
        let count = collection.get_nbr_devices()?;
        for i in 0..count {
            let dev = collection.get_device_at_index(i)?;
            let name = dev.get_friendlyname().unwrap_or_default().to_lowercase();
            if name.contains(&needle) {
                return Ok(dev);
            }
        }
        bail!("No render device matched substring: {needle}");
    }

    Ok(enumerator.get_default_device(&Direction::Render)?)
}

fn downmix_n_to_stereo(interleaved: &[i16], channels: usize) -> Vec<i16> {
    // Simple “energy-ish” downmix:
    // L = average of even-ish set, R = average of odd-ish set.
    // (For real use, use channel masks & a proper downmix matrix.)
    let frames = interleaved.len() / channels;
    let mut out = Vec::with_capacity(frames * 2);

    for f in 0..frames {
        let base = f * channels;
        let mut l_acc: i32 = 0;
        let mut r_acc: i32 = 0;
        let mut l_n: i32 = 0;
        let mut r_n: i32 = 0;

        for ch in 0..channels {
            let s = interleaved[base + ch] as i32;
            if ch % 2 == 0 {
                l_acc += s;
                l_n += 1;
            } else {
                r_acc += s;
                r_n += 1;
            }
        }

        let l = (l_acc / l_n.max(1)).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let r = (r_acc / r_n.max(1)).clamp(i16::MIN as i32, i16::MAX as i32) as i16;

        out.push(l);
        out.push(r);
    }

    out
}

fn downmix_n_to_mono(interleaved: &[i16], channels: usize) -> Vec<i16> {
    let frames = interleaved.len() / channels;
    let mut out = Vec::with_capacity(frames);
    for f in 0..frames {
        let base = f * channels;
        let mut acc: i32 = 0;
        for ch in 0..channels {
            acc += interleaved[base + ch] as i32;
        }
        let m = (acc / channels as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        out.push(m);
    }
    out
}

fn take_first_two_channels(interleaved: &[i16], channels: usize) -> Vec<i16> {
    let frames = interleaved.len() / channels;
    let mut out = Vec::with_capacity(frames * 2);
    for f in 0..frames {
        let base = f * channels;
        let l = interleaved[base];
        let r = interleaved[base + 1.min(channels - 1)];
        out.push(l);
        out.push(r);
    }
    out
}
