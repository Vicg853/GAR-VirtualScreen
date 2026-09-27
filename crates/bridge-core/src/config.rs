use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    Av1,
    H264,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Acceleration {
    Hardware,
    Software,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum H264Profile {
    Baseline,
    Main,
    #[default]
    High,
}

impl H264Profile {
    pub fn next(self) -> Self {
        match self {
            Self::Baseline => Self::Main,
            Self::Main => Self::High,
            Self::High => Self::Baseline,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecOption {
    pub codec: Codec,
    pub acceleration: Acceleration,
}

impl CodecOption {
    pub fn priority(self) -> u8 {
        match (self.codec, self.acceleration) {
            (Codec::Av1, Acceleration::Hardware) => 4,
            (Codec::H264, Acceleration::Hardware) => 3,
            (Codec::Av1, Acceleration::Software) => 2,
            (Codec::H264, Acceleration::Software) => 1,
        }
    }
}

pub fn negotiate(
    host: &[CodecOption],
    viewer: &[CodecOption],
    preferred: Option<CodecOption>,
) -> Result<CodecOption> {
    let common = host.iter().filter(|option| viewer.contains(option));
    if let Some(choice) = preferred {
        if common.clone().any(|option| *option == choice) {
            return Ok(choice);
        }
        bail!("The selected codec is unavailable on one of the computers");
    }
    common
        .max_by_key(|option| option.priority())
        .copied()
        .context("No mutually supported codec")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub keyframe_interval: u32,
    #[serde(default)]
    pub h264_profile: H264Profile,
    #[serde(default)]
    pub low_latency_rate_control: bool,
    pub vsync: bool,
    pub audio: bool,
    pub preferred_codec: Option<CodecOption>,
}

impl VideoSettings {
    pub fn cycle_codec_preference(&mut self) {
        self.preferred_codec = match self.preferred_codec {
            None => Some(CodecOption {
                codec: Codec::H264,
                acceleration: Acceleration::Hardware,
            }),
            Some(CodecOption {
                codec: Codec::H264,
                acceleration: Acceleration::Hardware,
            }) => Some(CodecOption {
                codec: Codec::H264,
                acceleration: Acceleration::Software,
            }),
            _ => None,
        };
    }

    pub fn validate(&self) -> Result<()> {
        if !(320..=7680).contains(&self.width) || !(240..=4320).contains(&self.height) {
            bail!("Resolution must be between 320×240 and 7680×4320");
        }
        if !(1..=240).contains(&self.fps) {
            bail!("Frame rate must be between 1 and 240 fps");
        }
        if !(250..=100_000).contains(&self.bitrate_kbps) {
            bail!("Bitrate must be between 250 and 100,000 kbps");
        }
        if !(1..=600).contains(&self.keyframe_interval) {
            bail!("Keyframe interval must be between 1 and 600 frames");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub settings: VideoSettings,
}

pub fn built_in_presets() -> Vec<Preset> {
    [
        ("Responsive", 1280, 720, 60, 8_000),
        ("Balanced", 1920, 1080, 60, 15_000),
        ("Detailed", 2560, 1440, 60, 25_000),
    ]
    .into_iter()
    .map(|(name, width, height, fps, bitrate_kbps)| Preset {
        name: name.into(),
        settings: VideoSettings {
            width,
            height,
            fps,
            bitrate_kbps,
            keyframe_interval: fps,
            h264_profile: H264Profile::High,
            low_latency_rate_control: false,
            vsync: true,
            audio: false,
            preferred_codec: None,
        },
    })
    .collect()
}

pub fn config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("Cannot find a configuration directory")?;
    Ok(base.join("airbridge"))
}

pub fn load_custom_presets() -> Result<Vec<Preset>> {
    let path = config_dir()?.join("presets.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let presets: Vec<Preset> = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("Reading {}", path.display()))?,
    )?;
    for preset in &presets {
        preset.settings.validate()?;
    }
    Ok(presets)
}

pub fn save_custom_presets(presets: &[Preset]) -> Result<()> {
    for preset in presets {
        preset.settings.validate()?;
    }
    let dir = config_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join("presets.json");
    let temporary = dir.join("presets.json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(presets)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}
