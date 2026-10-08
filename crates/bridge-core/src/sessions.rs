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
    if !(8..=256).contains(&password.len()) {
        bail!("Password must contain 8–256 bytes");
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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewerScreen {
    pub screen_id: ScreenId,
    pub window: WindowPlacement,
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
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewerRegistration {
    pub viewer_id: String,
    pub preferences: ViewerPreferences,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionContract {
    pub version: u16,
    pub session_id: String,
    pub host_id: String,
    pub username: String,
    pub revision: u64,
    pub keep_awake: bool,
    pub screens: Vec<SavedScreen>,
}
impl SessionContract {
    pub fn validate(&self) -> Result<()> {
        if self.version != STORE_VERSION || !valid_id(&self.session_id) || !valid_id(&self.host_id)
        {
            bail!("Invalid session contract identity/version");
        }
        if self.username.is_empty() || self.username.len() > 64 || self.screens.len() > MAX_SCREENS
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
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostSession {
    pub contract: SessionContract,
    pub salt: [u8; 16],
    pub key: Secret,
    pub viewers: Vec<ViewerRegistration>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostStore {
    pub version: u16,
    #[serde(default = "first_screen_id")]
    pub next_screen_id: u32,
    pub host_id: String,
    pub sessions: Vec<HostSession>,
}
impl HostStore {
    pub fn new() -> Result<Self> {
        Ok(Self {
            version: STORE_VERSION,
            next_screen_id: 1,
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
                if !screens.insert(screen.screen_id) || screens.len() > MAX_SCREENS {
                    bail!("Host screen ownership is duplicated or exceeds the screen limit");
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
        if self.next_screen_id == 0 || screens.iter().any(|id| *id >= self.next_screen_id) {
            bail!("Invalid screen identity allocator");
        }
        Ok(())
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
        if screens.iter().any(|screen| screen.screen_id == u32::MAX) {
            bail!("Screen identity exhausted");
        }
        self.next_screen_id = self
            .next_screen_id
            .max(screens.iter().map(|s| s.screen_id + 1).max().unwrap_or(1));
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
            revision: 1,
            keep_awake: false,
            screens,
        };
        contract.validate()?;
        self.sessions.push(HostSession {
            contract,
            salt,
            key,
            viewers: vec![],
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
    fn contract_bounds() {
        let mut s = HostStore::new().unwrap();
        s.create("bob".into(), "password-123", vec![]).unwrap();
        s.sessions[0].contract.host_id = "spoof".into();
        assert!(s.validate().is_err());
        let mut p = ViewerPreferences::default();
        p.screens.push(ViewerScreen {
            screen_id: 1,
            window: WindowPlacement {
                width: 999999,
                ..Default::default()
            },
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
                    settings: crate::config::built_in_presets()[1].settings.clone(),
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
                screen_id: 7,
                window: WindowPlacement {
                    width: 1280,
                    height: 720,
                    x: Some(-1280),
                    y: Some(50),
                },
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

fn first_screen_id() -> u32 {
    1
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
