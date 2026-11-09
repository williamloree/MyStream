//! Peer-to-peer layer on top of iroh: every user is identified by a public
//! key, peers are found through iroh's public discovery / relay
//! infrastructure, and all traffic is end-to-end encrypted QUIC (direct when
//! hole punching works, relayed otherwise). No server of our own.
//!
//! Protocol (ALPN `mystream/1`): each request opens a bidirectional stream
//! carrying length-prefixed JSON messages. While watching, video frames flow
//! host -> viewer on unidirectional streams (one per frame, no head-of-line
//! blocking between frames) and audio as unreliable datagrams.

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
use crate::store::{Permission, Store};

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
}

#[derive(Debug, Serialize, Deserialize)]
enum Response {
    Ok,
    Accepted { name: String },
    Pending,
    Denied(String),
    Status { name: String, stream: Option<StreamInfo> },
    WatchAccepted(StreamInfo),
}

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
}

#[derive(Clone, Debug, Default)]
pub struct PeerState {
    pub online: bool,
    pub stream: Option<StreamInfo>,
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
    repaint: Box<dyn Fn() + Send + Sync>,
    conns: tokio::sync::Mutex<HashMap<EndpointId, Connection>>,
    host: Mutex<Option<HostSession>>,
    watch_stop: Mutex<Option<Arc<Notify>>>,
    pending_watch: Mutex<HashMap<u64, oneshot::Sender<bool>>>,
    next_req: AtomicU64,
    pub volume: Arc<AtomicU32>,
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
        let volume = store.lock().settings.volume;
        let node = Arc::new(Self {
            endpoint: endpoint.clone(),
            router: OnceLock::new(),
            store,
            live: Mutex::new(Live::default()),
            frame: Mutex::new(None),
            stats: Stats { received_frames: AtomicU64::new(0), received_bytes: AtomicU64::new(0) },
            events,
            repaint: Box::new(repaint),
            conns: tokio::sync::Mutex::new(HashMap::new()),
            host: Mutex::new(None),
            watch_stop: Mutex::new(None),
            pending_watch: Mutex::new(HashMap::new()),
            next_req: AtomicU64::new(1),
            volume: Arc::new(AtomicU32::new(volume)),
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

    async fn connect(&self, id: EndpointId) -> Result<Connection> {
        let mut conns = self.conns.lock().await;
        if let Some(c) = conns.get(&id)
            && c.close_reason().is_none() {
                return Ok(c.clone());
            }
        let conn = tokio::time::timeout(Duration::from_secs(12), self.endpoint.connect(id, ALPN))
            .await
            .context("délai dépassé")??;
        conns.insert(id, conn.clone());
        Ok(conn)
    }

    /// Simple request/response round trip.
    async fn request(&self, id: EndpointId, req: &Request) -> Result<Response> {
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
        let (is_friend, perm, my_name, outgoing) = {
            let d = self.store.lock();
            let f = d.friend(&id);
            (f.is_some(), f.map(|f| f.permission).unwrap_or_default(), d.name.clone(), d.outgoing.contains(&id))
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
            _ if !is_friend => {
                write_msg(&mut send, &Response::Denied("pas ami".into())).await?;
            }
            Request::Status => {
                let stream = self.visible_stream(&id);
                write_msg(&mut send, &Response::Status { name: my_name, stream }).await?;
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
        if perm == Permission::Ask {
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
                        d.push(2u8);
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
        let (friends, outgoing) = {
            let d = self.store.lock();
            (
                d.friends.iter().filter(|f| f.permission != Permission::Blocked).map(|f| f.id.clone()).collect::<Vec<_>>(),
                d.outgoing.clone(),
            )
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
            Ok(Response::Status { name, stream }) => {
                let name = clean_name(&name);
                self.store.update(|d| {
                    if let Some(f) = d.friend_mut(&z) {
                        f.name = name;
                    }
                });
                PeerState { online: true, stream }
            }
            _ => {
                self.conns.lock().await.remove(&id);
                PeerState { online: false, stream: None }
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
        let targets: Vec<String> = {
            let d = self.store.lock();
            d.friends.iter().map(|f| f.id.clone()).collect()
        };
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
        let d = self.store.lock();
        d.friends
            .iter()
            .filter(|f| f.permission != Permission::Blocked)
            .map(|f| f.id.clone())
            .filter(|id| match audience {
                Audience::AllFriends => true,
                Audience::Group(g) => d.groups.iter().any(|gr| gr.id == *g && gr.members.contains(id)),
            })
            .collect()
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
        let player = player.map(Arc::new);
        let audio_conn = conn.clone();
        let player2 = player.clone();
        let volume = self.volume.clone();
        let audio_task = tokio::spawn(async move {
            while let Ok(d) = audio_conn.read_datagram().await {
                if let (Some(p), true) = (&player2, d.len() > 5 && d[0] == 2) {
                    p.volume.store(volume.load(Ordering::Relaxed), Ordering::Relaxed);
                    p.push(d[5..].to_vec());
                }
            }
        });

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
        audio_task.abort();
        video_task.abort();
        host_task.abort();
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

