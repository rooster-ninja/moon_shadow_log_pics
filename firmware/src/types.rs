use serde::{Deserialize, Serialize};

/// Received on `moon-shadow/senddata` — triggers capture with specified camera settings.
/// exposure:   >=0 = manual AEC lines (0-1200), -1 = auto AEC
/// gain:       >=0 = manual AGC gain (0-30),    -1 = auto AGC
/// stack_secs: 0.0 = single frame; >0 = stack for this many seconds (firmware measures fps)
#[derive(Deserialize, Debug)]
pub struct SendDataCmd {
    pub exposure:   i32,
    pub gain:       i32,
    #[serde(default)]
    pub stack_secs: u32,   // whole seconds; f32 triggers Xtensa LLVM constant-pool bug
}

/// Received on `moon-shadow/cmd` — persistent camera/device configuration.
/// Supported cmd values: "SetFramesize", "SetQuality", "SetGainCeiling"
#[derive(Deserialize, Debug)]
pub struct ConfigCmd {
    pub cmd:   String,
    pub value: Option<serde_json::Value>,
}

/// Published on `moon-shadow/status`
#[derive(Serialize, Debug)]
pub struct StatusMessage<'a> {
    pub status:       &'a str,
    pub framesize:    u8,
    pub quality:      u8,
    pub gain_ceiling: u8,
    pub upload_host:  &'a str,
}
