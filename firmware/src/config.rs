/// Flash-backed device configuration.
///
/// Stored in the `config` partition (offset 0x3F0000, 4 KB) as:
///   [magic: 4 B][length: 4 B LE][JSON payload]
///
/// To provision: run tools/provision.py --flash
use serde::{Deserialize, Serialize};

const MAGIC: [u8; 4] = [0xFA, 0x12, 0xC3, 0x7A];
const HEADER_SIZE: usize = 8;
const MAX_JSON: usize = 512;
const SECTOR_SIZE: usize = 4096;
const PARTITION_NAME: &[u8] = b"config\0";

fn default_flash() -> bool { true }

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub wifi_ssid:    String,
    pub wifi_pass:    String,
    pub mqtt_host:    String,
    pub mqtt_port:    u16,
    pub mqtt_user:    String,
    pub mqtt_pass:    String,
    pub device_id:    String,
    // Camera defaults (can be changed via moon-shadow/cmd)
    pub framesize:    u8,    // 6=SVGA(800x600), 8=XGA, 10=UXGA
    pub jpeg_quality: u8,    // 0-63; lower = better (OV2640 convention)
    pub gain_ceiling: u8,    // 0=2x, 1=4x, 2=8x … 6=128x max auto gain
    #[serde(default = "default_flash")]
    pub flash:        bool,  // fire LED during capture
    // HTTP upload destination
    pub upload_host:  String,
    pub upload_port:  u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wifi_ssid:    String::new(),
            wifi_pass:    String::new(),
            mqtt_host:    String::new(),
            mqtt_port:    1883,
            mqtt_user:    String::new(),
            mqtt_pass:    String::new(),
            device_id:    String::new(),
            framesize:    6,
            jpeg_quality: 10,
            gain_ceiling: 0,
            flash:        true,
            upload_host:  String::new(),
            upload_port:  8765,
        }
    }
}

fn find_partition() -> *const esp_idf_sys::esp_partition_t {
    unsafe {
        esp_idf_sys::esp_partition_find_first(
            esp_idf_sys::esp_partition_type_t_ESP_PARTITION_TYPE_DATA,
            esp_idf_sys::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_ANY,
            PARTITION_NAME.as_ptr() as *const _,
        )
    }
}

pub fn load() -> Option<Config> {
    let partition = find_partition();
    if partition.is_null() {
        log::error!("config partition not found");
        return None;
    }

    let mut buf = [0u8; HEADER_SIZE + MAX_JSON];
    let err = unsafe {
        esp_idf_sys::esp_partition_read(
            partition, 0,
            buf.as_mut_ptr() as *mut _,
            buf.len(),
        )
    };
    if err != 0 {
        log::error!("partition read failed: {}", err);
        return None;
    }

    if buf[..4] != MAGIC {
        return None;
    }

    let len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
    if len == 0 || len > MAX_JSON {
        return None;
    }

    match serde_json::from_slice(&buf[HEADER_SIZE..HEADER_SIZE + len]) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            log::error!("config JSON parse error: {e}");
            None
        }
    }
}

pub fn save(cfg: &Config) -> anyhow::Result<()> {
    let json = serde_json::to_string(cfg)?;
    let json_bytes = json.as_bytes();
    anyhow::ensure!(json_bytes.len() <= MAX_JSON, "config JSON too large");

    let partition = find_partition();
    anyhow::ensure!(!partition.is_null(), "config partition not found");

    // Stack-allocated write buffer — esp_partition_write requires the source to be
    // in internal DRAM (not PSRAM); heap allocation may land in PSRAM when DRAM is
    // tight, causing the write to fail silently after erasing the sector.
    // The main-task stack is always in DRAM on ESP32.
    let mut buf = [0xFFu8; HEADER_SIZE + MAX_JSON + 4];
    buf[..4].copy_from_slice(&MAGIC);
    buf[4..8].copy_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    buf[HEADER_SIZE..HEADER_SIZE + json_bytes.len()].copy_from_slice(json_bytes);
    let aligned = (HEADER_SIZE + json_bytes.len() + 3) & !3;

    let err = unsafe { esp_idf_sys::esp_partition_erase_range(partition, 0, SECTOR_SIZE) };
    anyhow::ensure!(err == 0, "partition erase failed: 0x{:X}", err);

    let err = unsafe {
        esp_idf_sys::esp_partition_write(partition, 0, buf.as_ptr() as *const _, aligned)
    };
    anyhow::ensure!(err == 0, "partition write failed: 0x{:X}", err);

    Ok(())
}
