//! Peer-to-peer layer on top of iroh: every user is identified by a public
//! key, peers are found through iroh's public discovery / relay
//! infrastructure, and all traffic is end-to-end encrypted QUIC (direct when
//! hole punching works, relayed otherwise). No server of our own.
//!
//! Protocol (ALPN `mystream/1`): each request opens a bidirectional stream
//! carrying length-prefixed JSON messages. While watching, video frames flow
//! host -> viewer on unidirectional streams (one per frame, no head-of-line
//! blocking between frames) and audio as unreliable datagrams.
//!
//! Voice channels: every participant sends its Opus packets straight to every
//! other participant as datagrams (full mesh, no relay of our own, nothing
//! recorded). Servers are only a shared list of members and channel names,
//! kept by each member and merged whenever two members meet.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointId};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, broadcast, mpsc, oneshot};

use crate::media::audio::{AudioCapture, AudioPlayer};
use crate::media::capture::Source;
use crate::media::decoder::{DecodedFrame, Decoder};
use crate::media::pipeline::Pipeline;
use crate::media::voice::{Mic, Mixer, VoiceShared};
use crate::store::{Permission, Server, ServerAdmin, ServerInvite, Store};

pub const ALPN: &[u8] = b"mystream/1";
const MAX_MSG: usize = 1 << 20;
const MAX_SCREENSHOT: u64 = 64 << 20;
const MAX_FRAME: usize = 16 << 20;

// ---------------------------------------------------------------- protocol

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StreamInfo {
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub audio: bool,
}

#[derive(Debug, Serialize, Deserialize)]
enum Request {
    FriendRequest { name: String },
    FriendAccept { name: String },
    Status,
    Notify,
    Watch,
    Screenshot { filename: String, size: u64 },
    ServerInvite { server: Box<Server>, name: String },
    ServerSync { server: Box<Server> },
}

#[derive(Debug, Serialize, Deserialize)]
enum Response {
    Ok,
    Accepted { name: String },
    Pending,
    Denied(String),
    Status {
        name: String,
        stream: Option<StreamInfo>,
        #[serde(default)]
        voice: Option<VoiceState>,
        /// (server id, digest) of the servers we share with the requester.
        #[serde(default)]
        servers: Vec<(String, u64)>,
    },
    WatchAccepted(StreamInfo),
    Server(Box<Server>),
}

/// Which voice channel someone is in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VoiceState {
    pub server: String,
    pub channel: u64,
    pub muted: bool,
    pub deafened: bool,
}

impl VoiceState {
    pub fn is_in(&self, server: &str, channel: u64) -> bool {
        self.server == server && self.channel == channel
    }
}

const DGRAM_STREAM_AUDIO: u8 = 2;
const DGRAM_VOICE: u8 = 3;

/// Viewer -> host, on the watch control stream.
#[derive(Debug, Serialize, Deserialize)]
enum ViewerMsg {
    Keyframe,
}

/// Host -> viewer, on the watch control stream.
#[derive(Debug, Serialize, Deserialize)]
enum HostMsg {
    Ended,
}

async fn write_msg(send: &mut SendStream, msg: &impl Serialize) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    send.write_all(&(body.len() as u32).to_le_bytes()).await?;
    send.write_all(&body).await?;
    Ok(())
}

async fn read_msg<T: for<'a> Deserialize<'a>>(recv: &mut RecvStream) -> Result<T> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MSG {
        bail!("message trop gros");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

/// One encoded video frame as sent on the wire.
#[derive(Clone)]
pub struct VideoPacket {
    pub seq: u32,
    pub timestamp_us: u64,
    pub keyframe: bool,
    pub width: u16,
    pub height: u16,
    pub data: Bytes,
}

const VIDEO_HEADER: usize = 4 + 8 + 1 + 2 + 2;

impl VideoPacket {
    fn header(&self) -> [u8; VIDEO_HEADER] {
        let mut h = [0u8; VIDEO_HEADER];
        h[0..4].copy_from_slice(&self.seq.to_le_bytes());
        h[4..12].copy_from_slice(&self.timestamp_us.to_le_bytes());
        h[12] = self.keyframe as u8;
        h[13..15].copy_from_slice(&self.width.to_le_bytes());
        h[15..17].copy_from_slice(&self.height.to_le_bytes());
        h
    }
    fn parse(mut buf: Vec<u8>) -> Option<Self> {
        if buf.len() < VIDEO_HEADER {
            return None;
        }
        let seq = u32::from_le_bytes(buf[0..4].try_into().ok()?);
        let timestamp_us = u64::from_le_bytes(buf[4..12].try_into().ok()?);
        let keyframe = buf[12] != 0;
        let width = u16::from_le_bytes(buf[13..15].try_into().ok()?);
        let height = u16::from_le_bytes(buf[15..17].try_into().ok()?);
        let data = Bytes::from(buf.split_off(VIDEO_HEADER));
        Some(Self { seq, timestamp_us, keyframe, width, height, data })
    }
}

// ---------------------------------------------------------------- shared state

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Audience {
    #[default]
    AllFriends,
    Group(u64),
    /// Every member of a server.
    Server(String),
}

#[derive(Clone, Debug, Default)]
pub struct PeerState {
    pub online: bool,
    pub stream: Option<StreamInfo>,
    pub voice: Option<VoiceState>,
}

#[derive(Clone, Debug)]
pub struct HostingView {
    pub info: StreamInfo,
    pub encoder: String,
    pub audience: Audience,
    pub viewers: Vec<String>,
    pub started: Instant,
}

#[derive(Clone, Debug)]
pub struct WatchingView {
    pub friend: String,
    pub info: StreamInfo,
}

/// State read by the UI every frame.
#[derive(Default)]
pub struct Live {
    pub peers: HashMap<String, PeerState>,
    pub hosting: Option<HostingView>,
    pub watching: Option<WatchingView>,
    pub connecting_to: Option<String>,
    pub online: bool,
    /// The voice channel we are in.
    pub voice: Option<VoiceState>,
}

pub enum UiEvent {
    Toast(String),
    WatchRequest { req_id: u64, friend: String, name: String },
    Screenshot { from: String, path: PathBuf },
    HostFailed(String),
}

pub struct Stats {
    pub received_frames: AtomicU64,
    pub received_bytes: AtomicU64,
}

struct HostSession {
    pipeline: Pipeline,
    _audio: Option<AudioCapture>,
    video_tx: broadcast::Sender<Arc<VideoPacket>>,
    audio_tx: broadcast::Sender<Bytes>,
    info: StreamInfo,
    audience: Audience,
    kicks: HashMap<String, Arc<Notify>>,
}

struct VoiceSession {
    server: String,
    mixer: Mixer,
    _mic: Option<Mic>,
    sender: tokio::task::JoinHandle<()>,
}

impl Drop for VoiceSession {
    fn drop(&mut self) {
        self.sender.abort();
    }
}

// ---------------------------------------------------------------- node

pub struct Node {
    pub endpoint: Endpoint,
    router: OnceLock<Router>,
    pub store: Store,
    pub live: Mutex<Live>,
    /// Latest decoded frame of the stream being watched.
    pub frame: Mutex<Option<DecodedFrame>>,
    pub stats: Stats,
    events: std::sync::mpsc::Sender<UiEvent>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    conns: tokio::sync::Mutex<HashMap<EndpointId, Connection>>,
    /// Any live connection (incoming or outgoing) per peer, for datagrams.
    links: Mutex<HashMap<String, Connection>>,
    host: Mutex<Option<HostSession>>,
    watch_stop: Mutex<Option<Arc<Notify>>>,
    /// Player for the sound of the stream we watch: (host, player).
    watch_audio: Mutex<Option<(String, Arc<AudioPlayer>)>>,
    pending_watch: Mutex<HashMap<u64, oneshot::Sender<bool>>>,
    next_req: AtomicU64,
    pub volume: Arc<AtomicU32>,
    pub voice_shared: Arc<VoiceShared>,
    voice: Mutex<Option<VoiceSession>>,
}

#[derive(Clone)]
struct Handler(Arc<Node>);

impl std::fmt::Debug for Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MyStreamHandler")
    }
}

impl ProtocolHandler for Handler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let remote = conn.remote_id();
        self.0.add_link(remote, &conn);
        while let Ok((send, recv)) = conn.accept_bi().await {
            let node = self.0.clone();
            let conn = conn.clone();
            tokio::spawn(async move {
                if let Err(e) = node.handle(conn, remote, send, recv).await {
                    tracing::debug!("requête de {}: {e:#}", remote.fmt_short());
                }
            });
        }
        Ok(())
    }
}

impl Node {
    pub async fn start(
        store: Store,
        events: std::sync::mpsc::Sender<UiEvent>,
        repaint: impl Fn() + Send + Sync + 'static,
    ) -> Result<Arc<Self>> {
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(store.secret.clone())
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await?;
        let (volume, voice_settings) = {
            let d = store.lock();
            (d.settings.volume, d.settings.voice.clone())
        };
        let node = Arc::new(Self {
            endpoint: endpoint.clone(),
            router: OnceLock::new(),
            store,
            live: Mutex::new(Live::default()),
            frame: Mutex::new(None),
            stats: Stats { received_frames: AtomicU64::new(0), received_bytes: AtomicU64::new(0) },
            events,
            repaint: Arc::new(repaint),
            conns: tokio::sync::Mutex::new(HashMap::new()),
            links: Mutex::new(HashMap::new()),
            host: Mutex::new(None),
            watch_stop: Mutex::new(None),
            watch_audio: Mutex::new(None),
            pending_watch: Mutex::new(HashMap::new()),
            next_req: AtomicU64::new(1),
            volume: Arc::new(AtomicU32::new(volume)),
            voice_shared: VoiceShared::new(&voice_settings),
            voice: Mutex::new(None),
        });
        let router = Router::builder(endpoint).accept(ALPN, Handler(node.clone())).spawn();
        let _ = node.router.set(router);

        let n = node.clone();
        tokio::spawn(async move {
            n.endpoint.online().await;
            n.live.lock().unwrap().online = true;
            (n.repaint)();
        });
        tokio::spawn(node.clone().presence_loop());
        Ok(node)
    }

    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    fn event(&self, e: UiEvent) {
        let _ = self.events.send(e);
        (self.repaint)();
    }

    pub fn notify_ui(&self, e: UiEvent) {
        self.event(e);
    }

    fn toast(&self, msg: impl Into<String>) {
        self.event(UiEvent::Toast(msg.into()));
    }

    pub fn repaint(&self) {
        (self.repaint)();
    }

    async fn connect(self: &Arc<Self>, id: EndpointId) -> Result<Connection> {
        let mut conns = self.conns.lock().await;
        if let Some(c) = conns.get(&id)
            && c.close_reason().is_none() {
                return Ok(c.clone());
            }
        let conn = tokio::time::timeout(Duration::from_secs(12), self.endpoint.connect(id, ALPN))
            .await
            .context("délai dépassé")??;
        conns.insert(id, conn.clone());
        self.add_link(id, &conn);
        Ok(conn)
    }

    /// Remembers a connection for datagrams and dispatches the ones it carries.
    fn add_link(self: &Arc<Self>, id: EndpointId, conn: &Connection) {
        let z = id.to_z32();
        self.links.lock().unwrap().insert(z.clone(), conn.clone());
        let (n, c) = (self.clone(), conn.clone());
        tokio::spawn(async move {
            while let Ok(d) = c.read_datagram().await {
                n.on_datagram(&z, &d);
            }
            let mut links = n.links.lock().unwrap();
            if links.get(&z).is_some_and(|l| l.stable_id() == c.stable_id()) {
                links.remove(&z);
            }
        });
    }

    fn on_datagram(&self, from: &str, d: &[u8]) {
        if d.len() < 6 {
            return;
        }
        let seq = u32::from_le_bytes([d[1], d[2], d[3], d[4]]);
        match d[0] {
            DGRAM_STREAM_AUDIO => {
                if let Some((host, p)) = &*self.watch_audio.lock().unwrap()
                    && host == from
                {
                    p.volume.store(self.volume.load(Ordering::Relaxed), Ordering::Relaxed);
                    p.push(d[5..].to_vec());
                }
            }
            DGRAM_VOICE => {
                let voice = self.voice.lock().unwrap();
                if let Some(v) = voice.as_ref()
                    && self.store.lock().server(&v.server).is_some_and(|s| s.is_member(from))
                {
                    v.mixer.push(from.to_string(), seq, d[5..].to_vec());
                }
            }
            _ => {}
        }
    }

    /// Simple request/response round trip.
    async fn request(self: &Arc<Self>, id: EndpointId, req: &Request) -> Result<Response> {
        let fut = async {
            let conn = self.connect(id).await?;
            let (mut send, mut recv) = conn.open_bi().await?;
            write_msg(&mut send, req).await?;
            let resp = read_msg(&mut recv).await?;
            let _ = send.finish();
            Ok::<_, anyhow::Error>(resp)
        };
        let res = tokio::time::timeout(Duration::from_secs(15), fut).await.context("délai dépassé").and_then(|r| r);
        if res.is_err() {
            // Stale connection (peer restarted, network change…): reconnect next time.
            self.conns.lock().await.remove(&id);
        }
        res
    }

    // ------------------------------------------------------------ incoming

    async fn handle(self: Arc<Self>, conn: Connection, remote: EndpointId, mut send: SendStream, mut recv: RecvStream) -> Result<()> {
        let req: Request = read_msg(&mut recv).await?;
        let id = remote.to_z32();
        let (is_friend, perm, my_name, outgoing, co_member) = {
            let d = self.store.lock();
            let f = d.friend(&id);
            (f.is_some(), f.map(|f| f.permission).unwrap_or_default(), d.name.clone(), d.outgoing.contains(&id), d.shares_server(&id))
        };
        if perm == Permission::Blocked {
            write_msg(&mut send, &Response::Denied("bloqué".into())).await?;
            return Ok(());
        }
        match req {
            Request::FriendRequest { name } => {
                let name = clean_name(&name);
                if is_friend || outgoing {
                    self.store.update(|d| d.add_friend(&id, &name));
                    write_msg(&mut send, &Response::Accepted { name: my_name }).await?;
                    if !is_friend {
                        self.toast(format!("{name} est maintenant ton ami"));
                    }
                    tokio::spawn(self.clone().refresh_peer(remote));
                } else {
                    let added = self.store.update(|d| {
                        if d.incoming.iter().any(|r| r.id == id) {
                            false
                        } else {
                            d.incoming.push(crate::store::PendingRequest { id: id.clone(), name: name.clone() });
                            true
                        }
                    });
                    write_msg(&mut send, &Response::Pending).await?;
                    if added {
                        self.toast(format!("Demande d'ami de {name}"));
                    }
                }
            }
            Request::FriendAccept { name } => {
                if is_friend || outgoing {
                    let name = clean_name(&name);
                    self.store.update(|d| d.add_friend(&id, &name));
                    write_msg(&mut send, &Response::Accepted { name: my_name }).await?;
                    if !is_friend {
                        self.toast(format!("{name} a accepté ta demande d'ami"));
                    }
                    tokio::spawn(self.clone().refresh_peer(remote));
                } else {
                    write_msg(&mut send, &Response::Denied("aucune demande".into())).await?;
                }
            }
            Request::ServerSync { server } => {
                let merged = self.store.update(|d| {
                    let mine = d.server_mut(&server.id)?;
                    // Members, or someone a member just invited.
                    let allowed = mine.is_member(&id)
                        || server.members.iter().any(|m| m.id == id && m.present && mine.is_member(&m.invited_by));
                    if !allowed {
                        return None;
                    }
                    let changed = mine.merge(&server);
                    Some((mine.clone(), changed))
                });
                match merged {
                    Some((s, changed)) => {
                        write_msg(&mut send, &Response::Server(Box::new(s.clone()))).await?;
                        if changed {
                            self.after_server_change(&s.id);
                        }
                    }
                    None => write_msg(&mut send, &Response::Denied("serveur inconnu".into())).await?,
                }
            }
            Request::ServerInvite { server, name } => {
                if !is_friend || !server.admin_valid() || !server.is_member(&id) {
                    write_msg(&mut send, &Response::Denied("invitation refusée".into())).await?;
                    return Ok(());
                }
                let known = self.store.update(|d| match d.server_mut(&server.id) {
                    Some(mine) => {
                        mine.merge(&server);
                        true
                    }
                    None => {
                        d.server_invites.retain(|i| i.server.id != server.id);
                        d.server_invites.push(ServerInvite { server: *server.clone(), from: id.clone(), from_name: clean_name(&name) });
                        false
                    }
                });
                write_msg(&mut send, &if known { Response::Ok } else { Response::Pending }).await?;
                if !known {
                    self.toast(format!("{} t'invite dans le serveur « {} »", clean_name(&name), server.admin.name));
                }
            }
            _ if !is_friend && !co_member => {
                write_msg(&mut send, &Response::Denied("pas ami".into())).await?;
            }
            Request::Status => {
                let stream = self.visible_stream(&id);
                let (voice, servers) = {
                    let d = self.store.lock();
                    let voice = self.live.lock().unwrap().voice.clone().filter(|v| d.server(&v.server).is_some_and(|s| s.is_member(&id)));
                    let servers = d.servers.iter().filter(|s| s.is_member(&id)).map(|s| (s.id.clone(), s.digest())).collect();
                    (voice, servers)
                };
                write_msg(&mut send, &Response::Status { name: my_name, stream, voice, servers }).await?;
                // They are obviously online: refresh our view of them too.
                let known_online = self.live.lock().unwrap().peers.get(&id).is_some_and(|p| p.online);
                if !known_online {
                    tokio::spawn(self.clone().refresh_peer(remote));
                }
            }
            Request::Notify => {
                write_msg(&mut send, &Response::Ok).await?;
                tokio::spawn(self.clone().refresh_peer(remote));
            }
            Request::Screenshot { filename, size } => {
                if size > MAX_SCREENSHOT {
                    write_msg(&mut send, &Response::Denied("trop gros".into())).await?;
                    return Ok(());
                }
                let mut data = vec![0u8; size as usize];
                recv.read_exact(&mut data).await?;
                let from = self.store.lock().display_name(&id);
                let path = self.save_received(&from, &filename, &data)?;
                write_msg(&mut send, &Response::Ok).await?;
                self.event(UiEvent::Screenshot { from, path });
            }
            Request::Watch => {
                self.handle_watch(conn, remote, perm, send, recv).await?;
            }
        }
        Ok(())
    }

    fn save_received(&self, from: &str, filename: &str, data: &[u8]) -> Result<PathBuf> {
        let dir = self.store.lock().settings.screenshots_dir.join("reçues");
        std::fs::create_dir_all(&dir)?;
        let safe: String = format!("{from}_{filename}")
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
            .collect();
        let safe = if safe.ends_with(".png") { safe } else { format!("{safe}.png") };
        let path = dir.join(safe);
        std::fs::write(&path, data)?;
        Ok(path)
    }

    fn in_audience(&self, id: &str, audience: &Audience) -> bool {
        let d = self.store.lock();
        match audience {
            Audience::AllFriends => d.friend(id).is_some(),
            Audience::Group(g) => d.groups.iter().any(|gr| gr.id == *g && gr.members.iter().any(|m| m == id)),
            Audience::Server(s) => d.server(s).is_some_and(|s| s.is_member(id)),
        }
    }

    fn visible_stream(&self, id: &str) -> Option<StreamInfo> {
        let host = self.host.lock().unwrap();
        let s = host.as_ref()?;
        self.in_audience(id, &s.audience).then(|| s.info.clone())
    }

    async fn handle_watch(
        self: &Arc<Self>,
        conn: Connection,
        remote: EndpointId,
        perm: Permission,
        mut send: SendStream,
        mut recv: RecvStream,
    ) -> Result<()> {
        let id = remote.to_z32();
        if self.visible_stream(&id).is_none() {
            write_msg(&mut send, &Response::Denied("aucun partage en cours pour toi".into())).await?;
            return Ok(());
        }
        // Sharing with a server works like a voice channel: members just join.
        let server_audience = self.host.lock().unwrap().as_ref().is_some_and(|s| matches!(s.audience, Audience::Server(_)));
        if perm == Permission::Ask && !server_audience {
            let (tx, rx) = oneshot::channel();
            let req_id = self.next_req.fetch_add(1, Ordering::Relaxed);
            self.pending_watch.lock().unwrap().insert(req_id, tx);
            let name = self.store.lock().display_name(&id);
            self.event(UiEvent::WatchRequest { req_id, friend: id.clone(), name });
            let ok = matches!(tokio::time::timeout(Duration::from_secs(60), rx).await, Ok(Ok(true)));
            self.pending_watch.lock().unwrap().remove(&req_id);
            if !ok {
                write_msg(&mut send, &Response::Denied("refusé".into())).await?;
                return Ok(());
            }
        }
        // Subscribe now (the stream may have stopped while we were asking).
        let sub = {
            let mut host = self.host.lock().unwrap();
            host.as_mut().map(|s| {
                let kick = Arc::new(Notify::new());
                s.kicks.insert(id.clone(), kick.clone());
                (s.info.clone(), s.video_tx.subscribe(), s.audio_tx.subscribe(), s.pipeline.keyframe_flag(), kick)
            })
        };
        let Some((info, mut video_rx, mut audio_rx, keyframe, kick)) = sub else {
            write_msg(&mut send, &Response::Denied("partage terminé".into())).await?;
            return Ok(());
        };
        write_msg(&mut send, &Response::WatchAccepted(info)).await?;
        self.set_viewer(&id, true);
        keyframe.store(true, Ordering::Relaxed);

        let audio_conn = conn.clone();
        let audio_task = tokio::spawn(async move {
            let mut seq = 0u32;
            loop {
                match audio_rx.recv().await {
                    Ok(pkt) => {
                        let mut d = Vec::with_capacity(pkt.len() + 5);
                        d.push(DGRAM_STREAM_AUDIO);
                        d.extend_from_slice(&seq.to_le_bytes());
                        d.extend_from_slice(&pkt);
                        seq = seq.wrapping_add(1);
                        if audio_conn.send_datagram(Bytes::from(d)).is_err() && audio_conn.close_reason().is_some() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Viewer control messages are read on their own task (read_msg is not
        // cancellation safe).
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<ViewerMsg>(8);
        let ctrl_task = tokio::spawn(async move {
            while let Ok(m) = read_msg::<ViewerMsg>(&mut recv).await {
                if ctrl_tx.send(m).await.is_err() {
                    break;
                }
            }
        });

        let mut wait_key = true;
        let result: Result<()> = async {
            loop {
                tokio::select! {
                    frame = video_rx.recv() => match frame {
                        Ok(f) => {
                            if wait_key && !f.keyframe {
                                continue;
                            }
                            wait_key = false;
                            let mut s = conn.open_uni().await?;
                            s.write_all(&f.header()).await?;
                            s.write_chunk(f.data.clone()).await?;
                            let _ = s.finish();
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // Too slow for this viewer: skip to the next keyframe.
                            wait_key = true;
                            keyframe.store(true, Ordering::Relaxed);
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    msg = ctrl_rx.recv() => match msg {
                        Some(ViewerMsg::Keyframe) => keyframe.store(true, Ordering::Relaxed),
                        None => break, // viewer left
                    },
                    _ = kick.notified() => break,
                }
            }
            Ok(())
        }
        .await;
        let _ = write_msg(&mut send, &HostMsg::Ended).await;
        let _ = send.finish();
        audio_task.abort();
        ctrl_task.abort();
        self.set_viewer(&id, false);
        if let Some(s) = self.host.lock().unwrap().as_mut() {
            s.kicks.remove(&id);
        }
        result
    }

    fn set_viewer(&self, id: &str, present: bool) {
        let name = self.store.lock().display_name(id);
        {
            let mut live = self.live.lock().unwrap();
            if let Some(h) = live.hosting.as_mut() {
                h.viewers.retain(|v| v != id);
                if present {
                    h.viewers.push(id.to_string());
                }
            }
        }
        self.toast(if present { format!("{name} regarde ton écran") } else { format!("{name} a quitté") });
    }

    pub fn answer_watch(&self, req_id: u64, accept: bool) {
        if let Some(tx) = self.pending_watch.lock().unwrap().remove(&req_id) {
            let _ = tx.send(accept);
        }
    }

    // ------------------------------------------------------------ friends

    pub async fn send_friend_request(self: Arc<Self>, id: EndpointId) {
        if id == self.id() {
            self.toast("C'est ton propre code 🙂");
            return;
        }
        let z = id.to_z32();
        self.store.update(|d| {
            if d.friend(&z).is_none() && !d.outgoing.contains(&z) {
                d.outgoing.push(z.clone());
            }
        });
        self.repaint();
        let name = self.store.lock().name.clone();
        match self.request(id, &Request::FriendRequest { name }).await {
            Ok(Response::Accepted { name }) => {
                let name = clean_name(&name);
                self.store.update(|d| d.add_friend(&z, &name));
                self.toast(format!("{name} est maintenant ton ami"));
                tokio::spawn(self.clone().refresh_peer(id));
            }
            Ok(Response::Pending) => self.toast("Demande envoyée, en attente d'acceptation"),
            Ok(Response::Denied(r)) => self.toast(format!("Demande refusée : {r}")),
            Ok(_) => {}
            Err(e) => {
                tracing::info!("demande d'ami: {e:#}");
                self.toast("Ami injoignable pour l'instant — nouvel essai automatique");
            }
        }
        self.repaint();
    }

    pub async fn accept_friend(self: Arc<Self>, id: EndpointId) {
        let z = id.to_z32();
        let name = self.store.lock().incoming.iter().find(|r| r.id == z).map(|r| r.name.clone()).unwrap_or_default();
        self.store.update(|d| d.add_friend(&z, &name));
        self.repaint();
        let my_name = self.store.lock().name.clone();
        // If they are offline they will retry their request later and be
        // accepted automatically, since they are now in our friend list.
        if let Ok(Response::Accepted { name }) = self.request(id, &Request::FriendAccept { name: my_name }).await {
            let name = clean_name(&name);
            self.store.update(|d| d.add_friend(&z, &name));
        }
        self.refresh_peer(id).await;
    }

    pub fn refuse_friend(&self, id: &str) {
        self.store.update(|d| d.incoming.retain(|r| r.id != id));
    }

    // ------------------------------------------------------------ presence

    async fn presence_loop(self: Arc<Self>) {
        loop {
            self.refresh_all().await;
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
    }

    pub async fn refresh_all(self: &Arc<Self>) {
        let me = self.id().to_z32();
        let (friends, outgoing) = {
            let d = self.store.lock();
            (d.known_peers(&me), d.outgoing.clone())
        };
        for f in friends {
            if let Ok(id) = crate::store::parse_friend_code(&f) {
                tokio::spawn(self.clone().refresh_peer(id));
            }
        }
        for o in outgoing {
            if let Ok(id) = crate::store::parse_friend_code(&o) {
                let n = self.clone();
                tokio::spawn(async move {
                    let name = n.store.lock().name.clone();
                    if let Ok(Response::Accepted { name }) = n.request(id, &Request::FriendRequest { name }).await {
                        let name = clean_name(&name);
                        n.store.update(|d| d.add_friend(&id.to_z32(), &name));
                        n.toast(format!("{name} a accepté ta demande d'ami"));
                        n.refresh_peer(id).await;
                    }
                });
            }
        }
    }

    pub async fn refresh_peer(self: Arc<Self>, id: EndpointId) {
        let z = id.to_z32();
        let state = match self.request(id, &Request::Status).await {
            Ok(Response::Status { name, stream, voice, servers }) => {
                let name = clean_name(&name);
                let stale: Vec<String> = self.store.update(|d| {
                    if let Some(f) = d.friend_mut(&z) {
                        f.name = name.clone();
                    }
                    for s in d.servers.iter_mut() {
                        if let Some(m) = s.member_mut(&z) {
                            m.name = name.clone();
                        }
                    }
                    servers
                        .iter()
                        .filter(|(sid, digest)| d.server(sid).is_some_and(|s| s.digest() != *digest))
                        .map(|(sid, _)| sid.clone())
                        .collect()
                });
                for sid in stale {
                    tokio::spawn(self.clone().sync_with(sid, id));
                }
                PeerState { online: true, stream, voice }
            }
            _ => {
                self.conns.lock().await.remove(&id);
                PeerState::default()
            }
        };
        let changed = {
            let mut live = self.live.lock().unwrap();
            let prev = live.peers.insert(z.clone(), state.clone());
            let was_live = prev.as_ref().is_some_and(|p| p.stream.is_some());
            (!was_live && state.stream.is_some()).then(|| self.store.lock().display_name(&z))
        };
        if let Some(name) = changed {
            self.toast(format!("{name} partage son écran"));
        }
        self.repaint();
    }

    fn notify_audience(self: &Arc<Self>, audience: &Audience) {
        let me = self.id().to_z32();
        let targets: Vec<String> = self.store.lock().known_peers(&me);
        for t in targets {
            if !self.in_audience(&t, audience) {
                continue;
            }
            if let Ok(id) = crate::store::parse_friend_code(&t) {
                let online = self.live.lock().unwrap().peers.get(&t).is_some_and(|p| p.online);
                if online {
                    let n = self.clone();
                    tokio::spawn(async move {
                        let _ = n.request(id, &Request::Notify).await;
                    });
                }
            }
        }
    }

    // ------------------------------------------------------------ hosting

    /// Starts sharing `source` with `audience`. Blocking (a few ms): call it
    /// from outside the async runtime or via spawn_blocking.
    pub fn start_host(self: &Arc<Self>, source: &Source, audience: Audience) -> Result<()> {
        self.stop_host();
        let settings = self.store.lock().settings.clone();
        let video = settings.quality.video();
        let (video_tx, _) = broadcast::channel::<Arc<VideoPacket>>(10);
        let (audio_tx, _) = broadcast::channel::<Bytes>(50);
        let vtx = video_tx.clone();
        let seq = AtomicU32::new(0);
        let dims = Arc::new(Mutex::new((0u16, 0u16)));
        let dims2 = dims.clone();
        let (pipeline, vinfo) = Pipeline::start(source.kind.clone(), video.clone(), move |f| {
            let (w, h) = *dims2.lock().unwrap();
            let _ = vtx.send(Arc::new(VideoPacket {
                seq: seq.fetch_add(1, Ordering::Relaxed),
                timestamp_us: f.timestamp_us,
                keyframe: f.keyframe || crate::media::h264::is_idr(&f.data),
                width: w,
                height: h,
                data: Bytes::from(f.data),
            }));
        })?;
        *dims.lock().unwrap() = (vinfo.width as u16, vinfo.height as u16);
        let audio = if settings.audio {
            let atx = audio_tx.clone();
            match AudioCapture::start(move |p| {
                let _ = atx.send(Bytes::from(p));
            }) {
                Ok(a) => Some(a),
                Err(e) => {
                    self.toast(format!("Audio indisponible : {e}"));
                    None
                }
            }
        } else {
            None
        };
        let info = StreamInfo {
            title: source.name.clone(),
            width: vinfo.width,
            height: vinfo.height,
            fps: video.fps,
            audio: audio.is_some(),
        };
        self.live.lock().unwrap().hosting = Some(HostingView {
            info: info.clone(),
            encoder: vinfo.encoder_name,
            audience: audience.clone(),
            viewers: vec![],
            started: Instant::now(),
        });
        *self.host.lock().unwrap() = Some(HostSession {
            pipeline,
            _audio: audio,
            video_tx,
            audio_tx,
            info,
            audience: audience.clone(),
            kicks: HashMap::new(),
        });
        self.notify_audience(&audience);
        self.repaint();
        Ok(())
    }

    pub fn stop_host(self: &Arc<Self>) {
        let session = self.host.lock().unwrap().take();
        if let Some(s) = session {
            let audience = s.audience.clone();
            for k in s.kicks.values() {
                k.notify_one();
            }
            drop(s); // stops capture/encoder threads
            self.live.lock().unwrap().hosting = None;
            self.notify_audience(&audience);
            self.repaint();
        }
    }

    pub fn kick(&self, id: &str) {
        if let Some(s) = self.host.lock().unwrap().as_ref()
            && let Some(k) = s.kicks.get(id) {
                k.notify_one();
            }
    }

    /// Changes who can see the current stream (kicks people who no longer can).
    pub fn set_audience(self: &Arc<Self>, audience: Audience) {
        let viewers: Vec<String> = {
            let mut host = self.host.lock().unwrap();
            let Some(s) = host.as_mut() else { return };
            s.audience = audience.clone();
            s.kicks.keys().cloned().collect()
        };
        if let Some(h) = self.live.lock().unwrap().hosting.as_mut() {
            h.audience = audience.clone();
        }
        for v in viewers {
            if !self.in_audience(&v, &audience) {
                self.kick(&v);
            }
        }
        self.notify_audience(&audience);
        self.notify_audience(&Audience::AllFriends);
    }

    pub fn host_screenshot(&self) -> Option<Result<(u32, u32, Vec<u8>)>> {
        self.host.lock().unwrap().as_ref().map(|s| s.pipeline.screenshot())
    }

    // ------------------------------------------------------------ screenshots

    pub fn audience_members(&self, audience: &Audience) -> Vec<String> {
        let me = self.id().to_z32();
        let peers = self.store.lock().known_peers(&me);
        peers.into_iter().filter(|id| self.in_audience(id, audience)).collect()
    }

    /// Sends a PNG to every member of `audience`.
    pub async fn send_screenshot(self: Arc<Self>, audience: Audience, png: Arc<Vec<u8>>, filename: String) {
        let members = self.audience_members(&audience);
        let mut tasks = Vec::new();
        for m in members {
            let Ok(id) = crate::store::parse_friend_code(&m) else { continue };
            let (n, png, filename) = (self.clone(), png.clone(), filename.clone());
            tasks.push(tokio::spawn(async move {
                let fut = async {
                    let conn = n.connect(id).await?;
                    let (mut send, mut recv) = conn.open_bi().await?;
                    write_msg(&mut send, &Request::Screenshot { filename, size: png.len() as u64 }).await?;
                    send.write_all(&png).await?;
                    match read_msg::<Response>(&mut recv).await? {
                        Response::Ok => Ok(()),
                        r => bail!("{r:?}"),
                    }
                };
                tokio::time::timeout(Duration::from_secs(60), fut).await.ok().and_then(|r| r.ok()).is_some()
            }));
        }
        let total = tasks.len();
        let mut ok = 0;
        for t in tasks {
            if t.await.unwrap_or(false) {
                ok += 1;
            }
        }
        self.toast(format!("Capture envoyée à {ok}/{total} ami(s)"));
    }

    // ------------------------------------------------------------ watching

    pub fn stop_watching(&self) {
        if let Some(n) = self.watch_stop.lock().unwrap().take() {
            n.notify_one();
        }
    }

    pub async fn watch(self: Arc<Self>, friend: EndpointId) {
        self.stop_watching();
        let z = friend.to_z32();
        self.live.lock().unwrap().connecting_to = Some(z.clone());
        self.repaint();
        let res = self.clone().watch_inner(friend).await;
        {
            let mut live = self.live.lock().unwrap();
            if live.connecting_to.as_deref() == Some(&z) {
                live.connecting_to = None;
            }
            if live.watching.as_ref().is_some_and(|w| w.friend == z) {
                live.watching = None;
            }
        }
        *self.frame.lock().unwrap() = None;
        match res {
            Ok(Some(msg)) | Err(WatchEnd::Msg(msg)) => self.toast(msg),
            _ => {}
        }
        self.repaint();
    }

    async fn watch_inner(self: Arc<Self>, friend: EndpointId) -> Result<Option<String>, WatchEnd> {
        let z = friend.to_z32();
        let name = self.store.lock().display_name(&z);
        let stop = Arc::new(Notify::new());
        *self.watch_stop.lock().unwrap() = Some(stop.clone());

        let conn = self.connect(friend).await.map_err(|e| WatchEnd::Msg(format!("{name} injoignable : {e}")))?;
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| WatchEnd::Msg(e.to_string()))?;
        write_msg(&mut send, &Request::Watch).await.map_err(|e| WatchEnd::Msg(e.to_string()))?;
        // The host may be asking its user: wait up to a minute (or until we cancel).
        let resp = tokio::select! {
            r = tokio::time::timeout(Duration::from_secs(75), read_msg::<Response>(&mut recv)) => r,
            _ = stop.notified() => return Ok(None),
        };
        let info = match resp {
            Ok(Ok(Response::WatchAccepted(info))) => info,
            Ok(Ok(Response::Denied(r))) => return Err(WatchEnd::Msg(format!("{name} : {r}"))),
            Ok(Ok(_)) => return Err(WatchEnd::Msg("réponse inattendue".into())),
            Ok(Err(e)) => return Err(WatchEnd::Msg(format!("connexion perdue : {e}"))),
            Err(_) => return Err(WatchEnd::Msg(format!("{name} n'a pas répondu"))),
        };
        {
            let mut live = self.live.lock().unwrap();
            live.connecting_to = None;
            live.watching = Some(WatchingView { friend: z.clone(), info: info.clone() });
        }
        self.repaint();

        // Decoder thread.
        let (frame_tx, frame_rx) = std::sync::mpsc::channel::<VideoPacket>();
        let (kf_tx, mut kf_rx) = mpsc::unbounded_channel::<()>();
        let node = self.clone();
        let decode_thread = std::thread::Builder::new()
            .name("decoder".into())
            .spawn(move || decode_loop(node, frame_rx, kf_tx))
            .map_err(|e| WatchEnd::Msg(e.to_string()))?;

        // Audio.
        let player = if info.audio { AudioPlayer::start(self.volume.load(Ordering::Relaxed)).ok() } else { None };
        if let Some(p) = &player {
            p.volume.store(self.volume.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        // Datagrams are dispatched by add_link to this player.
        *self.watch_audio.lock().unwrap() = player.map(|p| (z.clone(), Arc::new(p)));

        // Video frames, in order.
        let video_conn = conn.clone();
        let node = self.clone();
        let video_task = tokio::spawn(async move {
            while let Ok(mut s) = video_conn.accept_uni().await {
                let Ok(buf) = s.read_to_end(MAX_FRAME).await else { continue };
                node.stats.received_bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
                if let Some(p) = VideoPacket::parse(buf)
                    && frame_tx.send(p).is_err() {
                        break;
                    }
            }
        });

        let (host_tx, mut host_rx) = mpsc::channel::<()>(1);
        let host_task = tokio::spawn(async move {
            // Any message or end of stream from the host means it is over.
            let _ = read_msg::<HostMsg>(&mut recv).await;
            let _ = host_tx.send(()).await;
        });

        let end = loop {
            tokio::select! {
                _ = stop.notified() => break None,
                _ = host_rx.recv() => break Some(format!("{name} a arrêté le partage")),
                Some(()) = kf_rx.recv() => {
                    if write_msg(&mut send, &ViewerMsg::Keyframe).await.is_err() {
                        break Some("connexion perdue".to_string());
                    }
                }
                reason = conn.closed() => break Some(format!("connexion perdue : {reason}")),
            }
        };
        let _ = send.finish();
        video_task.abort();
        host_task.abort();
        let player = self.watch_audio.lock().unwrap().take();
        // Joining threads blocks: never do it on a runtime worker.
        let _ = tokio::task::spawn_blocking(move || {
            let _ = decode_thread.join();
            drop(player);
        })
        .await;
        Ok(end)
    }
}

enum WatchEnd {
    Msg(String),
}

fn decode_loop(node: Arc<Node>, rx: std::sync::mpsc::Receiver<VideoPacket>, kf_tx: mpsc::UnboundedSender<()>) {
    crate::media::encoder::com_init();
    let mut decoder = match Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            node.toast(format!("Décodeur indisponible : {e}"));
            return;
        }
    };
    let mut last_seq: Option<u32> = None;
    let mut need_key = true;
    let mut last_kf_request = Instant::now() - Duration::from_secs(10);
    while let Ok(pkt) = rx.recv() {
        let gap = last_seq.is_some_and(|l| pkt.seq != l.wrapping_add(1));
        if pkt.keyframe {
            need_key = false;
        } else if gap {
            need_key = true;
        }
        if need_key && !pkt.keyframe {
            if last_kf_request.elapsed() > Duration::from_millis(500) {
                let _ = kf_tx.send(());
                last_kf_request = Instant::now();
            }
            continue;
        }
        last_seq = Some(pkt.seq);
        match decoder.decode(&pkt.data, pkt.timestamp_us, pkt.width as u32, pkt.height as u32) {
            Ok(Some(frame)) => {
                node.stats.received_frames.fetch_add(1, Ordering::Relaxed);
                *node.frame.lock().unwrap() = Some(frame);
                node.repaint();
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!("décodage: {e:#}");
                need_key = true;
            }
        }
    }
}

fn clean_name(name: &str) -> String {
    let n: String = name.chars().filter(|c| !c.is_control()).take(32).collect();
    let n = n.trim().to_string();
    if n.is_empty() { "Ami".into() } else { n }
}


// ---------------------------------------------------------------- servers & voice

impl Node {
    fn me(&self) -> String {
        self.id().to_z32()
    }

    pub fn create_server(&self, name: &str) -> String {
        let my_name = self.store.lock().name.clone();
        let server = Server::create(&self.store.secret, &my_name, name);
        let id = server.id.clone();
        self.store.update(|d| d.servers.push(server));
        self.repaint();
        id
    }

    /// Sends our copy of a server to every other member.
    pub fn sync_server(self: &Arc<Self>, sid: &str) {
        for id in self.server_peers(sid) {
            tokio::spawn(self.clone().sync_with(sid.to_string(), id));
        }
    }

    fn server_peers(&self, sid: &str) -> Vec<EndpointId> {
        let me = self.me();
        self.store
            .lock()
            .server(sid)
            .map(|s| {
                s.active_members()
                    .filter(|m| m.id != me)
                    .filter_map(|m| crate::store::parse_friend_code(&m.id).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Exchanges copies of a server with one member and merges theirs.
    async fn sync_with(self: Arc<Self>, sid: String, peer: EndpointId) {
        let Some(server) = self.store.lock().server(&sid).cloned() else { return };
        if let Ok(Response::Server(theirs)) = self.request(peer, &Request::ServerSync { server: Box::new(server) }).await {
            let changed = self.store.update(|d| d.server_mut(&sid).is_some_and(|s| s.merge(&theirs)));
            if changed {
                self.after_server_change(&sid);
            }
        }
    }

    /// Reacts to a merged change: deleted server, being removed, new members.
    fn after_server_change(self: &Arc<Self>, sid: &str) {
        let me = self.me();
        let Some(s) = self.store.lock().server(sid).cloned() else { return };
        let gone = if s.admin.deleted {
            Some(format!("Le serveur « {} » a été supprimé", s.admin.name))
        } else if !s.is_member(&me) {
            Some(format!("Tu ne fais plus partie de « {} »", s.admin.name))
        } else {
            None
        };
        if let Some(msg) = gone {
            self.store.update(|d| d.servers.retain(|x| x.id != sid));
            if self.live.lock().unwrap().voice.as_ref().is_some_and(|v| v.server == sid) {
                let n = self.clone();
                tokio::task::spawn_blocking(move || n.leave_voice());
            }
            self.toast(msg);
        } else {
            let n = self.clone();
            tokio::spawn(async move { n.refresh_all().await });
        }
        self.repaint();
    }

    /// Creator only: changes name, channels or bans, then spreads it.
    pub fn admin_update(self: &Arc<Self>, sid: &str, f: impl FnOnce(&mut ServerAdmin)) {
        let me = self.me();
        let secret = self.store.secret.clone();
        let ok = self.store.update(|d| match d.server_mut(sid) {
            Some(s) if s.owner == me => {
                f(&mut s.admin);
                s.sign(&secret);
                true
            }
            _ => false,
        });
        if ok {
            self.sync_server(sid);
            self.after_server_change(sid);
        }
    }

    pub async fn invite_to_server(self: Arc<Self>, sid: String, friend: EndpointId) {
        let z = friend.to_z32();
        // Inviting someone the creator removed earlier lifts the ban.
        let banned = self.store.lock().server(&sid).is_some_and(|s| s.admin.banned.contains(&z));
        if banned {
            self.admin_update(&sid, |a| a.banned.retain(|b| *b != z));
        }
        let (server, name, who) = {
            let d = self.store.lock();
            (d.server(&sid).cloned(), d.name.clone(), d.display_name(&z))
        };
        let Some(server) = server else { return };
        match self.request(friend, &Request::ServerInvite { server: Box::new(server), name }).await {
            Ok(Response::Pending | Response::Ok) => self.toast(format!("Invitation envoyée à {who}")),
            Ok(Response::Denied(r)) => self.toast(format!("{who} : {r}")),
            _ => self.toast(format!("{who} est injoignable, réessaie quand il sera en ligne")),
        }
    }

    pub fn accept_invite(self: &Arc<Self>, sid: &str) {
        let (me, my_name) = (self.me(), self.store.lock().name.clone());
        self.store.update(|d| {
            let Some(pos) = d.server_invites.iter().position(|i| i.server.id == sid) else { return };
            let inv = d.server_invites.remove(pos);
            let mut s = inv.server;
            s.set_member(&me, &my_name, true, &inv.from);
            match d.server_mut(sid) {
                Some(mine) => {
                    mine.merge(&s);
                }
                None => d.servers.push(s),
            }
        });
        self.sync_server(sid);
        let n = self.clone();
        tokio::spawn(async move { n.refresh_all().await });
        self.repaint();
    }

    pub fn decline_invite(&self, sid: &str) {
        self.store.update(|d| d.server_invites.retain(|i| i.server.id != sid));
    }

    /// Leaves (or, for the creator, deletes) a server, then forgets it.
    pub async fn leave_server(self: Arc<Self>, sid: String) {
        if self.live.lock().unwrap().voice.as_ref().is_some_and(|v| v.server == sid) {
            let n = self.clone();
            let _ = tokio::task::spawn_blocking(move || n.leave_voice()).await;
        }
        let me = self.me();
        let secret = self.store.secret.clone();
        let name = self.store.lock().name.clone();
        let peers = self.server_peers(&sid);
        self.store.update(|d| {
            if let Some(s) = d.server_mut(&sid) {
                if s.owner == me {
                    s.admin.deleted = true;
                    s.sign(&secret);
                } else {
                    s.set_member(&me, &name, false, &me);
                }
            }
        });
        // Tell whoever is online, then forget everything about it.
        let tasks: Vec<_> = peers.into_iter().map(|id| tokio::spawn(self.clone().sync_with(sid.clone(), id))).collect();
        for t in tasks {
            let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
        }
        self.store.update(|d| d.servers.retain(|s| s.id != sid));
        self.repaint();
    }

    fn notify_server(self: &Arc<Self>, sid: &str) {
        for id in self.server_peers(sid) {
            let online = self.live.lock().unwrap().peers.get(&id.to_z32()).is_some_and(|p| p.online);
            if online {
                let n = self.clone();
                tokio::spawn(async move {
                    let _ = n.request(id, &Request::Notify).await;
                });
            }
        }
    }

    /// Joins a voice channel. Blocking (opens audio devices): call it from
    /// spawn_blocking.
    pub fn join_voice(self: &Arc<Self>, server: &str, channel: u64) {
        self.leave_voice();
        let shared = self.voice_shared.clone();
        let mixer = match Mixer::start(shared.clone(), self.repaint.clone()) {
            Ok(m) => m,
            Err(e) => {
                self.toast(format!("Sortie audio indisponible : {e}"));
                return;
            }
        };
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mic = match Mic::start(shared.clone(), self.repaint.clone(), move |p| {
            let _ = tx.send(p);
        }) {
            Ok(m) => Some(m),
            Err(e) => {
                self.toast(format!("Micro indisponible ({e}) : tu peux écouter, pas parler"));
                None
            }
        };
        let state = VoiceState {
            server: server.to_string(),
            channel,
            muted: shared.muted.load(Ordering::Relaxed) || mic.is_none(),
            deafened: shared.deafened.load(Ordering::Relaxed),
        };
        let sender = tokio::spawn(self.clone().voice_send_loop(rx, server.to_string(), channel));
        *self.voice.lock().unwrap() = Some(VoiceSession { server: server.to_string(), mixer, _mic: mic, sender });
        self.live.lock().unwrap().voice = Some(state);
        self.notify_server(server);
        // Learn right away who is already in the channel.
        for id in self.server_peers(server) {
            tokio::spawn(self.clone().refresh_peer(id));
        }
        self.repaint();
    }

    /// Blocking (joins the audio threads).
    pub fn leave_voice(self: &Arc<Self>) {
        let session = self.voice.lock().unwrap().take();
        if let Some(s) = session {
            let sid = s.server.clone();
            drop(s);
            self.live.lock().unwrap().voice = None;
            self.voice_shared.heard.lock().unwrap().clear();
            self.notify_server(&sid);
            self.repaint();
        }
    }

    pub fn set_voice_flags(self: &Arc<Self>, muted: bool, deafened: bool) {
        self.voice_shared.muted.store(muted, Ordering::Relaxed);
        self.voice_shared.deafened.store(deafened, Ordering::Relaxed);
        let sid = {
            let mut live = self.live.lock().unwrap();
            live.voice.as_mut().map(|v| {
                v.muted = muted;
                v.deafened = deafened;
                v.server.clone()
            })
        };
        if let Some(sid) = sid {
            self.notify_server(&sid);
        }
        self.repaint();
    }

    /// Everyone currently in a voice channel (excluding us).
    pub fn voice_peers(&self, server: &str, channel: u64) -> Vec<String> {
        let live = self.live.lock().unwrap();
        live.peers
            .iter()
            .filter(|(_, p)| p.online && p.voice.as_ref().is_some_and(|v| v.is_in(server, channel)))
            .map(|(id, _)| id.clone())
            .collect()
    }

    async fn voice_send_loop(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Vec<u8>>, server: String, channel: u64) {
        let mut seq = 0u32;
        let mut dialing: HashMap<String, Instant> = HashMap::new();
        while let Some(pkt) = rx.recv().await {
            let mut d = Vec::with_capacity(pkt.len() + 5);
            d.push(DGRAM_VOICE);
            d.extend_from_slice(&seq.to_le_bytes());
            d.extend_from_slice(&pkt);
            seq = seq.wrapping_add(1);
            let d = Bytes::from(d);
            for id in self.voice_peers(&server, channel) {
                let link = self.links.lock().unwrap().get(&id).filter(|c| c.close_reason().is_none()).cloned();
                match link {
                    Some(c) => {
                        let _ = c.send_datagram(d.clone());
                    }
                    None => {
                        if dialing.get(&id).is_none_or(|t| t.elapsed() > Duration::from_secs(5))
                            && let Ok(eid) = crate::store::parse_friend_code(&id)
                        {
                            dialing.insert(id, Instant::now());
                            let n = self.clone();
                            tokio::spawn(async move {
                                let _ = n.connect(eid).await;
                            });
                        }
                    }
                }
            }
        }
    }
}
