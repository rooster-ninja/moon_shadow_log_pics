mod camera;
mod config;
mod types;

use anyhow::Context;
use chrono::Utc;
use esp_idf_hal::gpio::{Output, PinDriver};
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::mqtt::client::{EspMqttClient, EventPayload, MqttClientConfiguration, QoS};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sntp::{EspSntp, SyncStatus};
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use types::{ConfigCmd, SendDataCmd, StatusMessage};

fn main() -> anyhow::Result<()> {
    esp_idf_svc::log::EspLogger::initialize_default();
    log::info!("=== moon_shadow_photo ===");

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // GPIO33 — onboard red indicator LED, active LOW; drive HIGH to suppress it
    let mut _red_led = PinDriver::output(peripherals.pins.gpio33)?;
    _red_led.set_high()?;
    // GPIO4 — white flash LED, active HIGH
    let mut led = PinDriver::output(peripherals.pins.gpio4)?;

    let cfg = config::load().unwrap_or_else(|| {
        panic!(
            "No config found. Flash with:\n  \
             espflash erase-region 0x3F0000 0x1000\n  \
             espflash write-bin 0x3F0000 tools/config.bin"
        )
    });

    log::info!("Device ID : {}", cfg.device_id);
    log::info!("MQTT      : {}:{}", cfg.mqtt_host, cfg.mqtt_port);
    log::info!("Upload    : {}:{}", cfg.upload_host, cfg.upload_port);
    log::info!("Framesize : {}  Quality: {}  GainCeil: {}", cfg.framesize, cfg.jpeg_quality, cfg.gain_ceiling);

    // WiFi
    log::info!("Starting WiFi (SSID: {})…", cfg.wifi_ssid);
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?,
        sys_loop,
    )?;
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid:        cfg.wifi_ssid.as_str().try_into().unwrap(),
        password:    cfg.wifi_pass.as_str().try_into().unwrap(),
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    }))?;
    wifi.start()?;
    log::info!("WiFi started, connecting…");
    loop {
        match wifi.connect() {
            Ok(_) => break,
            Err(e) => {
                log::warn!("WiFi connect failed ({e}), retrying in 5s…");
                std::thread::sleep(Duration::from_secs(5));
                let _ = wifi.disconnect();
            }
        }
    }
    wifi.wait_netif_up()?;
    // Disable modem sleep — beacon timeouts disconnect the session when the radio
    // sleeps through AP keepalives on a battery-less always-on device.
    unsafe { esp_idf_sys::esp_wifi_set_ps(esp_idf_sys::wifi_ps_type_t_WIFI_PS_NONE); }
    log::info!("WiFi up (power save disabled)");
    flash_led(&mut led, 2, 200, 200);

    // SNTP time sync (needed for timestamps in filenames)
    let sntp = EspSntp::new_default()?;
    log::info!("SNTP sync…");
    while sntp.get_sync_status() != SyncStatus::Completed {
        std::thread::sleep(Duration::from_millis(200));
    }
    log::info!("Time synced: {}", Utc::now().format("%Y-%m-%d %H:%M:%S UTC"));

    log::info!("Boot complete — entering MQTT loop");

    // Camera is initialised lazily on first MQTT connect so DMA activity
    // doesn't interfere with the WiFi/TCP handshake during startup.
    let mut cam_ready = false;

    // MQTT + session loop
    loop {
        if let Err(e) = run_session(&cfg, &mut led, &mut cam_ready) {
            log::error!("Session error: {e}");
            flash_led(&mut led, 8, 80, 80);  // rapid burst = critical error
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn flash_led(led: &mut PinDriver<'_, Output>, count: u8, on_ms: u64, gap_ms: u64) {
    for _ in 0..count {
        let _ = led.set_high();
        std::thread::sleep(Duration::from_millis(on_ms));
        let _ = led.set_low();
        std::thread::sleep(Duration::from_millis(gap_ms));
    }
}

fn run_session(
    cfg: &config::Config,
    led: &mut PinDriver<'_, Output>,
    cam_ready: &mut bool,
) -> anyhow::Result<()> {
    let url = format!("mqtt://{}:{}", cfg.mqtt_host, cfg.mqtt_port);
    let mqtt_conf = MqttClientConfiguration {
        client_id: Some(&cfg.device_id),
        username:  if cfg.mqtt_user.is_empty() { None } else { Some(&cfg.mqtt_user) },
        password:  if cfg.mqtt_pass.is_empty() { None } else { Some(&cfg.mqtt_pass) },
        keep_alive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    };

    let senddata_topic = format!("{}/senddata", cfg.device_id);
    let cmd_topic      = format!("{}/cmd",      cfg.device_id);
    let status_topic   = format!("{}/status",   cfg.device_id);
    let hello_topic    = format!("{}/hello",    cfg.device_id);

    log::info!("MQTT connecting to {}…", url);
    let (mut client, mut connection) = EspMqttClient::new(&url, &mqtt_conf)
        .context("MQTT client init")?;

    // Events the main thread needs to act on (owned data, safe to send across threads)
    enum Ev {
        Connected,
        Received { topic: String, data: Vec<u8> },
        Disconnected,
        Fatal,
    }

    let (tx, rx) = mpsc::channel::<Ev>();

    // Pump the event loop on a background thread.  subscribe()/publish() block
    // waiting for SUBACK/PUBACK which can only arrive through connection.next(),
    // so the two must run concurrently.
    std::thread::Builder::new()
        .stack_size(16384)
        .spawn(move || {
            loop {
                match connection.next() {
                    Ok(event) => {
                        let ev = match event.payload() {
                            EventPayload::Connected(_) => Some(Ev::Connected),
                            EventPayload::Disconnected => Some(Ev::Disconnected),
                            EventPayload::Error(_)     => Some(Ev::Fatal),
                            EventPayload::Received { topic: Some(t), data, .. } =>
                                Some(Ev::Received { topic: t.to_string(), data: data.to_vec() }),
                            _ => None,
                        };
                        // event dropped here before sending
                        if let Some(ev) = ev {
                            let fatal = matches!(ev, Ev::Fatal);
                            let _ = tx.send(ev);
                            if fatal { break; }
                        }
                    }
                    Err(_) => { let _ = tx.send(Ev::Fatal); break; }
                }
            }
        })?;

    let mut last_status = Instant::now();

    loop {
        match rx.recv().map_err(|_| anyhow::anyhow!("MQTT event thread exited"))? {
            Ev::Connected => {
                log::info!("MQTT connected");
                if !*cam_ready {
                    log::info!("Camera init (first connect)…");
                    camera::init(cfg.framesize, cfg.jpeg_quality)?;
                    *cam_ready = true;
                    log::info!("Camera ready");
                }
                client.subscribe(&senddata_topic, QoS::AtMostOnce)?;
                client.subscribe(&cmd_topic,      QoS::AtMostOnce)?;
                log::info!("Subscribed OK");

                let hello = serde_json::json!({
                    "device_id": cfg.device_id,
                    "msg": "booted",
                    "framesize": cfg.framesize,
                    "quality": cfg.jpeg_quality,
                });
                client.publish(&hello_topic, QoS::AtMostOnce, true,
                    hello.to_string().as_bytes())?;
                log::info!("Published hello");
                publish_status(&mut client, &status_topic, "online", cfg)?;
                flash_led(led, 3, 200, 200);
            }
            Ev::Received { topic, data } => {
                if topic == senddata_topic {
                    match serde_json::from_slice::<SendDataCmd>(&data) {
                        Ok(cmd) => {
                            log::info!("▶ capture: exposure={}, gain={}, stack_secs={}",
                                       cmd.exposure, cmd.gain, cmd.stack_secs);
                            camera::apply_sensor(cmd.exposure, cmd.gain);
                            std::thread::sleep(Duration::from_millis(150));

                            let jpeg = if cmd.stack_secs > 0 {
                                // Measure fps at the current exposure setting
                                let fps = camera::measure_fps(3).max(0.1);
                                let n_frames = ((cmd.stack_secs as f32 * fps).round() as u32).max(1);
                                log::info!("Stacking {} frames at {:.2} fps ≈ {:.1}s",
                                           n_frames, fps, n_frames as f32 / fps);
                                camera::capture_stacked_jpeg(
                                    n_frames, cfg.framesize, cfg.jpeg_quality,
                                    cmd.exposure, cmd.gain,
                                )
                            } else {
                                camera::capture_jpeg()
                            };

                            // Flash off immediately after capture, before upload
                            let _ = led.set_low();

                            match jpeg {
                                Ok(jpeg) => {
                                    log::info!("Capture OK: {} B — uploading to {}:{}",
                                               jpeg.len(), cfg.upload_host, cfg.upload_port);
                                    let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
                                    if let Err(e) = http_post_jpeg(cfg, &jpeg, &ts) {
                                        log::error!("HTTP upload failed: {e}");
                                    }
                                }
                                Err(e) => {
                                    log::error!("Capture failed: {e}");
                                    camera::recover(cfg.framesize, cfg.jpeg_quality);
                                }
                            }
                        }
                        Err(e) => log::warn!("Bad senddata JSON: {e}"),
                    }
                } else if topic == cmd_topic {
                    if let Ok(cmd) = serde_json::from_slice::<ConfigCmd>(&data) {
                        handle_config_cmd(cmd);
                    }
                }
            }
            Ev::Disconnected => log::warn!("MQTT disconnected — waiting for reconnect"),
            Ev::Fatal        => return Err(anyhow::anyhow!("MQTT error / connection lost")),
        }

        if last_status.elapsed() >= Duration::from_secs(60) {
            publish_status(&mut client, &status_topic, "alive", cfg)?;
            last_status = Instant::now();
        }
    }
}

fn publish_status(
    client: &mut EspMqttClient<'_>,
    topic: &str,
    status: &str,
    cfg: &config::Config,
) -> anyhow::Result<()> {
    let msg = StatusMessage {
        status,
        framesize:    cfg.framesize,
        quality:      cfg.jpeg_quality,
        gain_ceiling: cfg.gain_ceiling,
        upload_host:  &cfg.upload_host,
    };
    let payload = serde_json::to_vec(&msg)?;
    client.publish(topic, QoS::AtMostOnce, false, &payload)?;
    Ok(())
}

fn handle_config_cmd(cmd: ConfigCmd) {
    // Runtime cmd messages update in-memory camera state only — no flash write.
    // Config partition is write-once (provisioned via provision.py).
    // Writing flash at runtime risks leaving the partition blank if a reset
    // occurs between the erase and write, causing a boot-loop on next power-on.
    match cmd.cmd.as_str() {
        "SetFramesize" => {
            if let Some(v) = cmd.value.and_then(|v| v.as_u64()) {
                log::info!("SetFramesize → {} (takes effect on next capture reinit)", v);
            }
        }
        "SetQuality" => {
            if let Some(v) = cmd.value.and_then(|v| v.as_u64()) {
                log::info!("SetQuality → {} (takes effect on next capture reinit)", v);
            }
        }
        "SetGainCeiling" => {
            if let Some(v) = cmd.value.and_then(|v| v.as_u64()) {
                camera::set_gain_ceiling(v as u8);
                log::info!("SetGainCeiling → {}", v);
            }
        }
        _ => log::warn!("Unknown cmd: {}", cmd.cmd),
    }
}

/// HTTP POST the JPEG to the logger's upload endpoint.
/// Uses a raw TCP connection — no TLS, local network only.
fn http_post_jpeg(cfg: &config::Config, jpeg: &[u8], timestamp: &str) -> anyhow::Result<()> {
    let addr = format!("{}:{}", cfg.upload_host, cfg.upload_port);
    let mut stream = TcpStream::connect(&addr)
        .with_context(|| format!("TCP connect to {addr}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();

    let header = format!(
        "POST /upload HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Content-Type: image/jpeg\r\n\
         X-Timestamp: {timestamp}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        jpeg.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(jpeg)?;

    // Read status line to confirm receipt
    let mut resp = [0u8; 64];
    let n = stream.read(&mut resp).unwrap_or(0);
    let status_line = std::str::from_utf8(&resp[..n]).unwrap_or("").lines().next().unwrap_or("");
    log::info!("Upload response: {}", status_line);

    Ok(())
}
