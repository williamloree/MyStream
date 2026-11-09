//! Everything persisted on disk: identity key, profile, friends, groups,
//! permissions and settings. One JSON file, no server involved.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use iroh::{EndpointId, SecretKey};
use serde::{Deserialize, Serialize};

pub const CODE_PREFIX: &str = "ms-";

/// The friend code is just the public key: send it once, valid forever.
pub fn friend_code(id: &EndpointId) -> String {
    format!("{CODE_PREFIX}{}", id.to_z32())
}

pub fn parse_friend_code(code: &str) -> Result<EndpointId> {
    let code = code.trim();
    let raw = code.strip_prefix(CODE_PREFIX).unwrap_or(code);
    EndpointId::from_z32(raw).map_err(|_| anyhow::anyhow!("code ami invalide"))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Permission {
    /// Ask every time this friend wants to watch.
    #[default]
    Ask,
    /// Let them in without asking.
    Always,
    /// Never: they don't even see that you are streaming.
    Blocked,
}

impl Permission {
    pub fn label(self) -> &'static str {
        match self {
            Permission::Ask => "Demander",
            Permission::Always => "Toujours autoriser",
            Permission::Blocked => "Bloqué",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Friend {
    /// z-base-32 public key.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub permission: Permission,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Group {
    pub id: u64,
    pub name: String,
    pub members: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingRequest {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Quality {
    /// 720p 30 fps — minimal GPU and bandwidth.
    Eco,
    /// 1080p 30 fps.
    Standard,
    /// 1080p 60 fps — for fast games.
    Smooth,
}

impl Quality {
    pub fn label(&self) -> &'static str {
        match self {
            Quality::Eco => "Éco — 720p 30 i/s",
            Quality::Standard => "Standard — 1080p 30 i/s",
            Quality::Smooth => "Fluide — 1080p 60 i/s",
        }
    }
    pub fn video(&self) -> crate::media::pipeline::VideoSettings {
        let (fps, max_height, bitrate) = match self {
            Quality::Eco => (30, 720, 2_500_000),
            Quality::Standard => (30, 1080, 5_000_000),
            Quality::Smooth => (60, 1080, 9_000_000),
        };
        crate::media::pipeline::VideoSettings { fps, max_height, bitrate, always_encode: false }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub quality: Quality,
    pub audio: bool,
    pub volume: u32,
    pub screenshots_dir: PathBuf,
}

impl Default for Settings {
    fn default() -> Self {
        let pictures = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Pictures")
            .join("MyStream");
        Self { quality: Quality::Standard, audio: true, volume: 100, screenshots_dir: pictures }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Data {
    pub name: String,
    pub friends: Vec<Friend>,
    pub groups: Vec<Group>,
    /// People who sent us a friend request.
    pub incoming: Vec<PendingRequest>,
    /// People we sent a request to (retried until they answer).
    pub outgoing: Vec<String>,
    #[serde(default)]
    pub settings: Settings,
}

impl Data {
    pub fn friend(&self, id: &str) -> Option<&Friend> {
        self.friends.iter().find(|f| f.id == id)
    }
    pub fn friend_mut(&mut self, id: &str) -> Option<&mut Friend> {
        self.friends.iter_mut().find(|f| f.id == id)
    }
    pub fn display_name(&self, id: &str) -> String {
        self.friend(id).map(|f| f.name.clone()).unwrap_or_else(|| id.chars().take(8).collect())
    }
    pub fn add_friend(&mut self, id: &str, name: &str) {
        self.incoming.retain(|r| r.id != id);
        self.outgoing.retain(|o| o != id);
        if let Some(f) = self.friend_mut(id) {
            f.name = name.to_string();
        } else {
            self.friends.push(Friend { id: id.to_string(), name: name.to_string(), permission: Permission::Ask });
        }
    }
    pub fn remove_friend(&mut self, id: &str) {
        self.friends.retain(|f| f.id != id);
        for g in &mut self.groups {
            g.members.retain(|m| m != id);
        }
    }
}

#[derive(Clone)]
pub struct Store {
    data: Arc<Mutex<Data>>,
    pub dir: PathBuf,
    pub secret: SecretKey,
}

impl Store {
    /// `profile` lets several instances run on one machine (tests).
    pub fn open(profile: Option<&str>) -> Result<Self> {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("MyStream");
        let dir = match profile {
            Some(p) => base.join("profiles").join(p),
            None => base,
        };
        std::fs::create_dir_all(&dir).with_context(|| format!("création de {}", dir.display()))?;

        let key_path = dir.join("secret.key");
        let secret = match std::fs::read(&key_path) {
            Ok(bytes) if bytes.len() == 32 => SecretKey::from_bytes(bytes.as_slice().try_into().unwrap()),
            _ => {
                let key = SecretKey::generate();
                std::fs::write(&key_path, key.to_bytes())?;
                key
            }
        };

        let data_path = dir.join("data.json");
        let mut data: Data = std::fs::read(&data_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if data.name.is_empty() {
            data.name = std::env::var("USERNAME").unwrap_or_else(|_| "Moi".into());
        }
        let store = Self { data: Arc::new(Mutex::new(data)), dir, secret };
        store.save();
        Ok(store)
    }

    pub fn lock(&self) -> MutexGuard<'_, Data> {
        self.data.lock().unwrap()
    }

    /// Applies a change and writes it to disk.
    pub fn update<R>(&self, f: impl FnOnce(&mut Data) -> R) -> R {
        let r = f(&mut self.lock());
        self.save();
        r
    }

    pub fn save(&self) {
        let json = serde_json::to_vec_pretty(&*self.lock()).unwrap();
        let tmp = self.dir.join("data.json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, self.dir.join("data.json"));
        }
    }
}
