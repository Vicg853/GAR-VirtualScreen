//! Persistent session contracts. Host and viewer configuration have separate owners.
use crate::{
    config::{VideoSettings, config_dir},
    protocol::{MAX_SCREENS, ScreenId},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::Path, sync::Mutex};
use zeroize::Zeroize;

pub const STORE_VERSION: u16 = 1;
const MAX_SESSIONS: usize = 32;
static STORAGE: Mutex<()> = Mutex::new(());

#[derive(Clone, Serialize, Deserialize)]
pub struct Secret(pub String);
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}
impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

pub fn new_id() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("System randomness: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
pub fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit())
}
pub fn validate_login(user: &str, password: &str) -> Result<()> {
    if user.is_empty()
        || user.len() > 64
        || !user
            .chars()
            .all(|c| c.is_alphanumeric() || "._-".contains(c))
    {
        bail!("Username must contain 1–64 letters, digits, dots, underscores or hyphens");
    }
    if password.len() > 256 {
        bail!("Password must contain at most 256 bytes");
    }
    Ok(())
}
pub fn password_key(password: &str, salt: &[u8; 16]) -> Result<Secret> {
    if password.len() > 256 {
        bail!("Password too long");
    }
    let mut bytes = [0; 32];
    argon2::Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut bytes)
        .map_err(|e| anyhow::anyhow!("Password derivation failed: {e}"))?;
    let result = Secret(bytes.iter().map(|b| format!("{b:02x}")).collect());
    bytes.zeroize();
    Ok(result)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedScreen {
    #[serde(default)]
    pub position: Option<DisplayPosition>,
    pub screen_id: ScreenId,
    pub name: String,
    pub settings: VideoSettings,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowPlacement {
    pub width: u32,
    pub height: u32,
    pub x: Option<i32>,
    pub y: Option<i32>,
}
/// The monitor a stream is shown on, identified by its desktop origin so the
/// controls and the native video worker agree without sharing handles.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayTarget {
    pub x: i32,
    pub y: i32,
    pub label: String,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowMode {
    Fullscreen,
    #[default]
    Window,
    Borderless,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scaling {
    #[default]
    Fit,
    Fill,
    Stretch,
    Native,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpscaleFilter {
    #[default]
    Bilinear,
    Lanczos,
    Nearest,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecoderChoice {
    #[default]
    Hardware,
    Software,
}
/// How one stream is presented on this viewer. Never changes the stream or the host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewerDisplay {
    pub target: Option<DisplayTarget>,
    pub window_mode: WindowMode,
    pub scaling: Scaling,
    pub filter: UpscaleFilter,
    /// Clockwise quarter turns in degrees: 0, 90, 180 or 270.
    pub rotation: u16,
    pub vsync: bool,
    pub jitter_frames: u8,
    pub decoder: DecoderChoice,
}
impl ViewerDisplay {
    pub const MAX_JITTER_FRAMES: u8 = 8;
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.rotation, 0 | 90 | 180 | 270)
            || self.jitter_frames > Self::MAX_JITTER_FRAMES
            || self.target.as_ref().is_some_and(|t| t.label.len() > 128)
        {
            bail!("Invalid viewer display settings");
        }
        Ok(())
    }
}
fn yes() -> bool {
    true
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewerScreen {
    pub screen_id: ScreenId,
    pub window: WindowPlacement,
    /// False while this viewer has paused the screen; the host then sends it no video.
    #[serde(default = "yes")]
    pub visible: bool,
    #[serde(default)]
    pub display: ViewerDisplay,
}
impl ViewerScreen {
    pub fn new(screen_id: ScreenId) -> Self {
        Self {
            screen_id,
            window: WindowPlacement::default(),
            visible: true,
            display: ViewerDisplay::default(),
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ViewerPreferences {
    pub keep_awake: bool,
    pub screens: Vec<ViewerScreen>,
}
impl ViewerPreferences {
    pub fn validate(&self) -> Result<()> {
        if self.screens.len() > MAX_SCREENS {
            bail!("Too many viewer screens");
        }
        let mut ids = std::collections::HashSet::new();
        for screen in &self.screens {
            if !ids.insert(screen.screen_id) || screen.screen_id == 0 {
                bail!("Duplicate or invalid viewer screen");
            }
            if screen.window.width > 16384
                || screen.window.height > 16384
                || ((screen.window.width == 0) != (screen.window.height == 0))
            {
                bail!("Invalid window size");
            }
            screen.display.validate()?;
        }
        Ok(())
    }
    pub fn screen(&self, screen_id: ScreenId) -> Option<&ViewerScreen> {
        self.screens.iter().find(|s| s.screen_id == screen_id)
    }
    /// The slot's entry, created on first use; `None` once the slot table is full.
    pub fn screen_mut(&mut self, screen_id: ScreenId) -> Option<&mut ViewerScreen> {
        if screen_id == 0 {
            return None;
        }
        if !self.screens.iter().any(|s| s.screen_id == screen_id) {
            if self.screens.len() >= MAX_SCREENS {
                return None;
            }
            self.screens.push(ViewerScreen::new(screen_id));
        }
        self.screens.iter_mut().find(|s| s.screen_id == screen_id)
    }
    pub fn visible(&self, screen_id: ScreenId) -> bool {
        self.screen(screen_id).is_none_or(|s| s.visible)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewerRegistration {
    pub viewer_id: String,
    pub preferences: ViewerPreferences,
}
/// Host policy is independent of the viewer's saved local preference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewerLockPolicy {
    #[default]
    ViewerChoice,
    KeepAwake,
    AllowIdle,
}
impl ViewerLockPolicy {
    pub fn effective_keep_awake(self, viewer_preference: bool) -> bool {
        match self {
            Self::ViewerChoice => viewer_preference,
            Self::KeepAwake => true,
            Self::AllowIdle => false,
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::ViewerChoice => Self::KeepAwake,
            Self::KeepAwake => Self::AllowIdle,
            Self::AllowIdle => Self::ViewerChoice,
        }
    }
    pub const ALL: [Self; 3] = [Self::ViewerChoice, Self::KeepAwake, Self::AllowIdle];
    pub fn label(self) -> &'static str {
        match self {
            Self::ViewerChoice => "Viewer decides",
            Self::KeepAwake => "Keep viewers awake",
            Self::AllowIdle => "Allow idle lock",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionContract {
    pub version: u16,
    pub session_id: String,
    pub host_id: String,
    pub username: String,
    /// Display name; the login stays `username` so saved viewers keep working.
    #[serde(default)]
    pub label: String,
    pub revision: u64,
    pub keep_awake: bool,
    #[serde(default)]
    pub viewer_lock_policy: ViewerLockPolicy,
    pub screens: Vec<SavedScreen>,
}
impl SessionContract {
    pub fn validate(&self) -> Result<()> {
        if self.version != STORE_VERSION || !valid_id(&self.session_id) || !valid_id(&self.host_id)
        {
            bail!("Invalid session contract identity/version");
        }
        if self.username.is_empty()
            || self.username.len() > 64
            || self.label.len() > 64
            || self.screens.len() > MAX_SCREENS
        {
            bail!("Invalid session account or screen count");
        }
        let mut ids = std::collections::HashSet::new();
        for s in &self.screens {
            if s.screen_id == 0 || !ids.insert(s.screen_id) || s.name.len() > 128 {
                bail!("Invalid session screen");
            }
            s.settings.validate()?;
        }
        Ok(())
    }
    /// The name shown in both apps.
    pub fn display_name(&self) -> &str {
        if self.label.is_empty() {
            &self.username
        } else {
            &self.label
        }
    }
    /// Stable short label of a screen within this session: Slot A, Slot B, …
    pub fn slot_label(&self, screen_id: ScreenId) -> Option<String> {
        self.screens
            .iter()
            .position(|s| s.screen_id == screen_id)
            .map(slot_label)
    }
}
pub fn slot_label(index: usize) -> String {
    format!("Slot {}", (b'A' + (index % 26) as u8) as char)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostSession {
    pub contract: SessionContract,
    pub salt: [u8; 16],
    pub key: Secret,
    pub viewers: Vec<ViewerRegistration>,
    /// An active session has its virtual screens created; an inactive one keeps only its config.
    #[serde(default = "yes")]
    pub active: bool,
    /// Host-side switch: when off, viewers are not connected automatically.
    #[serde(default = "yes")]
    pub auto_connect: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostStore {
    pub version: u16,
    pub host_id: String,
    pub sessions: Vec<HostSession>,
}
impl HostStore {
    pub fn new() -> Result<Self> {
        Ok(Self {
            version: STORE_VERSION,
            host_id: new_id()?,
            sessions: vec![],
        })
    }
    pub fn load() -> Result<Self> {
        let path = config_dir()?.join("host-sessions.json");
        if !path.exists() {
            let value = Self::new()?;
            value.save()?;
            return Ok(value);
        }
        let value: Self = read(&path)?;
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        if self.version != STORE_VERSION
            || !valid_id(&self.host_id)
            || self.sessions.len() > MAX_SESSIONS
        {
            bail!("Invalid host session store");
        }
        let mut ids = std::collections::HashSet::new();
        let mut users = std::collections::HashSet::new();
        let mut screens = std::collections::HashSet::new();
        for s in &self.sessions {
            s.contract.validate()?;
            for screen in &s.contract.screens {
                if !screens.insert(screen.screen_id) {
                    bail!("Host screen ownership is duplicated");
                }
            }
            if s.contract.host_id != self.host_id
                || !ids.insert(&s.contract.session_id)
                || !users.insert(&s.contract.username)
                || s.key.0.len() != 64
                || !s.key.0.bytes().all(|b| b.is_ascii_hexdigit())
                || s.viewers.len() > 128
            {
                bail!("Invalid stored session");
            }
            for v in &s.viewers {
                if !valid_id(&v.viewer_id) {
                    bail!("Invalid stored viewer identity");
                }
                v.preferences.validate()?;
            }
        }
        Ok(())
    }
    /// The lowest screen ID no saved or live screen uses. IDs are slots: a
    /// re-created screen takes a released one instead of an ever-growing number.
    pub fn free_screen_id(&self, live: impl IntoIterator<Item = ScreenId>) -> ScreenId {
        let used = self
            .sessions
            .iter()
            .flat_map(|s| s.contract.screens.iter().map(|screen| screen.screen_id))
            .chain(live)
            .collect::<std::collections::HashSet<_>>();
        (1..).find(|id| !used.contains(id)).unwrap_or(1)
    }
    pub fn session(&self, session_id: &str) -> Option<&HostSession> {
        self.sessions
            .iter()
            .find(|s| s.contract.session_id == session_id)
    }
    pub fn session_mut(&mut self, session_id: &str) -> Option<&mut HostSession> {
        self.sessions
            .iter_mut()
            .find(|s| s.contract.session_id == session_id)
    }
    /// The session that remembers a screen, if it is not a temporary one.
    pub fn owner(&self, screen_id: ScreenId) -> Option<&HostSession> {
        self.sessions
            .iter()
            .find(|s| s.contract.screens.iter().any(|x| x.screen_id == screen_id))
    }
    pub fn create(
        &mut self,
        username: String,
        password: &str,
        screens: Vec<SavedScreen>,
    ) -> Result<String> {
        validate_login(&username, password)?;
        if self.sessions.len() >= MAX_SESSIONS
            || self
                .sessions
                .iter()
                .any(|s| s.contract.username == username)
        {
            bail!("Account already exists or session limit reached");
        }
        if screens
            .iter()
            .any(|screen| self.owner(screen.screen_id).is_some())
        {
            bail!("A screen already belongs to another session");
        }
        let session_id = new_id()?;
        let salt_id = new_id()?;
        let mut salt = [0; 16];
        for (i, b) in salt.iter_mut().enumerate() {
            *b = u8::from_str_radix(&salt_id[i * 2..i * 2 + 2], 16)?;
        }
        let key = password_key(password, &salt)?;
        let contract = SessionContract {
            version: STORE_VERSION,
            session_id: session_id.clone(),
            host_id: self.host_id.clone(),
            username,
            label: String::new(),
            revision: 1,
            keep_awake: false,
            viewer_lock_policy: ViewerLockPolicy::default(),
            screens,
        };
        contract.validate()?;
        self.sessions.push(HostSession {
            contract,
            salt,
            key,
            viewers: vec![],
            active: true,
            auto_connect: true,
        });
        Ok(session_id)
    }
    pub fn save(&self) -> Result<()> {
        self.validate()?;
        write(&config_dir()?.join("host-sessions.json"), self)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedViewerSession {
    pub contract: SessionContract,
    pub viewer_id: String,
    pub key: Secret,
    pub preferences: ViewerPreferences,
    pub auto_connect: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewerStore {
    pub version: u16,
    pub viewer_id: String,
    pub sessions: Vec<SavedViewerSession>,
}
impl ViewerStore {
    pub fn load() -> Result<Self> {
        let path = config_dir()?.join("viewer-sessions.json");
        if !path.exists() {
            let s = Self {
                version: STORE_VERSION,
                viewer_id: new_id()?,
                sessions: vec![],
            };
            s.save()?;
            return Ok(s);
        }
        let s: Self = read(&path)?;
        s.validate()?;
        Ok(s)
    }
    fn validate(&self) -> Result<()> {
        if self.version != STORE_VERSION
            || !valid_id(&self.viewer_id)
            || self.sessions.len() > MAX_SESSIONS
        {
            bail!("Invalid viewer store");
        }
        let mut ids = std::collections::HashSet::new();
        for s in &self.sessions {
            s.contract.validate()?;
            s.preferences.validate()?;
            if s.viewer_id != self.viewer_id
                || !ids.insert(&s.contract.session_id)
                || s.key.0.len() != 64
                || !s.key.0.bytes().all(|b| b.is_ascii_hexdigit())
            {
                bail!("Invalid viewer session");
            }
        }
        Ok(())
    }
    pub fn save(&self) -> Result<()> {
        self.validate()?;
        write(&config_dir()?.join("viewer-sessions.json"), self)
    }
    pub fn upsert(&mut self, s: SavedViewerSession) {
        self.sessions
            .retain(|v| v.contract.session_id != s.contract.session_id);
        self.sessions.push(s);
    }
}
fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path)?;
    if bytes.len() > 1024 * 1024 {
        bail!("Session store exceeds size limit");
    }
    serde_json::from_slice(&bytes).context("Invalid session store")
}
fn write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let _guard = STORAGE
        .lock()
        .map_err(|_| anyhow::anyhow!("Session storage unavailable"))?;
    let parent = path.parent().context("Session store parent missing")?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".sessions-{}.tmp", new_id()?));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}
/// Delay is bounded, and discovery identity must match the remembered host.
pub fn reconnect_delay(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_secs((1u64 << attempt.min(5)).min(30))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_credentials_and_owners() {
        let mut s = HostStore::new().unwrap();
        let id = s.create("alice".into(), "long-password", vec![]).unwrap();
        let a = &s.sessions[0];
        assert_eq!(a.contract.session_id, id);
        assert_eq!(a.key.0, password_key("long-password", &a.salt).unwrap().0);
        assert_ne!(a.key.0, password_key("other-password", &a.salt).unwrap().0);
        assert!(format!("{:?}", a).contains("[redacted]"));
        assert!(
            s.create("alice".into(), "another-password", vec![])
                .is_err()
        );
        s.validate().unwrap();
    }
    #[test]
    fn session_password_has_no_minimum() {
        for password in ["", "a", "12", "short"] {
            validate_login("user", password).unwrap();
            let mut store = HostStore::new().unwrap();
            store.create("user".into(), password, vec![]).unwrap();
            assert_eq!(
                store.sessions[0].key.0,
                password_key(password, &store.sessions[0].salt).unwrap().0
            );
        }
        assert!(validate_login("user", &"x".repeat(256)).is_ok());
        assert!(validate_login("user", &"x".repeat(257)).is_err());
    }
    #[test]
    fn viewer_lock_policy_defaults_and_roundtrips() {
        let mut store = HostStore::new().unwrap();
        store.create("user".into(), "", vec![]).unwrap();
        let mut legacy = serde_json::to_value(&store.sessions[0].contract).unwrap();
        legacy.as_object_mut().unwrap().remove("viewer_lock_policy");
        let legacy: SessionContract = serde_json::from_value(legacy).unwrap();
        assert_eq!(legacy.viewer_lock_policy, ViewerLockPolicy::ViewerChoice);
        for policy in [
            ViewerLockPolicy::ViewerChoice,
            ViewerLockPolicy::KeepAwake,
            ViewerLockPolicy::AllowIdle,
        ] {
            store.sessions[0].contract.viewer_lock_policy = policy;
            let copy: HostStore = serde_json::from_str(&snapshot(&store).unwrap()).unwrap();
            assert_eq!(copy.sessions[0].contract.viewer_lock_policy, policy);
            for local in [false, true] {
                assert_eq!(
                    policy.effective_keep_awake(local),
                    match policy {
                        ViewerLockPolicy::ViewerChoice => local,
                        ViewerLockPolicy::KeepAwake => true,
                        ViewerLockPolicy::AllowIdle => false,
                    }
                );
            }
        }
        assert_eq!(
            ViewerLockPolicy::ViewerChoice.next().next().next(),
            ViewerLockPolicy::ViewerChoice
        );
    }
    #[test]
    fn contract_bounds() {
        let mut s = HostStore::new().unwrap();
        s.create("bob".into(), "password-123", vec![]).unwrap();
        s.sessions[0].contract.host_id = "spoof".into();
        assert!(s.validate().is_err());
        let mut p = ViewerPreferences::default();
        p.screens.push(ViewerScreen {
            window: WindowPlacement {
                width: 999999,
                ..Default::default()
            },
            ..ViewerScreen::new(1)
        });
        assert!(p.validate().is_err());
        assert_eq!(reconnect_delay(100).as_secs(), 30);
    }
    #[test]
    fn credentials_not_in_wire_contract_and_placements_roundtrip() {
        let mut store = HostStore::new().unwrap();
        store
            .create(
                "user".into(),
                "long-password",
                vec![SavedScreen {
                    position: None,
                    screen_id: 7,
                    name: "Display".into(),
                    settings: VideoSettings::default(),
                }],
            )
            .unwrap();
        let contract = serde_json::to_string(&store.sessions[0].contract).unwrap();
        assert!(!contract.contains("long-password"));
        assert!(!contract.contains(&store.sessions[0].key.0));
        assert!(!contract.contains("salt"));
        let preferences = ViewerPreferences {
            keep_awake: true,
            screens: vec![ViewerScreen {
                window: WindowPlacement {
                    width: 1280,
                    height: 720,
                    x: Some(-1280),
                    y: Some(50),
                },
                ..ViewerScreen::new(7)
            }],
        };
        let decoded: ViewerPreferences =
            serde_json::from_str(&preferences_snapshot(&preferences).unwrap()).unwrap();
        assert_eq!(decoded.screens[0].window, preferences.screens[0].window);
        decoded.validate().unwrap();
        let duplicate = store.sessions[0].contract.screens[0].clone();
        store.sessions[0].contract.screens.push(duplicate);
        assert!(store.validate().is_err());
    }
    #[test]
    fn screen_ids_are_reused_slots_and_sessions_keep_their_own() {
        let mut store = HostStore::new().unwrap();
        let screen = |screen_id| SavedScreen {
            position: None,
            screen_id,
            name: format!("Screen {screen_id}"),
            settings: VideoSettings::default(),
        };
        assert_eq!(store.free_screen_id([]), 1);
        store
            .create("desk".into(), "", vec![screen(1), screen(2)])
            .unwrap();
        // A temporary screen takes the lowest free slot and releases it on removal.
        assert_eq!(store.free_screen_id([]), 3);
        assert_eq!(store.free_screen_id([3]), 4);
        assert_eq!(store.free_screen_id([]), 3);
        // Several sessions may exist; a screen belongs to at most one.
        assert!(store.create("tv".into(), "", vec![screen(2)]).is_err());
        store.create("tv".into(), "", vec![screen(3)]).unwrap();
        store.validate().unwrap();
        assert_eq!(store.owner(3).unwrap().contract.username, "tv");
        assert_eq!(store.sessions[0].contract.slot_label(2).unwrap(), "Slot B");
        // Stores written before sessions could be paused load as active.
        let mut legacy = serde_json::to_value(&store).unwrap();
        for session in legacy["sessions"].as_array_mut().unwrap() {
            let session = session.as_object_mut().unwrap();
            session.remove("active");
            session.remove("auto_connect");
        }
        legacy["next_screen_id"] = 12.into();
        let legacy: HostStore = serde_json::from_value(legacy).unwrap();
        legacy.validate().unwrap();
        assert!(legacy.sessions.iter().all(|s| s.active && s.auto_connect));
    }
    #[test]
    fn viewer_slots_default_to_visible_and_validate_display_settings() {
        let legacy: ViewerScreen = serde_json::from_str(
            r#"{"screen_id":4,"window":{"width":0,"height":0,"x":null,"y":null}}"#,
        )
        .unwrap();
        assert!(legacy.visible);
        assert_eq!(legacy.display, ViewerDisplay::default());
        let mut preferences = ViewerPreferences::default();
        assert!(preferences.visible(9), "unknown slots are shown");
        preferences.screen_mut(4).unwrap().visible = false;
        assert!(!preferences.visible(4));
        preferences.screen_mut(4).unwrap().display.rotation = 45;
        assert!(preferences.validate().is_err());
        preferences.screen_mut(4).unwrap().display.rotation = 270;
        preferences.validate().unwrap();
        assert!(preferences.screen_mut(0).is_none());
    }
    #[test]
    fn atomic_private_roundtrip() {
        let path =
            std::env::temp_dir().join(format!("airbridge-session-{}.json", new_id().unwrap()));
        let mut s = HostStore::new().unwrap();
        s.create("test".into(), "password-123", vec![]).unwrap();
        write(&path, &s).unwrap();
        let read: HostStore = read(&path).unwrap();
        read.validate().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_file(path).unwrap();
    }
}

pub fn snapshot(store: &HostStore) -> Result<String> {
    Ok(serde_json::to_string(store)?)
}

pub fn contract_snapshot(contract: &SessionContract) -> Result<String> {
    Ok(serde_json::to_string(contract)?)
}

static VIEWER_STORAGE: Mutex<()> = Mutex::new(());
pub fn remember_viewer(saved: SavedViewerSession) -> Result<()> {
    let _guard = VIEWER_STORAGE
        .lock()
        .map_err(|_| anyhow::anyhow!("Viewer storage unavailable"))?;
    let mut store = ViewerStore::load()?;
    store.upsert(saved);
    store.save()
}
pub fn forget_viewer(id: &str) -> Result<()> {
    let _guard = VIEWER_STORAGE
        .lock()
        .map_err(|_| anyhow::anyhow!("Viewer storage unavailable"))?;
    let mut store = ViewerStore::load()?;
    store.sessions.retain(|s| s.contract.session_id != id);
    store.save()
}
pub fn preferences_snapshot(preferences: &ViewerPreferences) -> Result<String> {
    Ok(serde_json::to_string(preferences)?)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayPosition {
    pub x: i32,
    pub y: i32,
}

impl std::ops::Deref for Secret {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}
pub fn clear_secret(value: &mut String) {
    value.zeroize();
}
