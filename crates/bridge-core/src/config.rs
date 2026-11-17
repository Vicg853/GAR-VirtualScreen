use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

/// Every codec AirBridge knows how to configure. Whether one can actually be
/// used depends on the host encoder, the viewer decoder and the stream path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    H264,
    Hevc,
    Av1,
    Vp9,
}

impl Codec {
    pub const ALL: [Codec; 4] = [Codec::H264, Codec::Hevc, Codec::Av1, Codec::Vp9];

    pub fn label(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
            Self::Av1 => "AV1",
            Self::Vp9 => "VP9",
        }
    }

    /// Heading of the per-codec options accordion.
    pub fn options_title(self) -> &'static str {
        match self {
            Self::H264 => "H.264 options",
            Self::Hevc => "HEVC (H.265) options",
            Self::Av1 => "AV1 options",
            Self::Vp9 => "VP9 options",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
            Self::Vp9 => "vp9",
        }
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecOption {
    pub codec: Codec,
    pub acceleration: Acceleration,
}

impl CodecOption {
    pub const H264_HARDWARE: CodecOption = CodecOption {
        codec: Codec::H264,
        acceleration: Acceleration::Hardware,
    };

    pub fn priority(self) -> u8 {
        match (self.codec, self.acceleration) {
            (Codec::Av1, Acceleration::Hardware) => 8,
            (Codec::Hevc, Acceleration::Hardware) => 7,
            (Codec::H264, Acceleration::Hardware) => 6,
            (Codec::Vp9, Acceleration::Hardware) => 5,
            (Codec::Av1, Acceleration::Software) => 4,
            (Codec::Hevc, Acceleration::Software) => 3,
            (Codec::Vp9, Acceleration::Software) => 2,
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

/// One codec-specific value. The schema decides which variant a key holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OptionValue {
    Bool(bool),
    Int(i64),
    Choice(String),
}

impl OptionValue {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(*value),
            _ => None,
        }
    }
    pub fn as_choice(&self) -> Option<&str> {
        match self {
            Self::Choice(value) => Some(value),
            _ => None,
        }
    }
}

pub type CodecValues = BTreeMap<String, OptionValue>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Values of every codec the user has configured on this screen, keyed by
    /// codec, so switching codec and back restores what was set before.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub codec_options: BTreeMap<String, CodecValues>,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 15_000,
            keyframe_interval: 60,
            h264_profile: H264Profile::High,
            low_latency_rate_control: false,
            vsync: true,
            audio: false,
            preferred_codec: None,
            codec_options: BTreeMap::new(),
        }
    }
}

impl VideoSettings {
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
        if self.codec_options.len() > Codec::ALL.len()
            || self.codec_options.values().any(|values| {
                values.len() > 32
                    || values.iter().any(|(key, value)| {
                        key.len() > 48 || value.as_choice().is_some_and(|c| c.len() > 48)
                    })
            })
        {
            bail!("Codec options exceed their size limit");
        }
        Ok(())
    }

    /// The codec this screen encodes with.
    pub fn codec(&self) -> Codec {
        self.preferred_codec
            .map(|option| option.codec)
            .unwrap_or(Codec::H264)
    }

    pub fn acceleration(&self) -> Acceleration {
        self.preferred_codec
            .map(|option| option.acceleration)
            .unwrap_or(Acceleration::Hardware)
    }

    pub fn portrait(&self) -> bool {
        self.height > self.width
    }

    /// Value of one schema field. H.264 fields the encoder consumes live in
    /// typed members; everything else is kept in the per-codec map.
    pub fn codec_value(&self, codec: Codec, field: &CodecField) -> OptionValue {
        if codec == Codec::H264 {
            match field.key {
                "profile" => {
                    return OptionValue::Choice(
                        match self.h264_profile {
                            H264Profile::Baseline => "baseline",
                            H264Profile::Main => "main",
                            H264Profile::High => "high",
                        }
                        .into(),
                    );
                }
                "keyframe_interval" => return OptionValue::Int(self.keyframe_interval as i64),
                "low_latency" => return OptionValue::Bool(self.low_latency_rate_control),
                _ => {}
            }
        }
        self.codec_options
            .get(codec.key())
            .and_then(|values| values.get(field.key))
            .filter(|value| field.accepts(value))
            .cloned()
            .unwrap_or_else(|| field.default_value())
    }

    pub fn set_codec_value(&mut self, codec: Codec, field: &CodecField, value: OptionValue) {
        if !field.accepts(&value) {
            return;
        }
        if codec == Codec::H264 {
            match (field.key, &value) {
                ("profile", OptionValue::Choice(choice)) => {
                    self.h264_profile = match choice.as_str() {
                        "baseline" => H264Profile::Baseline,
                        "main" => H264Profile::Main,
                        _ => H264Profile::High,
                    };
                    return;
                }
                ("keyframe_interval", OptionValue::Int(frames)) => {
                    self.keyframe_interval = (*frames).clamp(1, 600) as u32;
                    return;
                }
                ("low_latency", OptionValue::Bool(enabled)) => {
                    self.low_latency_rate_control = *enabled;
                    return;
                }
                _ => {}
            }
        }
        self.codec_options
            .entry(codec.key().into())
            .or_default()
            .insert(field.key.into(), value);
    }

    /// Constant-quality target (0–100) when the active H.264 rate control asks for it.
    pub fn h264_quality(&self) -> Option<u8> {
        let values = self.codec_options.get(Codec::H264.key())?;
        (values.get("rate_control")?.as_choice()? == "quality").then(|| {
            values
                .get("quality")
                .and_then(OptionValue::as_int)
                .unwrap_or(70)
                .clamp(1, 100) as u8
        })
    }

    /// Write a preset's bundle of values into the active codec.
    pub fn apply_codec_preset(&mut self, preset: CodecPreset) {
        let codec = self.codec();
        for (key, value) in preset.bundle(codec, self.fps) {
            if let Some(field) = codec_schema(codec).iter().find(|field| field.key == key) {
                self.set_codec_value(codec, field, value);
            }
        }
    }

    /// The preset whose bundle matches the active codec's values, if any.
    pub fn codec_preset(&self) -> CodecPreset {
        let codec = self.codec();
        [CodecPreset::LowLatency, CodecPreset::Quality]
            .into_iter()
            .find(|preset| {
                preset
                    .bundle(codec, self.fps)
                    .into_iter()
                    .all(|(key, value)| {
                        codec_schema(codec)
                            .iter()
                            .find(|field| field.key == key)
                            .is_none_or(|field| self.codec_value(codec, field) == value)
                    })
            })
            .unwrap_or(CodecPreset::Custom)
    }
}

/// Global codec preset. `Custom` is what the selector shows once any codec
/// option diverges from both bundles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecPreset {
    LowLatency,
    Quality,
    Custom,
}

impl CodecPreset {
    pub fn label(self) -> &'static str {
        match self {
            Self::LowLatency => "Low latency",
            Self::Quality => "Quality",
            Self::Custom => "Custom (edited)",
        }
    }

    fn bundle(self, codec: Codec, fps: u32) -> Vec<(&'static str, OptionValue)> {
        let choice = |value: &str| OptionValue::Choice(value.into());
        let low = matches!(self, Self::LowLatency);
        let keyframes = OptionValue::Int(if low { fps } else { fps * 2 }.clamp(1, 600) as i64);
        match (self, codec) {
            (Self::Custom, _) => vec![],
            (_, Codec::H264) => vec![
                ("profile", choice(if low { "main" } else { "high" })),
                ("rate_control", choice("vbr")),
                ("keyframe_interval", keyframes),
                ("low_latency", OptionValue::Bool(low)),
            ],
            (_, Codec::Hevc) => vec![
                ("profile", choice("main")),
                ("rate_control", choice(if low { "cbr" } else { "vbr" })),
                ("keyframe_interval", keyframes),
                ("realtime", OptionValue::Bool(low)),
                ("low_latency", OptionValue::Bool(low)),
            ],
            (_, Codec::Av1) => vec![
                ("speed", OptionValue::Int(if low { 11 } else { 8 })),
                ("rate_control", choice(if low { "cbr" } else { "vbr" })),
                ("tune", choice("screen")),
                ("keyframe_interval", keyframes),
                ("screen_content", OptionValue::Bool(true)),
            ],
            (_, Codec::Vp9) => vec![
                ("deadline", choice(if low { "realtime" } else { "good" })),
                ("cpu_used", OptionValue::Int(if low { 8 } else { 5 })),
                ("rate_control", choice(if low { "cbr" } else { "vbr" })),
                ("keyframe_interval", keyframes),
                ("error_resilient", OptionValue::Bool(low)),
            ],
        }
    }
}

/// One selectable value of a segmented control or dropdown.
#[derive(Clone, Copy, Debug)]
pub struct Choice {
    pub value: &'static str,
    pub label: &'static str,
    /// Why the codec supports this value but this encoder or stream cannot use it.
    pub unavailable: Option<&'static str>,
}

const fn choice(value: &'static str, label: &'static str) -> Choice {
    Choice {
        value,
        label,
        unavailable: None,
    }
}
const fn choice_off(value: &'static str, label: &'static str, reason: &'static str) -> Choice {
    Choice {
        value,
        label,
        unavailable: Some(reason),
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Control {
    Segmented(&'static [Choice]),
    Select(&'static [Choice]),
    Toggle {
        default: bool,
    },
    Number {
        min: i64,
        max: i64,
        default: i64,
        unit: &'static str,
    },
    Range {
        min: i64,
        max: i64,
        default: i64,
    },
}

/// One row of a codec's options accordion.
#[derive(Clone, Copy, Debug)]
pub struct CodecField {
    pub key: &'static str,
    pub label: &'static str,
    pub control: Control,
    /// Set when the codec has the option but this encoder or stream path does not.
    pub unavailable: Option<&'static str>,
    /// Only shown while another field of the same codec holds this choice.
    pub visible_when: Option<(&'static str, &'static str)>,
}

impl CodecField {
    const fn new(key: &'static str, label: &'static str, control: Control) -> Self {
        Self {
            key,
            label,
            control,
            unavailable: None,
            visible_when: None,
        }
    }
    const fn off(mut self, reason: &'static str) -> Self {
        self.unavailable = Some(reason);
        self
    }
    const fn when(mut self, key: &'static str, value: &'static str) -> Self {
        self.visible_when = Some((key, value));
        self
    }

    pub fn default_value(&self) -> OptionValue {
        match self.control {
            Control::Segmented(choices) | Control::Select(choices) => OptionValue::Choice(
                choices
                    .iter()
                    .find(|choice| choice.unavailable.is_none())
                    .or(choices.first())
                    .map(|choice| choice.value)
                    .unwrap_or_default()
                    .into(),
            ),
            Control::Toggle { default } => OptionValue::Bool(default),
            Control::Number { default, .. } | Control::Range { default, .. } => {
                OptionValue::Int(default)
            }
        }
    }

    /// Whether a stored value still fits this field; stale stores fall back to the default.
    pub fn accepts(&self, value: &OptionValue) -> bool {
        match (self.control, value) {
            (Control::Segmented(choices) | Control::Select(choices), OptionValue::Choice(v)) => {
                choices.iter().any(|choice| choice.value == v)
            }
            (Control::Toggle { .. }, OptionValue::Bool(_)) => true,
            (
                Control::Number { min, max, .. } | Control::Range { min, max, .. },
                OptionValue::Int(v),
            ) => (min..=max).contains(v),
            _ => false,
        }
    }

    pub fn visible(&self, settings: &VideoSettings, codec: Codec) -> bool {
        self.visible_when.is_none_or(|(key, expected)| {
            codec_schema(codec)
                .iter()
                .find(|field| field.key == key)
                .is_some_and(|field| {
                    settings.codec_value(codec, field).as_choice() == Some(expected)
                })
        })
    }
}

const NOT_IN_VIDEOTOOLBOX: &str = "Not exposed by the VideoToolbox encoder";
const NO_REORDERING: &str = "The low-latency stream path carries no reordered frames";
const SDR_ONLY: &str = "The stream path is 8-bit 4:2:0 SDR";

const KEYFRAMES: Control = Control::Number {
    min: 1,
    max: 600,
    default: 60,
    unit: "frames",
};

static H264_FIELDS: [CodecField; 7] = [
    CodecField::new(
        "profile",
        "Profile",
        Control::Segmented(&[
            choice("baseline", "Baseline"),
            choice("main", "Main"),
            choice("high", "High"),
        ]),
    ),
    CodecField::new(
        "rate_control",
        "Rate control",
        Control::Select(&[
            choice("vbr", "VBR"),
            choice("quality", "Constant quality"),
            choice_off("cbr", "CBR", NOT_IN_VIDEOTOOLBOX),
        ]),
    ),
    CodecField::new(
        "quality",
        "Quality",
        Control::Range {
            min: 1,
            max: 100,
            default: 70,
        },
    )
    .when("rate_control", "quality"),
    CodecField::new("keyframe_interval", "Keyframe interval", KEYFRAMES),
    CodecField::new(
        "low_latency",
        "Low-latency rate control",
        Control::Toggle { default: false },
    ),
    CodecField::new("b_frames", "B-frames", Control::Toggle { default: false }).off(NO_REORDERING),
    CodecField::new(
        "entropy",
        "Entropy",
        Control::Segmented(&[choice("cabac", "CABAC"), choice("cavlc", "CAVLC")]),
    )
    .off(NOT_IN_VIDEOTOOLBOX),
];

static HEVC_FIELDS: [CodecField; 7] = [
    CodecField::new(
        "profile",
        "Profile",
        Control::Segmented(&[
            choice("main", "Main"),
            choice_off("main10", "Main 10", SDR_ONLY),
            choice_off("444", "4:4:4", SDR_ONLY),
        ]),
    ),
    CodecField::new(
        "rate_control",
        "Rate control",
        Control::Select(&[
            choice("cbr", "CBR"),
            choice("vbr", "VBR"),
            choice("quality", "Constant quality"),
        ]),
    ),
    CodecField::new(
        "quality",
        "Quality",
        Control::Range {
            min: 1,
            max: 100,
            default: 70,
        },
    ),
    CodecField::new(
        "keyframe_interval",
        "Keyframe interval",
        Control::Number {
            min: 1,
            max: 600,
            default: 120,
            unit: "frames",
        },
    ),
    CodecField::new("hdr", "HDR (HLG / PQ)", Control::Toggle { default: false }).off(SDR_ONLY),
    CodecField::new(
        "realtime",
        "Real-time priority",
        Control::Toggle { default: true },
    ),
    CodecField::new(
        "low_latency",
        "Low-latency rate control",
        Control::Toggle { default: true },
    ),
];

const TILES: &[Choice] = &[choice("1", "1"), choice("2", "2"), choice("4", "4")];

static AV1_FIELDS: [CodecField; 8] = [
    CodecField::new(
        "encoder",
        "Encoder",
        Control::Select(&[
            choice("svt", "SVT-AV1 (software)"),
            choice_off(
                "hardware",
                "Hardware (M3+ only)",
                "No hardware AV1 encoder on this Mac",
            ),
        ]),
    ),
    CodecField::new(
        "speed",
        "Speed preset",
        Control::Range {
            min: 0,
            max: 13,
            default: 10,
        },
    ),
    CodecField::new(
        "rate_control",
        "Rate control",
        Control::Select(&[
            choice("cbr", "CBR"),
            choice("vbr", "VBR"),
            choice("crf", "CRF"),
        ]),
    ),
    CodecField::new(
        "tune",
        "Tune",
        Control::Segmented(&[
            choice("screen", "Screen content"),
            choice("psnr", "PSNR"),
            choice("visual", "Visual"),
        ]),
    ),
    CodecField::new("tile_columns", "Tile columns", Control::Select(TILES)),
    CodecField::new("tile_rows", "Tile rows", Control::Select(TILES)),
    CodecField::new(
        "keyframe_interval",
        "Keyframe interval",
        Control::Number {
            min: 1,
            max: 600,
            default: 240,
            unit: "frames",
        },
    ),
    CodecField::new(
        "screen_content",
        "Screen-content tools (palette, IntraBC)",
        Control::Toggle { default: true },
    ),
];

static VP9_FIELDS: [CodecField; 7] = [
    CodecField::new(
        "profile",
        "Profile",
        Control::Segmented(&[
            choice("0", "0 (8-bit 4:2:0)"),
            choice_off("2", "2 (10-bit)", SDR_ONLY),
        ]),
    ),
    CodecField::new(
        "deadline",
        "Deadline",
        Control::Segmented(&[choice("realtime", "Realtime"), choice("good", "Good")]),
    ),
    CodecField::new(
        "cpu_used",
        "CPU used",
        Control::Range {
            min: 0,
            max: 9,
            default: 8,
        },
    ),
    CodecField::new(
        "rate_control",
        "Rate control",
        Control::Select(&[
            choice("cbr", "CBR"),
            choice("vbr", "VBR"),
            choice("cq", "CQ"),
        ]),
    ),
    CodecField::new(
        "row_mt",
        "Row multithreading",
        Control::Toggle { default: true },
    ),
    CodecField::new(
        "error_resilient",
        "Error resilient",
        Control::Toggle { default: true },
    ),
    CodecField::new(
        "keyframe_interval",
        "Keyframe interval",
        Control::Number {
            min: 1,
            max: 600,
            default: 120,
            unit: "frames",
        },
    ),
];

/// The options a codec really exposes. One generic renderer draws any schema;
/// the content is what differs per codec.
pub fn codec_schema(codec: Codec) -> &'static [CodecField] {
    match codec {
        Codec::H264 => &H264_FIELDS,
        Codec::Hevc => &HEVC_FIELDS,
        Codec::Av1 => &AV1_FIELDS,
        Codec::Vp9 => &VP9_FIELDS,
    }
}

/// Whether a codec can be selected for a stream, and if not, which side blocks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecAvailability {
    pub codec: Codec,
    pub unavailable: Option<&'static str>,
}

/// Combine what the host can encode with what the stream path and viewers can
/// carry. `viewer` is `None` while no viewer is attached.
pub fn codec_availability(
    host: &[CodecOption],
    viewer: Option<&[CodecOption]>,
) -> Vec<CodecAvailability> {
    Codec::ALL
        .into_iter()
        .map(|codec| {
            let encodes = host.iter().any(|option| option.codec == codec);
            let unavailable = if !encodes {
                Some(match codec {
                    Codec::H264 => "No H.264 encoder on this host",
                    Codec::Hevc => "The stream path carries H.264 only in this version",
                    Codec::Av1 => "No AV1 encoder on this host",
                    Codec::Vp9 => "No VP9 encoder on this host",
                })
            } else if viewer.is_some_and(|options| !options.iter().any(|o| o.codec == codec)) {
                Some(match codec {
                    Codec::H264 => "Viewer cannot decode H.264",
                    Codec::Hevc => "Viewer cannot decode HEVC",
                    Codec::Av1 => "Viewer cannot decode AV1",
                    Codec::Vp9 => "Viewer cannot decode VP9",
                })
            } else {
                None
            };
            CodecAvailability { codec, unavailable }
        })
        .collect()
}

/// Resolutions offered by the resolution selector; anything else is "Custom…".
pub const COMMON_RESOLUTIONS: [(u32, u32); 8] = [
    (1280, 720),
    (1920, 1080),
    (1920, 1200),
    (2560, 1440),
    (2560, 1600),
    (3024, 1964),
    (3440, 1440),
    (3840, 2160),
];

pub const COMMON_FRAME_RATES: [u32; 4] = [30, 60, 90, 120];

pub fn aspect_label(width: u32, height: u32) -> String {
    fn gcd(a: u32, b: u32) -> u32 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let divisor = gcd(width, height).max(1);
    match (width / divisor, height / divisor) {
        (8, 5) => "16:10".into(),
        (5, 8) => "10:16".into(),
        (43, 18) => "21:9".into(),
        (w, h) if w <= 32 && h <= 32 => format!("{w}:{h}"),
        _ => format!("{:.2}:1", width as f64 / height as f64),
    }
}

pub fn config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("Cannot find a configuration directory")?;
    Ok(base.join("airbridge"))
}

/// This computer's name as other devices should see it.
pub fn device_name() -> String {
    let name = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().trim_end_matches(".local").to_owned())
        .unwrap_or_default();
    if name.is_empty() {
        "AirBridge device".into()
    } else {
        name.chars().take(48).collect()
    }
}

/// Host-wide application settings, independent of any session or screen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSettings {
    pub listen_port: u16,
    pub advertise: bool,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self {
            listen_port: crate::protocol::PORT,
            advertise: true,
        }
    }
}

impl HostSettings {
    /// Missing or unreadable settings fall back to the defaults.
    pub fn load() -> Self {
        config_dir()
            .ok()
            .and_then(|dir| std::fs::read(dir.join("host-settings.json")).ok())
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .filter(|settings| settings.listen_port >= 1024)
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        if self.listen_port < 1024 {
            bail!("Listen port must be between 1024 and 65535");
        }
        let dir = config_dir()?;
        std::fs::create_dir_all(&dir)?;
        let temporary = dir.join("host-settings.json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(temporary, dir.join("host-settings.json"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_are_codec_specific_and_defaults_fit_their_controls() {
        for codec in Codec::ALL {
            let schema = codec_schema(codec);
            let mut keys = std::collections::HashSet::new();
            for field in schema {
                assert!(keys.insert(field.key), "{codec:?} repeats {}", field.key);
                assert!(
                    field.accepts(&field.default_value()),
                    "{codec:?} {}",
                    field.key
                );
            }
        }
        let keys = |codec| {
            codec_schema(codec)
                .iter()
                .map(|field| field.key)
                .collect::<Vec<_>>()
        };
        assert!(keys(Codec::H264).contains(&"entropy"));
        assert!(!keys(Codec::Hevc).contains(&"entropy"));
        assert!(keys(Codec::Av1).contains(&"tile_columns"));
        assert!(keys(Codec::Vp9).contains(&"cpu_used"));
    }

    #[test]
    fn codec_values_are_kept_per_codec_and_reach_the_encoder_fields() {
        let mut settings = VideoSettings::default();
        let field = |codec, key: &str| {
            codec_schema(codec)
                .iter()
                .find(|field| field.key == key)
                .unwrap()
        };
        settings.set_codec_value(
            Codec::H264,
            field(Codec::H264, "profile"),
            OptionValue::Choice("main".into()),
        );
        settings.set_codec_value(
            Codec::H264,
            field(Codec::H264, "keyframe_interval"),
            OptionValue::Int(90),
        );
        assert_eq!(settings.h264_profile, H264Profile::Main);
        assert_eq!(settings.keyframe_interval, 90);
        settings.set_codec_value(
            Codec::Vp9,
            field(Codec::Vp9, "keyframe_interval"),
            OptionValue::Int(300),
        );
        assert_eq!(settings.keyframe_interval, 90, "VP9 values stay with VP9");
        assert_eq!(
            settings.codec_value(Codec::Vp9, field(Codec::Vp9, "keyframe_interval")),
            OptionValue::Int(300)
        );
        // Out-of-range and mistyped values never replace a valid one.
        settings.set_codec_value(
            Codec::Vp9,
            field(Codec::Vp9, "cpu_used"),
            OptionValue::Int(99),
        );
        assert_eq!(
            settings.codec_value(Codec::Vp9, field(Codec::Vp9, "cpu_used")),
            OptionValue::Int(8)
        );
        settings.validate().unwrap();
        let copy: VideoSettings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(copy, settings);
    }

    #[test]
    fn presets_write_a_bundle_and_edits_become_custom() {
        let mut settings = VideoSettings::default();
        settings.apply_codec_preset(CodecPreset::LowLatency);
        assert_eq!(settings.codec_preset(), CodecPreset::LowLatency);
        assert!(settings.low_latency_rate_control);
        assert_eq!(settings.keyframe_interval, settings.fps);
        settings.apply_codec_preset(CodecPreset::Quality);
        assert_eq!(settings.codec_preset(), CodecPreset::Quality);
        assert_eq!(settings.h264_profile, H264Profile::High);
        settings.keyframe_interval = 17;
        assert_eq!(settings.codec_preset(), CodecPreset::Custom);
    }

    #[test]
    fn quality_rows_follow_rate_control_and_availability_names_the_blocker() {
        let mut settings = VideoSettings::default();
        let schema = codec_schema(Codec::H264);
        let quality = schema.iter().find(|f| f.key == "quality").unwrap();
        let rate = schema.iter().find(|f| f.key == "rate_control").unwrap();
        assert!(!quality.visible(&settings, Codec::H264));
        assert_eq!(settings.h264_quality(), None);
        settings.set_codec_value(Codec::H264, rate, OptionValue::Choice("quality".into()));
        assert!(quality.visible(&settings, Codec::H264));
        assert_eq!(settings.h264_quality(), Some(70));

        let host = [CodecOption::H264_HARDWARE];
        let all = codec_availability(&host, None);
        assert!(all[0].unavailable.is_none());
        assert!(all[1..].iter().all(|entry| entry.unavailable.is_some()));
        let blocked = codec_availability(&host, Some(&[]));
        assert_eq!(blocked[0].unavailable, Some("Viewer cannot decode H.264"));
    }

    #[test]
    fn legacy_settings_without_codec_options_still_load() {
        let legacy = r#"{"width":1280,"height":720,"fps":60,"bitrate_kbps":8000,
            "keyframe_interval":60,"vsync":true,"audio":false,
            "preferred_codec":{"codec":"h264","acceleration":"software"}}"#;
        let settings: VideoSettings = serde_json::from_str(legacy).unwrap();
        assert_eq!(settings.acceleration(), Acceleration::Software);
        assert!(settings.codec_options.is_empty());
        assert_eq!(aspect_label(1920, 1080), "16:9");
        assert_eq!(aspect_label(2560, 1600), "16:10");
    }
}
