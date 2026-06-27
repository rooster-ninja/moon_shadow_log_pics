use serde::Deserialize;

fn default_quality() -> u8 { 10 }
fn default_gain_ceiling() -> u8 { 0 }
fn default_flash() -> bool { true }
fn default_utc_offset() -> i32 { 0 }
fn default_stack_secs() -> u32 { 0 }
fn default_framesize() -> u8 { 9 }  // SVGA 800×600

#[derive(Deserialize, Clone)]
pub struct Config {
    pub broker:           String,
    pub port:             u16,
    pub mqtt_user:        String,
    pub mqtt_pass:        String,
    pub client_id:        String,
    pub sensor_id:        String,
    pub interval:         u64,
    pub start_time:       String,   // "HH:MM" local
    pub stop_time:        String,   // "HH:MM" local
    pub exposure:         i32,      // -1 = auto, 0-1200
    pub gain:             i32,      // -1 = auto, 0-30
    #[serde(default = "default_quality")]
    pub quality:          u8,       // 1-63 (lower = better)
    #[serde(default = "default_gain_ceiling")]
    pub gain_ceiling:     u8,       // 0-6
    #[serde(default = "default_flash")]
    pub flash:            bool,
    #[serde(default = "default_utc_offset")]
    pub utc_offset_hours: i32,      // e.g. -7 for UTC-7 (MDT)
    #[serde(default = "default_stack_secs")]
    pub stack_secs:       u32,      // 0 = single frame, 1-10 = stacking duration
    #[serde(default = "default_framesize")]
    pub framesize:        u8,       // 9=SVGA … 13=UXGA
    pub log_dir:          String,
    pub http_port:        u16,
}

pub fn load(path: &std::path::Path) -> anyhow::Result<Config> {
    let text = std::fs::read_to_string(path)?;
    Ok(toml::from_str(&text)?)
}

/// Parse "HH:MM" into (hour, minute).
pub fn parse_hhmm(s: &str) -> anyhow::Result<(u32, u32)> {
    let parts: Vec<&str> = s.splitn(2, ':').collect();
    anyhow::ensure!(parts.len() == 2, "expected HH:MM, got: {s}");
    Ok((parts[0].parse()?, parts[1].parse()?))
}
