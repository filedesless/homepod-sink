//! Minimal standalone tool: play a given MP3 file to the (hardcoded) HomePod
//! at a given volume, using the same AirPlay 2 file-decoder path as
//! airplay2-rs's own play_audio example (Connection::connect_auto +
//! AudioDecoder + start_streaming). Doesn't touch PipeWire or the live
//! capture/stdin path at all - useful for a quick sanity check that the
//! AirPlay connection itself is healthy, independent of the capture pipeline.
//!
//! Usage: play <path-to-audio-file> [volume 0.0-1.0, default 1.0]

use homepod_sink::airplay::audio::{AlacEncoder, AudioDecoder};
use homepod_sink::airplay::client::Connection;
use homepod_sink::airplay::core::device::{Device, DeviceId};
use homepod_sink::airplay::core::features::Features;
use homepod_sink::airplay::core::stream::{PtpMode, StreamType, TimingProtocol};
use homepod_sink::airplay::core::{AudioFormat, StreamConfig};
use std::net::IpAddr;
use std::time::Duration;

const HOMEPOD_IP: &str = "192.168.0.13";
const HOMEPOD_PORT: u16 = 7000;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <path-to-audio-file> [volume 0.0-1.0]", args[0]);
        std::process::exit(1);
    }
    let audio_path = &args[1];
    let volume: f32 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(1.0);

    let ip: IpAddr = HOMEPOD_IP.parse()?;

    println!("Opening {}...", audio_path);
    let decoder = AudioDecoder::open(audio_path)?;
    println!(
        "Audio: {}Hz, {} channels",
        decoder.sample_rate(),
        decoder.channels()
    );

    let device = Device {
        id: DeviceId::from_mac_string("AA:BB:CC:00:11:22")?,
        name: "AirPlay Device".to_string(),
        model: "Unknown".to_string(),
        manufacturer: None,
        serial_number: None,
        addresses: vec![ip],
        port: HOMEPOD_PORT,
        features: Features::from_txt_value("0x4A7FCA00,0x3C354BD0").unwrap_or_default(),
        required_sender_features: None,
        public_key: None,
        source_version: Default::default(),
        firmware_version: None,
        os_version: None,
        protocol_version: None,
        requires_password: false,
        status_flags: 0,
        access_control: None,
        pairing_identity: None,
        system_pairing_identity: None,
        bluetooth_address: None,
        homekit_home_id: None,
        group_id: None,
        is_group_leader: false,
        group_public_name: None,
        group_contains_discoverable_leader: false,
        home_group_id: None,
        household_id: None,
        parent_group_id: None,
        parent_group_contains_discoverable_leader: false,
        tight_sync_id: None,
        raop_port: None,
        raop_encryption_types: None,
        raop_codecs: None,
        raop_transport: None,
        raop_metadata_types: None,
        raop_digest_auth: false,
        vodka_version: None,
    };

    let audio_format = AudioFormat::default(); // ALAC 44100/16-bit/stereo, 352 spf
    let temp_encoder = AlacEncoder::new(audio_format.clone())?;
    let asc = Some(temp_encoder.magic_cookie());

    let config = StreamConfig {
        stream_type: StreamType::Realtime,
        audio_format,
        timing_protocol: TimingProtocol::Ntp,
        ptp_mode: PtpMode::Master,
        latency_min: 22050,
        latency_max: 88200,
        supports_dynamic_stream_id: true,
        asc,
    };

    println!("Connecting to {}:{}...", ip, HOMEPOD_PORT);
    let mut conn = Connection::connect_auto(device, config, "3939").await?;
    println!("Connected!");

    conn.setup().await?;
    println!("Setup complete!");

    conn.start_streaming(decoder).await?;
    conn.set_volume(volume).await?;
    println!("Playing... (volume={:.2})", volume);

    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        println!(
            "Position: {:.1}s, State: {:?}",
            conn.playback_position(),
            conn.playback_state()
        );
    }
}
