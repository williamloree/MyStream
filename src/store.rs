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

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Quality {
    /// 720p 30 fps — minimal GPU and bandwidth.
    Eco,
    /// 1080p 30 fps.
    Standard,
    /// 1080p 60 fps — for fast games.
    Smooth,
}

impl Quality {
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
    #[serde(default)]
    pub voice: VoiceSettings,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VoiceSettings {
    pub push_to_talk: bool,
    /// Windows virtual-key code.
    pub ptt_key: u32,
    /// 0 (very sensitive) to 100.
    pub threshold: u32,
    pub denoise: bool,
    pub volume: u32,
    /// Per-person volume in percent.
    #[serde(default)]
    pub gains: std::collections::HashMap<String, u32>,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self { push_to_talk: false, ptt_key: 0x56, threshold: 40, denoise: true, volume: 100, gains: Default::default() }
    }
}

impl Default for Settings {
    fn default() -> Self {
        let pictures = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Pictures")
            .join("MyStream");
        Self { quality: Quality::Standard, audio: true, volume: 100, screenshots_dir: pictures, voice: VoiceSettings::default() }
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
    /// Welcome screen already shown.
    #[serde(default)]
    pub onboarded: bool,
    /// Servers we are a member of: only their name, members and channel
    /// names. Each member keeps their own copy, nothing lives anywhere else.
    #[serde(default)]
    pub servers: Vec<Server>,
    #[serde(default)]
    pub server_invites: Vec<ServerInvite>,
}

impl Data {
    pub fn friend(&self, id: &str) -> Option<&Friend> {
        self.friends.iter().find(|f| f.id == id)
    }
    pub fn friend_mut(&mut self, id: &str) -> Option<&mut Friend> {
        self.friends.iter_mut().find(|f| f.id == id)
    }
    pub fn display_name(&self, id: &str) -> String {
        if let Some(f) = self.friend(id) {
            return f.name.clone();
        }
        self.servers
            .iter()
            .flat_map(|s| s.members.iter())
            .find(|m| m.id == id)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| id.chars().take(8).collect())
    }
    pub fn server(&self, id: &str) -> Option<&Server> {
        self.servers.iter().find(|s| s.id == id)
    }
    pub fn server_mut(&mut self, id: &str) -> Option<&mut Server> {
        self.servers.iter_mut().find(|s| s.id == id)
    }
    /// True when `id` is a member of at least one of our servers.
    pub fn shares_server(&self, id: &str) -> bool {
        self.servers.iter().any(|s| s.is_member(id))
    }
    /// Everyone we keep track of: friends and members of our servers.
    pub fn known_peers(&self, me: &str) -> Vec<String> {
        let blocked = |id: &str| self.friend(id).is_some_and(|f| f.permission == Permission::Blocked);
        let mut v: Vec<String> = self.friends.iter().map(|f| f.id.clone()).filter(|id| !blocked(id)).collect();
        for s in &self.servers {
            for m in s.active_members() {
                if m.id != me && !blocked(&m.id) && !v.contains(&m.id) {
                    v.push(m.id.clone());
                }
            }
        }
        v
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

// ---------------------------------------------------------------- servers

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    pub id: u64,
    pub name: String,
}

/// Settings only the creator can change. Signed with the creator's key so a
/// copy relayed by another member cannot be tampered with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerAdmin {
    pub name: String,
    pub channels: Vec<Channel>,
    pub banned: Vec<String>,
    pub deleted: bool,
    pub version: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    pub name: String,
    /// false once they left.
    pub present: bool,
    /// Last change (ms since epoch): the most recent entry wins.
    pub stamp: u64,
    pub invited_by: String,
}

/// A group of friends with voice channels. There is no server behind it:
/// every member holds a copy and copies are merged when members meet.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Server {
    pub id: String,
    pub owner: String,
    pub admin: ServerAdmin,
    pub sig: iroh::Signature,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerInvite {
    pub server: Server,
    pub from: String,
    pub from_name: String,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn admin_bytes(id: &str, owner: &str, admin: &ServerAdmin) -> Vec<u8> {
    serde_json::to_vec(&(id, owner, admin)).unwrap_or_default()
}

impl Server {
    pub fn create(secret: &SecretKey, my_name: &str, name: &str) -> Self {
        let me = secret.public().to_z32();
        let admin = ServerAdmin {
            name: name.to_string(),
            channels: vec![Channel { id: 1, name: "Général".into() }, Channel { id: 2, name: "Jeux".into() }],
            banned: vec![],
            deleted: false,
            version: 0,
        };
        let id = format!("{:016x}", rand::random::<u64>());
        let sig = secret.sign(&admin_bytes(&id, &me, &admin));
        let member = Member { id: me.clone(), name: my_name.to_string(), present: true, stamp: now_ms(), invited_by: me.clone() };
        let mut s = Self { id, owner: me, admin, sig, members: vec![member] };
        s.sign(secret);
        s
    }

    /// Bumps the admin version and signs it (creator only).
    pub fn sign(&mut self, secret: &SecretKey) {
        self.admin.version += 1;
        self.sig = secret.sign(&admin_bytes(&self.id, &self.owner, &self.admin));
    }

    pub fn admin_valid(&self) -> bool {
        parse_friend_code(&self.owner)
            .is_ok_and(|k| k.verify(&admin_bytes(&self.id, &self.owner, &self.admin), &self.sig).is_ok())
    }

    pub fn is_member(&self, id: &str) -> bool {
        self.members.iter().any(|m| m.id == id && m.present) && !self.admin.banned.iter().any(|b| b == id)
    }

    pub fn active_members(&self) -> impl Iterator<Item = &Member> {
        self.members.iter().filter(|m| m.present && !self.admin.banned.contains(&m.id))
    }

    pub fn member_mut(&mut self, id: &str) -> Option<&mut Member> {
        self.members.iter_mut().find(|m| m.id == id)
    }

    /// Sets (or adds) a member entry with a newer stamp than the current one.
    pub fn set_member(&mut self, id: &str, name: &str, present: bool, invited_by: &str) {
        let stamp = now_ms().max(self.members.iter().find(|m| m.id == id).map_or(0, |m| m.stamp + 1));
        let entry = Member { id: id.into(), name: name.into(), present, stamp, invited_by: invited_by.into() };
        match self.member_mut(id) {
            Some(m) => *m = entry,
            None => self.members.push(entry),
        }
    }

    /// Merges another copy into ours. Returns true when ours changed.
    pub fn merge(&mut self, other: &Server) -> bool {
        if other.id != self.id || other.owner != self.owner {
            return false;
        }
        let mut changed = false;
        if other.admin.version > self.admin.version && other.admin_valid() {
            self.admin = other.admin.clone();
            self.sig = other.sig;
            changed = true;
        }
        for m in &other.members {
            match self.member_mut(&m.id) {
                Some(mine) if m.stamp > mine.stamp => {
                    *mine = m.clone();
                    changed = true;
                }
                Some(_) => {}
                None => {
                    self.members.push(m.clone());
                    changed = true;
                }
            }
        }
        changed
    }

    /// Cheap fingerprint to detect diverging copies (FNV-1a).
    pub fn digest(&self) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        let mut eat = |b: &[u8]| {
            for x in b {
                h ^= *x as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
        };
        eat(&self.admin.version.to_le_bytes());
        let mut ms: Vec<_> = self.members.iter().map(|m| (m.id.as_str(), m.stamp)).collect();
        ms.sort();
        for (id, stamp) in ms {
            eat(id.as_bytes());
            eat(&stamp.to_le_bytes());
        }
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_merge_and_signature() {
        let owner = SecretKey::generate();
        let mut a = Server::create(&owner, "Alice", "Potes");
        assert!(a.admin_valid());
        let mut b = a.clone();
        b.set_member("bob", "Bob", true, &a.owner);
        assert!(a.merge(&b));
        assert!(a.is_member("bob"));
        assert_eq!(a.digest(), b.digest());
        // A member cannot forge admin changes.
        let mut forged = a.clone();
        forged.admin.name = "Pirate".into();
        forged.admin.version += 5;
        assert!(!a.merge(&forged));
        assert_eq!(a.admin.name, "Potes");
        // The owner can.
        let mut real = a.clone();
        real.admin.banned.push("bob".into());
        real.sign(&owner);
        assert!(b.merge(&real));
        assert!(!b.is_member("bob"));
        // Leaving wins over an older copy.
        let mut c = a.clone();
        c.set_member("bob", "Bob", false, &a.owner);
        assert!(a.merge(&c));
        assert!(!a.members.iter().find(|m| m.id == "bob").unwrap().present);
    }
}
