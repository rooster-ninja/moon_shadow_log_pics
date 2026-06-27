/// OV2640 camera driver wrapper for AI-Thinker ESP32-CAM-MB.
///
/// Pin map (AI-Thinker board):
///   PWDN=32, RESET=-1, XCLK=0, SIOD=26, SIOC=27
///   D7=35, D6=34, D5=39, D4=36, D3=21, D2=19, D1=18, D0=5
///   VSYNC=25, HREF=23, PCLK=22, FLASH_LED=4

use esp_idf_sys::{ledc_channel_t_LEDC_CHANNEL_0, ledc_timer_t_LEDC_TIMER_0, ESP_OK};

const PIN_PWDN:  i32 = 32;
const PIN_RESET: i32 = -1;
const PIN_XCLK:  i32 = 0;
const PIN_SIOD:  i32 = 26;
const PIN_SIOC:  i32 = 27;
const PIN_D7:    i32 = 35;
const PIN_D6:    i32 = 34;
const PIN_D5:    i32 = 39;
const PIN_D4:    i32 = 36;
const PIN_D3:    i32 = 21;
const PIN_D2:    i32 = 19;
const PIN_D1:    i32 = 18;
const PIN_D0:    i32 = 5;
const PIN_VSYNC: i32 = 25;
const PIN_HREF:  i32 = 23;
const PIN_PCLK:  i32 = 22;

// After esp_camera_deinit(), GPIO32 (PWDN) is released to floating high-Z.
// The AI-Thinker board has no external pullup, so OV2640 never fully power-cycles
// and the probe on reinit reads stale grayscale-mode registers → ESP_ERR_NOT_SUPPORTED.
// Drive PWDN HIGH explicitly so the sensor resets during the recovery delay.
fn hold_pwdn_high() {
    unsafe {
        let _ = esp_idf_sys::gpio_set_direction(
            PIN_PWDN, esp_idf_sys::gpio_mode_t_GPIO_MODE_OUTPUT,
        );
        let _ = esp_idf_sys::gpio_set_level(PIN_PWDN, 1);
    }
}

// ── Manual FFI for espressif/esp32-camera ────────────────────────────────────
// bindgen fails on macOS because esp_camera.h → driver/ledc.h exceeds clang's
// default include-depth limit and Apple clang doesn't support -fmax-include-depth.
// All sizes/offsets verified against the actual headers in the managed component.

// pixformat_t (sensor.h)
pub const PIXFORMAT_GRAYSCALE: u32 = 3;
pub const PIXFORMAT_JPEG:      u32 = 4;
// camera_fb_location_t (esp_camera.h)
pub const CAMERA_FB_IN_PSRAM:    u32 = 0;
// camera_grab_mode_t (esp_camera.h)
pub const CAMERA_GRAB_WHEN_EMPTY: u32 = 0;
pub const CAMERA_GRAB_LATEST:     u32 = 1;

// MALLOC_CAP_SPIRAM = (1 << 10)
const MALLOC_CAP_SPIRAM: u32 = 0x400;

/// camera_config_t
#[repr(C)]
pub struct CameraConfig {
    pub pin_pwdn:      i32,
    pub pin_reset:     i32,
    pub pin_xclk:      i32,
    pub pin_sccb_sda:  i32,
    pub pin_sccb_scl:  i32,
    pub pin_d7:        i32,
    pub pin_d6:        i32,
    pub pin_d5:        i32,
    pub pin_d4:        i32,
    pub pin_d3:        i32,
    pub pin_d2:        i32,
    pub pin_d1:        i32,
    pub pin_d0:        i32,
    pub pin_vsync:     i32,
    pub pin_href:      i32,
    pub pin_pclk:      i32,
    pub xclk_freq_hz:  i32,
    pub ledc_timer:    u32,
    pub ledc_channel:  u32,
    pub pixel_format:  u32,
    pub frame_size:    u32,
    pub jpeg_quality:  i32,
    pub fb_count:      usize,
    pub fb_location:   u32,
    pub grab_mode:     u32,
    pub sccb_i2c_port: i32,
}

/// camera_fb_t
#[repr(C)]
pub struct CameraFb {
    pub buf:       *mut u8,
    pub len:       usize,
    pub width:     usize,
    pub height:    usize,
    pub format:    u32,
    pub timestamp: [i32; 2],
}

/// sensor_id_t
#[repr(C)]
pub struct SensorId {
    pub midh: u8,
    pub midl: u8,
    pub pid:  u16,
    pub ver:  u8,
}

/// camera_status_t
#[repr(C)]
pub struct CameraStatus {
    pub framesize:      u32,
    pub scale:          u8,
    pub binning:        u8,
    pub quality:        u8,
    pub brightness:     i8,
    pub contrast:       i8,
    pub saturation:     i8,
    pub sharpness:      i8,
    pub denoise:        u8,
    pub special_effect: u8,
    pub wb_mode:        u8,
    pub awb:            u8,
    pub awb_gain:       u8,
    pub aec:            u8,
    pub aec2:           u8,
    pub ae_level:       i8,
    pub aec_value:      u16,
    pub agc:            u8,
    pub agc_gain:       u8,
    pub gainceiling:    u8,
    pub bpc:            u8,
    pub wpc:            u8,
    pub raw_gma:        u8,
    pub lenc:           u8,
    pub hmirror:        u8,
    pub vflip:          u8,
    pub dcw:            u8,
    pub colorbar:       u8,
}

/// sensor_t
#[repr(C)]
pub struct SensorT {
    pub id:           SensorId,
    pub slv_addr:     u8,
    pub _pad1:        u8,
    pub pixformat:    u32,
    pub status:       CameraStatus,
    pub xclk_freq_hz: i32,

    pub init_status:       Option<unsafe extern "C" fn(*mut SensorT) -> i32>,
    pub reset:             Option<unsafe extern "C" fn(*mut SensorT) -> i32>,
    pub set_pixformat:     Option<unsafe extern "C" fn(*mut SensorT, u32) -> i32>,
    pub set_framesize:     Option<unsafe extern "C" fn(*mut SensorT, u32) -> i32>,
    pub set_contrast:      Option<unsafe extern "C" fn(*mut SensorT, i32) -> i32>,
    pub set_brightness:    Option<unsafe extern "C" fn(*mut SensorT, i32) -> i32>,
    pub set_saturation:    Option<unsafe extern "C" fn(*mut SensorT, i32) -> i32>,
    pub set_sharpness:     Option<unsafe extern "C" fn(*mut SensorT, i32) -> i32>,
    pub set_denoise:       Option<unsafe extern "C" fn(*mut SensorT, i32) -> i32>,
    pub set_gainceiling:   Option<unsafe extern "C" fn(*mut SensorT, u32) -> i32>,
    pub set_quality:       Option<unsafe extern "C" fn(*mut SensorT, i32) -> i32>,
    pub set_colorbar:      Option<unsafe extern "C" fn(*mut SensorT, i32) -> i32>,
    pub set_whitebal:      Option<unsafe extern "C" fn(*mut SensorT, i32) -> i32>,
    pub set_gain_ctrl:     Option<unsafe extern "C" fn(*mut SensorT, i32) -> i32>,
    pub set_exposure_ctrl: Option<unsafe extern "C" fn(*mut SensorT, i32) -> i32>,
    pub set_hmirror:       Option<unsafe extern "C" fn(*mut SensorT, i32) -> i32>,
    pub set_vflip:         Option<unsafe extern "C" fn(*mut SensorT, i32) -> i32>,
    pub set_aec2:          Option<unsafe extern "C" fn(*mut SensorT, i32) -> i32>,
    pub set_awb_gain:      Option<unsafe extern "C" fn(*mut SensorT, i32) -> i32>,
    pub set_agc_gain:      Option<unsafe extern "C" fn(*mut SensorT, i32) -> i32>,
    pub set_aec_value:     Option<unsafe extern "C" fn(*mut SensorT, i32) -> i32>,
}

extern "C" {
    pub fn esp_camera_init(config: *const CameraConfig) -> i32;
    pub fn esp_camera_deinit() -> i32;
    pub fn esp_camera_fb_get() -> *mut CameraFb;
    pub fn esp_camera_fb_return(fb: *mut CameraFb);
    pub fn esp_camera_sensor_get() -> *mut SensorT;

    /// Encode raw pixel data as JPEG. Output buffer allocated with malloc; caller must free().
    pub fn fmt2jpg(
        src:     *const u8,
        src_len: usize,
        width:   u16,
        height:  u16,
        format:  u32,
        quality: u8,
        out:     *mut *mut u8,
        out_len: *mut usize,
    ) -> bool;

    fn free(ptr: *mut core::ffi::c_void);
}

// ─────────────────────────────────────────────────────────────────────────────

fn make_config(framesize: u8, quality: u8, pixel_format: u32, fb_count: usize, grab_mode: u32) -> CameraConfig {
    CameraConfig {
        pin_pwdn:      PIN_PWDN,
        pin_reset:     PIN_RESET,
        pin_xclk:      PIN_XCLK,
        pin_sccb_sda:  PIN_SIOD,
        pin_sccb_scl:  PIN_SIOC,
        pin_d7:        PIN_D7,
        pin_d6:        PIN_D6,
        pin_d5:        PIN_D5,
        pin_d4:        PIN_D4,
        pin_d3:        PIN_D3,
        pin_d2:        PIN_D2,
        pin_d1:        PIN_D1,
        pin_d0:        PIN_D0,
        pin_vsync:     PIN_VSYNC,
        pin_href:      PIN_HREF,
        pin_pclk:      PIN_PCLK,
        xclk_freq_hz:  20_000_000,
        ledc_timer:    ledc_timer_t_LEDC_TIMER_0 as u32,
        ledc_channel:  ledc_channel_t_LEDC_CHANNEL_0 as u32,
        pixel_format,
        frame_size:    framesize as u32,
        jpeg_quality:  quality as i32,
        fb_count,
        fb_location:   CAMERA_FB_IN_PSRAM,
        grab_mode,
        sccb_i2c_port: -1,
    }
}

// Shorthand for the normal JPEG operating mode: 2 rotating buffers, keep newest frame.
// GRAB_LATEST prevents the DMA ring from stalling when nobody is calling fb_get
// (e.g. between captures at idle, or during bright-scene FB-OVF runs).
fn jpeg_config(framesize: u8, quality: u8) -> CameraConfig {
    make_config(framesize, quality, PIXFORMAT_JPEG, 2, CAMERA_GRAB_LATEST)
}

/// Initialise the camera in JPEG mode. Call once after WiFi is up.
pub fn init(framesize: u8, quality: u8) -> anyhow::Result<()> {
    let config = jpeg_config(framesize, quality);
    let ret = unsafe { esp_camera_init(&config as *const _) };
    anyhow::ensure!(ret == ESP_OK as i32, "esp_camera_init failed: 0x{:X}", ret);
    log::info!("Camera init OK (framesize={}, quality={})", framesize, quality);
    Ok(())
}

/// Recover from a stuck camera (FB-OVF spiral, timeout, null frame).
/// Deinits, waits for DMA drain, then reinits in JPEG mode.
pub fn recover(framesize: u8, quality: u8) {
    log::warn!("Camera recover: deinit + reinit");
    unsafe { esp_camera_deinit() };
    hold_pwdn_high();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let config = jpeg_config(framesize, quality);
    let ret = unsafe { esp_camera_init(&config as *const _) };
    if ret == ESP_OK as i32 {
        log::info!("Camera recover OK");
    } else {
        log::error!("Camera recover failed: 0x{:X}", ret);
    }
}

/// Capture a single JPEG frame.
pub fn capture_jpeg() -> anyhow::Result<Vec<u8>> {
    let fb = unsafe { esp_camera_fb_get() };
    anyhow::ensure!(!fb.is_null(), "esp_camera_fb_get returned null");
    let jpeg = unsafe { std::slice::from_raw_parts((*fb).buf, (*fb).len).to_vec() };
    unsafe { esp_camera_fb_return(fb) };
    log::info!("Captured {} bytes JPEG", jpeg.len());
    Ok(jpeg)
}

/// Measure actual frame rate at current sensor settings.
/// Captures N test frames in the current mode and times them.
pub fn measure_fps(samples: u32) -> f32 {
    let start = std::time::Instant::now();
    for _ in 0..samples {
        let fb = unsafe { esp_camera_fb_get() };
        if !fb.is_null() {
            unsafe { esp_camera_fb_return(fb) };
        }
    }
    let elapsed = start.elapsed().as_secs_f32();
    if elapsed > 0.0 { samples as f32 / elapsed } else { 1.0 }
}

/// Capture multiple grayscale frames, average (stack) them, and return a JPEG.
///
/// Switches the camera to PIXFORMAT_GRAYSCALE for capture, accumulates pixel
/// sums in a PSRAM buffer, normalises, encodes with fmt2jpg, then restores JPEG mode.
///
/// `n_frames`  — number of frames to stack (calculated from target_secs × fps)
/// `framesize` — same framesize used in JPEG mode
/// `quality`   — JPEG encode quality for the output
/// `exposure`  — AEC setting re-applied after reinit (same as the caller used)
/// `gain`      — AGC setting re-applied after reinit
pub fn capture_stacked_jpeg(
    n_frames:  u32,
    framesize: u8,
    quality:   u8,
    exposure:  i32,
    gain:      i32,
) -> anyhow::Result<Vec<u8>> {
    // ── switch to grayscale ──────────────────────────────────────────────────
    unsafe { esp_camera_deinit() };
    hold_pwdn_high();  // hold PWDN HIGH so sensor actually power-cycles
    std::thread::sleep(std::time::Duration::from_millis(1000));
    let config = make_config(framesize, quality, PIXFORMAT_GRAYSCALE, 1, CAMERA_GRAB_WHEN_EMPTY);
    let ret = unsafe { esp_camera_init(&config as *const _) };
    if ret != ESP_OK as i32 {
        unsafe { esp_camera_deinit() };
        hold_pwdn_high();
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let _ = unsafe { esp_camera_init(&jpeg_config(framesize, quality) as *const _) };
        anyhow::bail!("grayscale reinit failed: 0x{:X}", ret);
    }

    // Re-apply sensor settings (init resets sensor registers to defaults)
    apply_sensor(exposure, gain);
    // Drain frames until AEC disable + manual exposure takes effect.
    // OV2640 needs ~5 frames minimum; at long exposures the frame rate can
    // be as low as 2-3 fps, so a fixed 300 ms sleep is not enough.
    for _ in 0..8 {
        let fb = unsafe { esp_camera_fb_get() };
        if !fb.is_null() { unsafe { esp_camera_fb_return(fb) }; }
    }

    // ── grab one frame to learn dimensions ───────────────────────────────────
    let fb0 = unsafe { esp_camera_fb_get() };
    anyhow::ensure!(!fb0.is_null(), "first grayscale frame null");
    let (width, height, px_len) = unsafe {
        ((*fb0).width, (*fb0).height, (*fb0).len)
    };
    unsafe { esp_camera_fb_return(fb0) };

    log::info!("Stack: {} frames  {}×{}  ({} px/frame)",
               n_frames, width, height, px_len);

    // ── allocate accumulator in PSRAM (u32 per pixel) ────────────────────────
    let acc_bytes = px_len * core::mem::size_of::<u32>();
    let acc_ptr = unsafe {
        esp_idf_sys::heap_caps_malloc(acc_bytes, MALLOC_CAP_SPIRAM) as *mut u32
    };
    anyhow::ensure!(!acc_ptr.is_null(), "PSRAM alloc failed ({} B)", acc_bytes);

    let acc = unsafe { core::slice::from_raw_parts_mut(acc_ptr, px_len) };
    acc.iter_mut().for_each(|x| *x = 0);

    // ── capture and accumulate ────────────────────────────────────────────────
    for i in 0..n_frames {
        let fb = unsafe { esp_camera_fb_get() };
        if fb.is_null() {
            unsafe { esp_idf_sys::heap_caps_free(acc_ptr as *mut _) };
            anyhow::bail!("frame {} null during stack", i);
        }
        let pixels = unsafe { core::slice::from_raw_parts((*fb).buf, (*fb).len) };
        let count = pixels.len().min(acc.len());
        for j in 0..count {
            acc[j] = acc[j].saturating_add(pixels[j] as u32);
        }
        unsafe { esp_camera_fb_return(fb) };
    }

    // ── normalise to u8 ──────────────────────────────────────────────────────
    let gray: Vec<u8> = acc.iter().map(|&s| (s / n_frames).min(255) as u8).collect();
    unsafe { esp_idf_sys::heap_caps_free(acc_ptr as *mut _) };

    // ── encode to JPEG via fmt2jpg ────────────────────────────────────────────
    let mut out_ptr: *mut u8 = core::ptr::null_mut();
    let mut out_len: usize   = 0;
    let ok = unsafe {
        fmt2jpg(
            gray.as_ptr(), gray.len(),
            width as u16, height as u16,
            PIXFORMAT_GRAYSCALE,
            quality,
            &mut out_ptr, &mut out_len,
        )
    };
    drop(gray);

    let jpeg = if ok && !out_ptr.is_null() && out_len > 0 {
        let data = unsafe { core::slice::from_raw_parts(out_ptr, out_len).to_vec() };
        unsafe { free(out_ptr as *mut _) };
        data
    } else {
        // restore JPEG mode before returning error
        unsafe { esp_camera_deinit() };
        let _ = init(framesize, quality);
        anyhow::bail!("fmt2jpg encoding failed");
    };

    log::info!("Stack result: {} bytes JPEG", jpeg.len());

    // ── restore JPEG mode ─────────────────────────────────────────────────────
    unsafe { esp_camera_deinit() };
    hold_pwdn_high();
    std::thread::sleep(std::time::Duration::from_millis(1000));
    let ret = unsafe { esp_camera_init(&jpeg_config(framesize, quality) as *const _) };
    if ret != ESP_OK as i32 {
        log::error!("JPEG reinit failed: 0x{:X}", ret);
    }

    Ok(jpeg)
}

/// Apply per-capture exposure and gain settings.
pub fn apply_sensor(exposure: i32, gain: i32) {
    let sensor = unsafe { esp_camera_sensor_get() };
    if sensor.is_null() { return; }
    unsafe {
        if exposure >= 0 {
            if let Some(f) = (*sensor).set_exposure_ctrl { f(sensor, 0); }
            if let Some(f) = (*sensor).set_aec_value     { f(sensor, exposure); }
        } else {
            if let Some(f) = (*sensor).set_exposure_ctrl { f(sensor, 1); }
        }
        if gain >= 0 {
            if let Some(f) = (*sensor).set_gain_ctrl { f(sensor, 0); }
            if let Some(f) = (*sensor).set_agc_gain  { f(sensor, gain); }
        } else {
            if let Some(f) = (*sensor).set_gain_ctrl { f(sensor, 1); }
        }
    }
}

/// Set gain ceiling (max auto gain). 0=2×, 1=4×, 2=8×, 3=16×, 4=32×, 5=64×, 6=128×.
pub fn set_gain_ceiling(ceiling: u8) {
    let sensor = unsafe { esp_camera_sensor_get() };
    if sensor.is_null() { return; }
    unsafe {
        if let Some(f) = (*sensor).set_gainceiling { f(sensor, ceiling as u32); }
    }
}
