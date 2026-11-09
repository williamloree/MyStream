//! egui interface. The UI only repaints on events (new video frame, network
//! event, user input), so it costs nothing while idle.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use crate::media::capture::{Source, grab_once, list_sources};
use crate::net::{Audience, Node, UiEvent};
use crate::store::{Permission, Quality, friend_code, parse_friend_code};
use crate::update::{self, Release};

const ACCENT: Color32 = Color32::from_rgb(88, 101, 242);
const LIVE: Color32 = Color32::from_rgb(237, 66, 69);
const ONLINE: Color32 = Color32::from_rgb(59, 165, 93);

#[derive(PartialEq)]
enum Tab {
    Share,
    Friends,
}

struct Toast {
    text: String,
    at: Instant,
    path: Option<PathBuf>,
}

enum UpdateState {
    None,
    Available(Release),
    Installing(String),
    Done,
    Failed(String),
}

struct WatchRequest {
    req_id: u64,
    friend: String,
    name: String,
}

pub struct App {
    node: Arc<Node>,
    rt: tokio::runtime::Handle,
    events: std::sync::mpsc::Receiver<UiEvent>,
    tab: Tab,
    sources: Vec<Source>,
    selected: usize,
    audience: Audience,
    shot_audience: Audience,
    add_code: String,
    new_group: String,
    name_edit: String,
    toasts: Vec<Toast>,
    requests: Vec<WatchRequest>,
    texture: Option<egui::TextureHandle>,
    last_frame: Option<(u32, u32, Vec<u8>)>,
    fullscreen: bool,
    starting: bool,
    fps: (Instant, u64, f32),
    /// Dev/test helpers: MYSTREAM_AUTOSTART=1 shares the first screen,
    /// MYSTREAM_AUTOWATCH=1 watches the first friend who shares.
    auto_start: bool,
    auto_watch: bool,
    update: Arc<std::sync::Mutex<UpdateState>>,
}

impl App {
    pub fn new(
        node: Arc<Node>,
        rt: tokio::runtime::Handle,
        events: std::sync::mpsc::Receiver<UiEvent>,
        ctx: egui::Context,
    ) -> Self {
        let name = node.store.lock().name.clone();
        let update = Arc::new(std::sync::Mutex::new(UpdateState::None));
        let u = update.clone();
        rt.spawn(async move {
            match update::check().await {
                Ok(Some(r)) => {
                    *u.lock().unwrap() = UpdateState::Available(r);
                    ctx.request_repaint();
                }
                Ok(None) => {}
                Err(e) => tracing::info!("vérification de mise à jour impossible : {e}"),
            }
        });
        Self {
            node,
            rt,
            events,
            tab: Tab::Share,
            sources: list_sources(),
            selected: 0,
            audience: Audience::AllFriends,
            shot_audience: Audience::AllFriends,
            add_code: String::new(),
            new_group: String::new(),
            name_edit: name,
            toasts: vec![],
            requests: vec![],
            texture: None,
            last_frame: None,
            fullscreen: false,
            starting: false,
            fps: (Instant::now(), 0, 0.0),
            auto_start: std::env::var("MYSTREAM_AUTOSTART").is_ok(),
            auto_watch: std::env::var("MYSTREAM_AUTOWATCH").is_ok(),
            update,
        }
    }

    fn toast(&mut self, text: impl Into<String>) {
        self.toasts.push(Toast { text: text.into(), at: Instant::now(), path: None });
    }

    fn audience_label(&self, a: &Audience) -> String {
        match a {
            Audience::AllFriends => "Tous mes amis".into(),
            Audience::Group(id) => self
                .node
                .store
                .lock()
                .groups
                .iter()
                .find(|g| g.id == *id)
                .map(|g| format!("Groupe « {} »", g.name))
                .unwrap_or_else(|| "Groupe supprimé".into()),
        }
    }

    fn audience_combo(&self, ui: &mut egui::Ui, salt: &str, value: &mut Audience) -> bool {
        let before = value.clone();
        let groups: Vec<(u64, String)> = self.node.store.lock().groups.iter().map(|g| (g.id, g.name.clone())).collect();
        egui::ComboBox::from_id_salt(salt)
            .selected_text(self.audience_label(value))
            .show_ui(ui, |ui| {
                ui.selectable_value(value, Audience::AllFriends, "Tous mes amis");
                for (id, name) in groups {
                    ui.selectable_value(value, Audience::Group(id), format!("Groupe « {name} »"));
                }
            });
        *value != before
    }

    fn handle_events(&mut self) {
        while let Ok(e) = self.events.try_recv() {
            match e {
                UiEvent::Toast(t) => self.toast(t),
                UiEvent::WatchRequest { req_id, friend, name } => self.requests.push(WatchRequest { req_id, friend, name }),
                UiEvent::HostFailed(e) => {
                    self.starting = false;
                    self.toast(e);
                }
                UiEvent::Screenshot { from, path } => self.toasts.push(Toast {
                    text: format!("📸 {from} t'a envoyé une capture"),
                    at: Instant::now(),
                    path: Some(path),
                }),
            }
        }
    }

    // ---------------------------------------------------------------- top bar

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("MyStream").strong().size(18.0).color(ACCENT));
            ui.separator();
            ui.label("Pseudo :");
            let r = ui.add(egui::TextEdit::singleline(&mut self.name_edit).desired_width(120.0));
            if r.lost_focus() {
                let n = self.name_edit.trim().chars().take(32).collect::<String>();
                if !n.is_empty() {
                    self.node.store.update(|d| d.name = n);
                }
            }
            ui.separator();
            let code = friend_code(&self.node.id());
            ui.label("Mon code ami :");
            ui.label(RichText::new(format!("{}…", &code[..14])).monospace());
            if ui.button("📋 Copier").on_hover_text(&code).clicked() {
                ui.ctx().copy_text(code);
                self.toast("Code copié — envoie-le à tes amis (Discord, WhatsApp…)");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let online = self.node.live.lock().unwrap().online;
                let (c, t) = if online { (ONLINE, "En ligne") } else { (Color32::GRAY, "Connexion…") };
                ui.label(RichText::new(t).color(c));
                ui.label(RichText::new("●").color(c));
            });
        });
    }

    // ---------------------------------------------------------------- update banner

    fn update_banner(&mut self, ui: &mut egui::Ui) {
        let mut state = self.update.lock().unwrap();
        match &*state {
            UpdateState::None => {}
            UpdateState::Available(r) => {
                let r = r.clone();
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "⬆ Nouvelle version v{} disponible (tu as v{})",
                            r.version,
                            env!("CARGO_PKG_VERSION")
                        ))
                        .strong(),
                    );
                    if ui.button("Mettre à jour").clicked() {
                        *state = UpdateState::Installing(r.version.clone());
                        let (u, ctx) = (self.update.clone(), ui.ctx().clone());
                        self.rt.spawn(async move {
                            let next = match update::install(&r).await {
                                Ok(()) => UpdateState::Done,
                                Err(e) => UpdateState::Failed(e.to_string()),
                            };
                            *u.lock().unwrap() = next;
                            ctx.request_repaint();
                        });
                    }
                    ui.hyperlink_to("Nouveautés", update::RELEASES_URL);
                    if ui.button("Plus tard").clicked() {
                        *state = UpdateState::None;
                    }
                });
            }
            UpdateState::Installing(v) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("Téléchargement de la v{v}…"));
                });
            }
            UpdateState::Done => {
                // Restart on the new exe.
                *state = UpdateState::None;
                self.node.stop_watching();
                self.node.stop_host();
                match update::relaunch() {
                    Ok(()) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                    Err(e) => *state = UpdateState::Failed(format!("redémarrage impossible : {e}")),
                }
            }
            UpdateState::Failed(e) => {
                let e = e.clone();
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("Mise à jour échouée : {e}")).color(LIVE));
                    ui.hyperlink_to("Télécharger à la main", update::RELEASES_URL);
                    if ui.button("OK").clicked() {
                        *state = UpdateState::None;
                    }
                });
            }
        }
    }

    // ---------------------------------------------------------------- friends panel

    fn friends_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.heading("Amis");
        ui.add_space(4.0);
        let (friends, incoming, outgoing) = {
            let d = self.node.store.lock();
            (d.friends.clone(), d.incoming.clone(), d.outgoing.clone())
        };
        let peers = self.node.live.lock().unwrap().peers.clone();
        let watching = self.node.live.lock().unwrap().watching.as_ref().map(|w| w.friend.clone());

        egui::ScrollArea::vertical().max_height(ui.available_height() - 190.0).show(ui, |ui| {
            if friends.is_empty() {
                ui.label(RichText::new("Pas encore d'amis. Colle le code d'un ami ci-dessous.").weak());
            }
            let mut sorted = friends.clone();
            sorted.sort_by_key(|f| {
                let p = peers.get(&f.id);
                (
                    !p.is_some_and(|p| p.stream.is_some()),
                    !p.is_some_and(|p| p.online),
                    f.name.to_lowercase(),
                )
            });
            for f in sorted {
                let p = peers.get(&f.id).cloned().unwrap_or_default();
                let color = if f.permission == Permission::Blocked {
                    Color32::DARK_GRAY
                } else if p.stream.is_some() {
                    LIVE
                } else if p.online {
                    ONLINE
                } else {
                    Color32::GRAY
                };
                let frame = egui::Frame::new().inner_margin(egui::Margin::symmetric(6, 4)).corner_radius(6.0);
                let resp = frame
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("●").color(color));
                            ui.label(RichText::new(&f.name).strong());
                            if f.permission == Permission::Always {
                                ui.label(RichText::new("auto").small().color(Color32::GOLD)).on_hover_text("Toujours autorisé à regarder");
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if p.stream.is_some() {
                                    let is_watching = watching.as_deref() == Some(f.id.as_str());
                                    if !is_watching
                                        && ui.add(egui::Button::new(RichText::new("▶ Regarder").color(Color32::WHITE)).fill(LIVE)).clicked()
                                        && let Ok(id) = parse_friend_code(&f.id) {
                                            self.rt.spawn(self.node.clone().watch(id));
                                        }
                                }
                            });
                        });
                        if let Some(s) = &p.stream {
                            ui.label(RichText::new(format!("🔴 {} · {}p", s.title, s.height)).small().color(LIVE));
                        } else if !p.online {
                            ui.label(RichText::new("hors ligne").small().weak());
                        }
                    })
                    .response;
                resp.context_menu(|ui| {
                    ui.label(RichText::new(&f.name).strong());
                    ui.separator();
                    ui.label("Quand cet ami veut regarder :");
                    for perm in [Permission::Ask, Permission::Always, Permission::Blocked] {
                        if ui.radio(f.permission == perm, perm.label()).clicked() {
                            self.node.store.update(|d| {
                                if let Some(x) = d.friend_mut(&f.id) {
                                    x.permission = perm;
                                }
                            });
                            ui.close();
                        }
                    }
                    ui.separator();
                    if ui.button("📋 Copier son code").clicked() {
                        ui.ctx().copy_text(format!("{}{}", crate::store::CODE_PREFIX, f.id));
                        ui.close();
                    }
                    if ui.button("🗑 Retirer des amis").clicked() {
                        self.node.store.update(|d| d.remove_friend(&f.id));
                        ui.close();
                    }
                });
                resp.on_hover_text("Clic droit : droits, retirer");
            }

            if !incoming.is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new("Demandes reçues").strong());
                for r in incoming {
                    ui.horizontal(|ui| {
                        ui.label(&r.name);
                        if ui.button("✔").on_hover_text("Accepter").clicked()
                            && let Ok(id) = parse_friend_code(&r.id) {
                                self.rt.spawn(self.node.clone().accept_friend(id));
                            }
                        if ui.button("✖").on_hover_text("Refuser").clicked() {
                            self.node.refuse_friend(&r.id);
                        }
                    });
                }
            }
            if !outgoing.is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new("En attente de réponse").weak());
                for o in outgoing {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{}…", &o[..10.min(o.len())])).monospace().weak());
                        if ui.small_button("✖").on_hover_text("Annuler").clicked() {
                            self.node.store.update(|d| d.outgoing.retain(|x| x != &o));
                        }
                    });
                }
            }
        });

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(8.0);
            if ui.button("↻ Actualiser").clicked() {
                let n = self.node.clone();
                self.rt.spawn(async move { n.refresh_all().await });
            }
            ui.horizontal(|ui| {
                let add = ui.button("➕ Ajouter");
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.add_code)
                        .hint_text("ms-…")
                        .desired_width(ui.available_width()),
                );
                if add.clicked() || (field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                    match parse_friend_code(&self.add_code) {
                        Ok(id) => {
                            self.rt.spawn(self.node.clone().send_friend_request(id));
                            self.add_code.clear();
                        }
                        Err(_) => self.toast("Code ami invalide"),
                    }
                }
            });
            ui.label("Ajouter un ami avec son code :");
            ui.separator();
        });
    }

    // ---------------------------------------------------------------- share tab

    fn share_tab(&mut self, ui: &mut egui::Ui) {
        let hosting = self.node.live.lock().unwrap().hosting.clone();
        ui.heading("Partager mon écran");
        ui.add_space(8.0);
        if let Some(h) = hosting {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("🔴 EN DIRECT").strong().color(LIVE));
                    let secs = h.started.elapsed().as_secs();
                    ui.label(format!("{:02}:{:02}", secs / 60, secs % 60));
                });
                ui.label(format!(
                    "{} — {}x{} @ {} i/s{}",
                    h.info.title,
                    h.info.width,
                    h.info.height,
                    h.info.fps,
                    if h.info.audio { " · son activé" } else { "" }
                ));
                ui.label(RichText::new(format!("Encodage matériel : {}", h.encoder)).small().weak());
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Visible par :");
                    let mut a = h.audience.clone();
                    if self.audience_combo(ui, "live_audience", &mut a) {
                        self.node.set_audience(a.clone());
                        self.audience = a;
                    }
                });
                ui.add_space(6.0);
                ui.label(RichText::new(format!("Spectateurs ({})", h.viewers.len())).strong());
                if h.viewers.is_empty() {
                    ui.label(RichText::new("Personne pour l'instant").weak());
                }
                for v in &h.viewers {
                    ui.horizontal(|ui| {
                        ui.label(self.node.store.lock().display_name(v));
                        if ui.small_button("Exclure").clicked() {
                            self.node.kick(v);
                        }
                    });
                }
                ui.add_space(8.0);
                if ui.add(egui::Button::new(RichText::new("⏹ Arrêter le partage").size(16.0)).min_size(egui::vec2(200.0, 34.0))).clicked() {
                    let n = self.node.clone();
                    self.rt.spawn_blocking(move || n.stop_host());
                }
            });
        } else {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Source").strong());
                if ui.small_button("↻").on_hover_text("Rafraîchir la liste").clicked() {
                    self.sources = list_sources();
                    self.selected = self.selected.min(self.sources.len().saturating_sub(1));
                }
            });
            egui::Frame::group(ui.style()).show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("sources").max_height(200.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for (i, s) in self.sources.iter().enumerate() {
                        let icon = match s.kind {
                            crate::media::capture::SourceKind::Monitor(_) => "🖥",
                            crate::media::capture::SourceKind::Window(_) => "🗗",
                        };
                        ui.selectable_value(&mut self.selected, i, format!("{icon} {}", s.name));
                    }
                });
            });
            ui.add_space(6.0);
            let mut settings = self.node.store.lock().settings.clone();
            let before = (settings.quality.clone(), settings.audio);
            egui::Grid::new("share_opts").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Qualité");
                egui::ComboBox::from_id_salt("quality").selected_text(settings.quality.label()).show_ui(ui, |ui| {
                    for q in [Quality::Eco, Quality::Standard, Quality::Smooth] {
                        let l = q.label();
                        ui.selectable_value(&mut settings.quality, q, l);
                    }
                });
                ui.end_row();
                ui.label("Son");
                ui.checkbox(&mut settings.audio, "Partager le son de l'ordinateur");
                ui.end_row();
                ui.label("Visible par");
                let mut a = self.audience.clone();
                if self.audience_combo(ui, "audience", &mut a) {
                    self.audience = a;
                }
                ui.end_row();
            });
            if (settings.quality.clone(), settings.audio) != before {
                self.node.store.update(|d| d.settings = settings);
            }
            ui.add_space(10.0);
            let label = if self.starting { "Démarrage…" } else { "▶ Démarrer le partage" };
            let btn = egui::Button::new(RichText::new(label).size(16.0).color(Color32::WHITE))
                .fill(ACCENT)
                .min_size(egui::vec2(220.0, 36.0));
            if ui.add_enabled(!self.starting && !self.sources.is_empty(), btn).clicked()
                && let Some(src) = self.sources.get(self.selected).cloned() {
                    self.starting = true;
                    let (n, a) = (self.node.clone(), self.audience.clone());
                    self.rt.spawn_blocking(move || {
                        if let Err(e) = n.start_host(&src, a) {
                            tracing::error!("partage: {e:#}");
                            n.notify_ui(UiEvent::HostFailed(format!("Impossible de partager : {e:#}")));
                        }
                    });
                }
        }
        if self.starting && self.node.live.lock().unwrap().hosting.is_some() {
            self.starting = false;
        }

        ui.add_space(16.0);
        ui.separator();
        ui.heading("Envoyer une capture d'écran");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("À :");
            let mut a = self.shot_audience.clone();
            if self.audience_combo(ui, "shot_audience", &mut a) {
                self.shot_audience = a;
            }
            if ui.button("📸 Capturer et envoyer").clicked() {
                self.capture_and_send();
            }
        });
        let source_hint = if self.node.live.lock().unwrap().hosting.is_some() {
            "Capture l'écran partagé, en pleine résolution.".to_string()
        } else {
            match self.sources.get(self.selected) {
                Some(s) => format!("Capture : {}", s.name),
                None => "Aucune source".into(),
            }
        };
        ui.label(RichText::new(source_hint).small().weak());
        ui.horizontal(|ui| {
            if ui.small_button("📂 Ouvrir le dossier des captures").clicked() {
                let dir = self.node.store.lock().settings.screenshots_dir.clone();
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::process::Command::new("explorer").arg(dir).spawn();
            }
        });
    }

    fn capture_and_send(&mut self) {
        let node = self.node.clone();
        let source = self.sources.get(self.selected).cloned();
        let audience = self.shot_audience.clone();
        let rt = self.rt.clone();
        self.rt.spawn_blocking(move || {
            crate::media::encoder::com_init();
            let img = match node.host_screenshot() {
                Some(r) => r,
                None => match &source {
                    Some(s) => grab_once(&s.kind),
                    None => Err(anyhow::anyhow!("aucune source")),
                },
            };
            let result = img.and_then(|(w, h, rgba)| {
                let png = encode_png(w, h, rgba)?;
                let name = timestamp_name();
                let dir = node.store.lock().settings.screenshots_dir.join("envoyées");
                std::fs::create_dir_all(&dir)?;
                std::fs::write(dir.join(&name), &png)?;
                Ok((png, name))
            });
            match result {
                Ok((png, name)) => {
                    rt.spawn(node.clone().send_screenshot(audience, Arc::new(png), name));
                }
                Err(e) => tracing::error!("capture: {e:#}"),
            }
        });
    }

    // ---------------------------------------------------------------- friends tab

    fn friends_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Droits par ami");
        ui.label(RichText::new("Qui peut regarder tes partages sans que tu aies à accepter.").weak());
        ui.add_space(6.0);
        let friends = self.node.store.lock().friends.clone();
        egui::Grid::new("perm_grid").num_columns(3).striped(true).spacing([16.0, 6.0]).show(ui, |ui| {
            for f in &friends {
                ui.label(&f.name);
                let mut perm = f.permission;
                egui::ComboBox::from_id_salt(("perm", &f.id)).selected_text(perm.label()).show_ui(ui, |ui| {
                    for p in [Permission::Ask, Permission::Always, Permission::Blocked] {
                        ui.selectable_value(&mut perm, p, p.label());
                    }
                });
                if perm != f.permission {
                    self.node.store.update(|d| {
                        if let Some(x) = d.friend_mut(&f.id) {
                            x.permission = perm;
                        }
                    });
                }
                if ui.small_button("Retirer").clicked() {
                    self.node.store.update(|d| d.remove_friend(&f.id));
                }
                ui.end_row();
            }
        });

        ui.add_space(16.0);
        ui.separator();
        ui.heading("Groupes");
        ui.label(
            RichText::new("Un groupe = une liste d'amis. Tu choisis ensuite « Visible par : groupe » quand tu partages.")
                .weak(),
        );
        ui.add_space(6.0);
        let groups = self.node.store.lock().groups.clone();
        for g in groups {
            egui::CollapsingHeader::new(format!("{} ({} membres)", g.name, g.members.len()))
                .id_salt(("group", g.id))
                .show(ui, |ui| {
                    for f in &friends {
                        let mut member = g.members.contains(&f.id);
                        if ui.checkbox(&mut member, &f.name).changed() {
                            self.node.store.update(|d| {
                                if let Some(gr) = d.groups.iter_mut().find(|x| x.id == g.id) {
                                    gr.members.retain(|m| m != &f.id);
                                    if member {
                                        gr.members.push(f.id.clone());
                                    }
                                }
                            });
                        }
                    }
                    if friends.is_empty() {
                        ui.label(RichText::new("Ajoute d'abord des amis.").weak());
                    }
                    if ui.small_button("🗑 Supprimer le groupe").clicked() {
                        self.node.store.update(|d| d.groups.retain(|x| x.id != g.id));
                    }
                });
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.new_group).hint_text("Nom du groupe").desired_width(180.0));
            if ui.button("➕ Créer le groupe").clicked() && !self.new_group.trim().is_empty() {
                let name = self.new_group.trim().to_string();
                self.node.store.update(|d| {
                    let id = d.groups.iter().map(|g| g.id).max().unwrap_or(0) + 1;
                    d.groups.push(crate::store::Group { id, name, members: vec![] });
                });
                self.new_group.clear();
            }
        });
    }

    // ---------------------------------------------------------------- watch view

    fn update_texture(&mut self, ctx: &egui::Context) {
        if let Some(f) = self.node.frame.lock().unwrap().take() {
            let img = egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba);
            match &mut self.texture {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("video", img, egui::TextureOptions::LINEAR)),
            }
            self.last_frame = Some((f.width, f.height, f.rgba));
            self.fps.1 += 1;
        }
        let el = self.fps.0.elapsed().as_secs_f32();
        if el >= 1.0 {
            self.fps.2 = self.fps.1 as f32 / el;
            self.fps = (Instant::now(), 0, self.fps.2);
        }
    }

    fn watch_toolbar(&mut self, ui: &mut egui::Ui) {
        let (watching, connecting) = {
            let l = self.node.live.lock().unwrap();
            (l.watching.clone(), l.connecting_to.clone())
        };
        ui.horizontal(|ui| {
            if let Some(w) = &watching {
                let name = self.node.store.lock().display_name(&w.friend);
                ui.label(RichText::new(format!("▶ {name}")).strong().color(LIVE));
                ui.label(RichText::new(format!("{} · {:.0} i/s", w.info.title, self.fps.2)).weak());
            } else if let Some(c) = &connecting {
                ui.spinner();
                ui.label(format!(
                    "Connexion à {}… (en attente d'acceptation)",
                    self.node.store.lock().display_name(c)
                ));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✖ Quitter").clicked() {
                    self.node.stop_watching();
                    self.set_fullscreen(ui.ctx(), false);
                }
                if watching.is_some() {
                    let fs = if self.fullscreen { "Fenêtré" } else { "Plein écran" };
                    if ui.button(fs).clicked() {
                        let f = !self.fullscreen;
                        self.set_fullscreen(ui.ctx(), f);
                    }
                    if ui.button("📸 Capture").on_hover_text("Enregistre l'image en cours").clicked() {
                        self.save_viewer_capture();
                    }
                    let mut vol = self.node.volume.load(Ordering::Relaxed);
                    if watching.as_ref().is_some_and(|w| w.info.audio)
                        && ui.add(egui::Slider::new(&mut vol, 0..=150).suffix("%").text("🔊")).changed()
                    {
                        self.node.volume.store(vol, Ordering::Relaxed);
                        self.node.store.update(|d| d.settings.volume = vol);
                    }
                }
            });
        });
    }

    fn set_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        self.fullscreen = on;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
    }

    fn save_viewer_capture(&mut self) {
        let Some((w, h, rgba)) = self.last_frame.clone() else { return };
        let dir = self.node.store.lock().settings.screenshots_dir.clone();
        let node = self.node.clone();
        let watching = node.live.lock().unwrap().watching.as_ref().map(|w| node.store.lock().display_name(&w.friend));
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<PathBuf> {
                let png = encode_png(w, h, rgba)?;
                std::fs::create_dir_all(&dir)?;
                let name = format!("{}_{}", watching.unwrap_or_default(), timestamp_name());
                let path = dir.join(name);
                std::fs::write(&path, png)?;
                Ok(path)
            })();
            match res {
                Ok(p) => node.notify_ui(UiEvent::Screenshot { from: "Toi".into(), path: p }),
                Err(e) => node.notify_ui(UiEvent::Toast(format!("Capture impossible : {e}"))),
            }
        });
    }

    fn video(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
        if let Some(t) = &self.texture {
            let [tw, th] = t.size();
            let scale = (rect.width() / tw as f32).min(rect.height() / th as f32);
            let size = egui::vec2(tw as f32 * scale, th as f32 * scale);
            let r = egui::Rect::from_center_size(rect.center(), size);
            ui.painter().image(t.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        } else {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "En attente de l'image…", egui::FontId::proportional(18.0), Color32::GRAY);
        }
        let resp = ui.allocate_rect(rect, egui::Sense::click());
        if resp.double_clicked() {
            let f = !self.fullscreen;
            self.set_fullscreen(ui.ctx(), f);
        }
    }

    // ---------------------------------------------------------------- overlays

    fn watch_requests(&mut self, ctx: &egui::Context) {
        let mut answered = vec![];
        for (i, r) in self.requests.iter().enumerate() {
            egui::Window::new("Demande de visionnage")
                .id(egui::Id::unique(("watchreq", r.req_id)))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, i as f32 * 30.0))
                .show(ctx, |ui| {
                    ui.label(RichText::new(format!("{} veut regarder ton écran", r.name)).size(16.0));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("✔ Accepter").clicked() {
                            answered.push((i, true, false));
                        }
                        if ui.button("Toujours autoriser").clicked() {
                            answered.push((i, true, true));
                        }
                        if ui.button("✖ Refuser").clicked() {
                            answered.push((i, false, false));
                        }
                    });
                });
        }
        for (i, accept, always) in answered.into_iter().rev() {
            let r = self.requests.remove(i);
            if always {
                self.node.store.update(|d| {
                    if let Some(f) = d.friend_mut(&r.friend) {
                        f.permission = Permission::Always;
                    }
                });
            }
            self.node.answer_watch(r.req_id, accept);
        }
    }

    fn toasts(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|t| t.at.elapsed() < Duration::from_secs(if t.path.is_some() { 12 } else { 6 }));
        if self.toasts.is_empty() {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        egui::Area::new(egui::Id::unique("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -12.0))
            .show(ctx, |ui| {
                for t in &self.toasts {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_max_width(320.0);
                        ui.label(&t.text);
                        if let Some(p) = &t.path
                            && ui.small_button("Ouvrir").clicked() {
                                let _ = std::process::Command::new("explorer").arg(p).spawn();
                            }
                    });
                    ui.add_space(4.0);
                }
            });
    }
}

impl App {
    fn dev_auto(&mut self) {
        if self.auto_start && self.node.live.lock().unwrap().online {
            self.auto_start = false;
            if let Some(src) = self.sources.first().cloned() {
                let n = self.node.clone();
                self.rt.spawn_blocking(move || n.start_host(&src, Audience::AllFriends));
            }
        }
        if self.auto_watch {
            let live = self.node.live.lock().unwrap().peers.iter().find(|(_, p)| p.stream.is_some()).map(|(k, _)| k.clone());
            if let Some(id) = live.and_then(|k| parse_friend_code(&k).ok()) {
                self.auto_watch = false;
                self.rt.spawn(self.node.clone().watch(id));
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_events();
        self.dev_auto();
        self.update_texture(&ctx);
        let in_watch = {
            let l = self.node.live.lock().unwrap();
            l.watching.is_some() || l.connecting_to.is_some()
        };
        if !in_watch {
            self.texture = None;
            self.last_frame = None;
            if self.fullscreen {
                self.set_fullscreen(&ctx, false);
            }
        }
        if self.fullscreen && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.set_fullscreen(&ctx, false);
        }

        if !self.fullscreen {
            egui::Panel::top("top").show(ui, |ui| {
                ui.add_space(4.0);
                self.top_bar(ui);
                ui.add_space(4.0);
                self.update_banner(ui);
            });
            egui::Panel::left("friends").resizable(true).default_size(260.0).min_size(200.0).show(ui, |ui| {
                self.friends_panel(ui);
            });
        }
        egui::CentralPanel::default_margins().show(ui, |ui| {
            if in_watch {
                if !self.fullscreen {
                    self.watch_toolbar(ui);
                    ui.add_space(4.0);
                } else {
                    // Thin toolbar on hover in fullscreen.
                    let hover_top = ctx.input(|i| i.pointer.hover_pos().is_some_and(|p| p.y < 60.0));
                    if hover_top {
                        self.watch_toolbar(ui);
                    }
                }
                self.video(ui);
            } else {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::Share, RichText::new("🖥 Partager").size(15.0));
                    ui.selectable_value(&mut self.tab, Tab::Friends, RichText::new("👥 Amis & groupes").size(15.0));
                });
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                    Tab::Share => self.share_tab(ui),
                    Tab::Friends => self.friends_tab(ui),
                });
            }
        });
        self.watch_requests(&ctx);
        self.toasts(&ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.node.stop_watching();
        self.node.stop_host();
    }
}

fn encode_png(w: u32, h: u32, rgba: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    let img = image::RgbaImage::from_raw(w, h, rgba).ok_or_else(|| anyhow::anyhow!("image invalide"))?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

fn timestamp_name() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("capture_{secs}.png")
}
