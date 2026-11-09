//! egui interface. The UI only repaints on events (new video frame, network
//! event, user input), so it costs nothing while idle.

pub mod theme;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Align2, Color32, CornerRadius, Id, Layout, Margin, RichText, Sense, Stroke, vec2};

use crate::media::capture::{Source, SourceKind, grab_once, list_sources};
use crate::net::{Audience, Node, PeerState, UiEvent};
use crate::store::{Friend, Permission, Quality, friend_code, parse_friend_code};
use crate::update::{self, Release};
use theme::{
    ACCENT, ACCENT_SOFT, BG, BORDER, CARD, ELEVATED, FAINT, INPUT, Kind, LIVE, MUTED, ONLINE, SIDEBAR, TEXT, icon,
};

#[derive(PartialEq, Clone, Copy)]
enum Page {
    Share,
    Friends,
    Settings,
}

enum Modal {
    Onboarding,
    AddFriend,
    RemoveFriend { id: String, name: String },
}

struct Toast {
    text: String,
    at: Instant,
    path: Option<PathBuf>,
}

struct WatchRequest {
    req_id: u64,
    friend: String,
    name: String,
}

enum UpdateState {
    None,
    Checking,
    Available(Release),
    Installing(String),
    Done,
    Failed(String),
}

/// Source previews, captured in the background.
struct Thumbs {
    tex: HashMap<String, egui::TextureHandle>,
    tx: std::sync::mpsc::Sender<(String, egui::ColorImage)>,
    rx: std::sync::mpsc::Receiver<(String, egui::ColorImage)>,
    requested: bool,
}

fn source_key(kind: &SourceKind) -> String {
    format!("{kind:?}")
}

pub struct App {
    node: Arc<Node>,
    rt: tokio::runtime::Handle,
    events: std::sync::mpsc::Receiver<UiEvent>,
    page: Page,
    sources: Vec<Source>,
    selected: usize,
    audience: Audience,
    shot_audience: Audience,
    add_code: String,
    add_error: Option<String>,
    new_group: String,
    name_edit: String,
    toasts: Vec<Toast>,
    requests: Vec<WatchRequest>,
    modal: Option<Modal>,
    modal_opened: bool,
    texture: Option<egui::TextureHandle>,
    logo: Option<egui::TextureHandle>,
    last_frame: Option<(u32, u32, Vec<u8>)>,
    fullscreen: bool,
    starting: bool,
    fps: (Instant, u64, f32),
    last_pointer_move: Instant,
    last_pointer_pos: Option<egui::Pos2>,
    thumbs: Thumbs,
    /// Dev/test helpers: MYSTREAM_AUTOSTART=1 shares the first screen,
    /// MYSTREAM_AUTOWATCH=1 watches the first friend who shares.
    auto_start: bool,
    auto_watch: bool,
    update: Arc<std::sync::Mutex<UpdateState>>,
    ctx: egui::Context,
}

impl App {
    pub fn new(
        node: Arc<Node>,
        rt: tokio::runtime::Handle,
        events: std::sync::mpsc::Receiver<UiEvent>,
        ctx: egui::Context,
    ) -> Self {
        let (name, onboarded) = {
            let d = node.store.lock();
            (d.name.clone(), d.onboarded)
        };
        let update = Arc::new(std::sync::Mutex::new(UpdateState::None));
        spawn_update_check(&rt, update.clone(), ctx.clone(), false);
        let (tx, rx) = std::sync::mpsc::channel();
        let logo = image::load_from_memory(include_bytes!("../../assets/icon.png")).ok().map(|i| {
            let i = i.to_rgba8();
            let img = egui::ColorImage::from_rgba_unmultiplied([i.width() as usize, i.height() as usize], i.as_raw());
            ctx.load_texture("logo", img, egui::TextureOptions::LINEAR)
        });
        Self {
            node,
            rt,
            events,
            page: Page::Share,
            sources: list_sources(),
            selected: 0,
            audience: Audience::AllFriends,
            shot_audience: Audience::AllFriends,
            add_code: String::new(),
            add_error: None,
            new_group: String::new(),
            name_edit: name,
            toasts: vec![],
            requests: vec![],
            modal: (!onboarded).then_some(Modal::Onboarding),
            modal_opened: false,
            texture: None,
            logo,
            last_frame: None,
            fullscreen: false,
            starting: false,
            fps: (Instant::now(), 0, 0.0),
            last_pointer_move: Instant::now(),
            last_pointer_pos: None,
            thumbs: Thumbs { tex: HashMap::new(), tx, rx, requested: false },
            auto_start: std::env::var("MYSTREAM_AUTOSTART").is_ok(),
            auto_watch: std::env::var("MYSTREAM_AUTOWATCH").is_ok(),
            update,
            ctx,
        }
    }

    fn toast(&mut self, text: impl Into<String>) {
        self.toasts.push(Toast { text: text.into(), at: Instant::now(), path: None });
    }

    fn open_modal(&mut self, m: Modal) {
        self.modal = Some(m);
        self.modal_opened = true;
    }

    fn my_name(&self) -> String {
        self.node.store.lock().name.clone()
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
            .width(220.0)
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
                    text: format!("{from} t'a envoyé une capture"),
                    at: Instant::now(),
                    path: Some(path),
                }),
            }
        }
        while let Ok((key, img)) = self.thumbs.rx.try_recv() {
            match self.thumbs.tex.get_mut(&key) {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => {
                    let t = self.ctx.load_texture(format!("thumb-{key}"), img, egui::TextureOptions::LINEAR);
                    self.thumbs.tex.insert(key, t);
                }
            }
        }
    }

    fn refresh_sources(&mut self, ctx: &egui::Context) {
        self.sources = list_sources();
        self.selected = self.selected.min(self.sources.len().saturating_sub(1));
        spawn_thumbnails(self.sources.clone(), self.thumbs.tx.clone(), ctx.clone());
        self.thumbs.requested = true;
    }

    // ================================================================ sidebar

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let online = self.node.live.lock().unwrap().online;
        let me = friend_code(&self.node.id()).trim_start_matches(crate::store::CODE_PREFIX).to_string();
        let my_name = self.my_name();

        egui::Frame::new().inner_margin(Margin::same(12)).show(ui, |ui| {
            // Profile card
            egui::Frame::new()
                .fill(CARD)
                .corner_radius(CornerRadius::same(12))
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        theme::avatar(ui, &me, &my_name, 40.0, Some(if online { ONLINE } else { FAINT }), CARD);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            theme::truncated(ui, &my_name, theme::semibold(15.0), TEXT, 150.0);
                            let (c, t) = if online { (ONLINE, "En ligne") } else { (MUTED, "Connexion…") };
                            ui.label(RichText::new(t).size(12.5).color(c));
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if theme::icon_button(ui, icon::SETTINGS, "Paramètres").clicked() {
                                self.page = Page::Settings;
                            }
                        });
                    });
                    ui.add_space(4.0);
                    let w = ui.available_width();
                    if theme::button_w(ui, Kind::Secondary, Some(icon::COPY), "Copier mon code ami", 34.0, true, Some(w))
                        .on_hover_text("Envoie ce code à tes amis (Discord, WhatsApp…) pour qu'ils t'ajoutent")
                        .clicked()
                    {
                        ui.ctx().copy_text(friend_code(&self.node.id()));
                        self.toast("Code copié ! Colle-le à tes amis sur Discord, WhatsApp…");
                    }
                });
        });

        // Bottom action
        egui::Panel::bottom("sidebar_bottom")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(Margin::same(12)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let w = ui.available_width() - 40.0;
                    if theme::button_w(ui, Kind::Primary, Some(icon::ADD_FRIEND), "Ajouter un ami", 38.0, true, Some(w)).clicked() {
                        self.add_code.clear();
                        self.add_error = None;
                        self.open_modal(Modal::AddFriend);
                    }
                    if theme::icon_button_sized(ui, icon::REFRESH, "Actualiser les amis", 38.0, MUTED).clicked() {
                        let n = self.node.clone();
                        self.rt.spawn(async move { n.refresh_all().await });
                    }
                });
            });

        let (friends, incoming, outgoing) = {
            let d = self.node.store.lock();
            (d.friends.clone(), d.incoming.clone(), d.outgoing.clone())
        };
        let (peers, watching) = {
            let l = self.node.live.lock().unwrap();
            (l.peers.clone(), l.watching.as_ref().map(|w| w.friend.clone()))
        };
        let state = |f: &Friend| peers.get(&f.id).cloned().unwrap_or_default();
        let mut sorted = friends.clone();
        sorted.sort_by_key(|f| f.name.to_lowercase());
        let live: Vec<&Friend> = sorted.iter().filter(|f| state(f).stream.is_some() && f.permission != Permission::Blocked).collect();
        let on: Vec<&Friend> = sorted.iter().filter(|f| { let s = state(f); s.online && s.stream.is_none() }).collect();
        let off: Vec<&Friend> = sorted.iter().filter(|f| !state(f).online && state(f).stream.is_none()).collect();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::new().inner_margin(Margin::symmetric(12, 0)).show(ui, |ui| {
                if !incoming.is_empty() {
                    ui.add_space(4.0);
                    theme::section(ui, &format!("Demandes d'ami · {}", incoming.len()));
                    for r in &incoming {
                        theme::row(ui, 48.0, false, |ui| {
                            theme::avatar(ui, &r.id, &r.name, 32.0, None, SIDEBAR);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                theme::truncated(ui, &r.name, theme::semibold(14.0), TEXT, 110.0);
                                ui.label(RichText::new("veut être ton ami").size(12.0).color(MUTED));
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if theme::icon_button_sized(ui, icon::CLOSE, "Refuser", 30.0, MUTED).clicked() {
                                    self.node.refuse_friend(&r.id);
                                }
                                if theme::icon_button_sized(ui, icon::CHECK, "Accepter", 30.0, ONLINE).clicked()
                                    && let Ok(id) = parse_friend_code(&r.id)
                                {
                                    self.rt.spawn(self.node.clone().accept_friend(id));
                                }
                            });
                        });
                    }
                    ui.add_space(8.0);
                }

                if friends.is_empty() && incoming.is_empty() {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        ui.label(theme::icon_text(icon::PEOPLE, 34.0).color(FAINT));
                        ui.add_space(6.0);
                        ui.label(theme::strong("Pas encore d'amis", 15.0));
                        ui.label(RichText::new("Ajoute un ami avec son code\npour partager vos écrans.").color(MUTED));
                    });
                }

                for (label, list) in [("En direct", &live), ("En ligne", &on), ("Hors ligne", &off)] {
                    if list.is_empty() {
                        continue;
                    }
                    ui.add_space(4.0);
                    theme::section(ui, &format!("{label} · {}", list.len()));
                    for f in list.iter() {
                        self.friend_row(ui, f, &state(f), watching.as_deref() == Some(f.id.as_str()));
                    }
                    ui.add_space(8.0);
                }

                if !outgoing.is_empty() {
                    theme::section(ui, &format!("En attente · {}", outgoing.len()));
                    for o in &outgoing {
                        theme::row(ui, 40.0, false, |ui| {
                            ui.label(theme::icon_text(icon::SEND, 14.0).color(FAINT));
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                ui.label(RichText::new(format!("ms-{}…", &o[..8.min(o.len())])).monospace().color(MUTED));
                                ui.label(RichText::new("demande envoyée").size(11.5).color(FAINT));
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if theme::icon_button_sized(ui, icon::CLOSE, "Annuler la demande", 28.0, FAINT).clicked() {
                                    self.node.store.update(|d| d.outgoing.retain(|x| x != o));
                                }
                            });
                        });
                    }
                }
                ui.add_space(8.0);
            });
        });
    }

    fn friend_row(&mut self, ui: &mut egui::Ui, f: &Friend, p: &PeerState, is_watching: bool) {
        let blocked = f.permission == Permission::Blocked;
        let status = if blocked {
            FAINT
        } else if p.stream.is_some() {
            LIVE
        } else if p.online {
            ONLINE
        } else {
            FAINT
        };
        let dim = !p.online && p.stream.is_none();
        let (resp, _) = theme::row(ui, 50.0, is_watching, |ui| {
            if dim {
                ui.multiply_opacity(0.55);
            }
            theme::avatar(ui, &f.id, &f.name, 34.0, Some(status), SIDEBAR);
            let live = p.stream.is_some() && !blocked;
            let name_w = if live { 96.0 } else { 160.0 };
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                theme::truncated(ui, &f.name, theme::semibold(14.0), TEXT, name_w);
                let (sub, c) = match &p.stream {
                    Some(s) if !blocked => (format!("{} · {}p", s.title, s.height), LIVE),
                    _ if blocked => ("bloqué".into(), FAINT),
                    _ if p.online => ("en ligne".into(), MUTED),
                    _ => ("hors ligne".into(), FAINT),
                };
                theme::truncated(ui, &sub, egui::FontId::proportional(12.0), c, name_w);
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if live {
                    if is_watching {
                        theme::badge(ui, "TU REGARDES", ACCENT_SOFT, TEXT);
                    } else if theme::button(ui, Kind::Live, Some(icon::PLAY), "Regarder", 28.0, true).clicked()
                        && let Ok(id) = parse_friend_code(&f.id)
                    {
                        self.rt.spawn(self.node.clone().watch(id));
                    }
                }
            });
        });
        resp.context_menu(|ui| self.friend_menu(ui, f));
    }

    fn friend_menu(&mut self, ui: &mut egui::Ui, f: &Friend) {
        ui.set_min_width(220.0);
        ui.label(theme::strong(&f.name, 14.0));
        ui.label(RichText::new("Quand cet ami veut regarder :").size(12.0).color(MUTED));
        for perm in [Permission::Ask, Permission::Always, Permission::Blocked] {
            if ui.radio(f.permission == perm, perm.label()).clicked() {
                self.set_permission(&f.id, perm);
                ui.close();
            }
        }
        ui.separator();
        if ui.button("Copier son code").clicked() {
            ui.ctx().copy_text(format!("{}{}", crate::store::CODE_PREFIX, f.id));
            ui.close();
        }
        if ui.button(RichText::new("Retirer des amis").color(LIVE)).clicked() {
            self.open_modal(Modal::RemoveFriend { id: f.id.clone(), name: f.name.clone() });
            ui.close();
        }
    }

    fn set_permission(&self, id: &str, perm: Permission) {
        self.node.store.update(|d| {
            if let Some(x) = d.friend_mut(id) {
                x.permission = perm;
            }
        });
    }

    // ================================================================ main area

    fn main_view(&mut self, ui: &mut egui::Ui) {
        let hosting = self.node.live.lock().unwrap().hosting.is_some();
        egui::Frame::new().inner_margin(Margin { left: 28, right: 28, top: 16, bottom: 8 }).show(ui, |ui| {
            ui.horizontal(|ui| {
                for (p, ic, label) in [
                    (Page::Share, icon::MONITOR, "Partager"),
                    (Page::Friends, icon::PEOPLE, "Amis & groupes"),
                    (Page::Settings, icon::SETTINGS, "Paramètres"),
                ] {
                    let kind = if self.page == p { Kind::Secondary } else { Kind::Ghost };
                    if theme::button(ui, kind, Some(ic), label, 36.0, true).clicked() {
                        self.page = p;
                    }
                }
                if hosting {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        theme::badge(ui, "● EN DIRECT", LIVE, Color32::WHITE);
                    });
                }
            });
            self.update_banner(ui);
        });

        if self.page == Page::Share {
            self.share_bar(ui);
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::new().inner_margin(Margin { left: 28, right: 28, top: 8, bottom: 28 }).show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(980.0));
                match self.page {
                    Page::Share => self.share_page(ui),
                    Page::Friends => self.friends_page(ui),
                    Page::Settings => self.settings_page(ui),
                }
            });
        });
    }

    fn update_banner(&mut self, ui: &mut egui::Ui) {
        let mut state = self.update.lock().unwrap();
        if matches!(*state, UpdateState::None | UpdateState::Checking) {
            return;
        }
        if matches!(*state, UpdateState::Done) {
            *state = UpdateState::None;
            self.node.stop_watching();
            self.node.stop_host();
            match update::relaunch() {
                Ok(()) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                Err(e) => *state = UpdateState::Failed(format!("redémarrage impossible : {e}")),
            }
            return;
        }
        ui.add_space(10.0);
        egui::Frame::new()
            .fill(ACCENT_SOFT)
            .stroke(Stroke::new(1.0, ACCENT.gamma_multiply(0.6)))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| match &*state {
                    UpdateState::Available(r) => {
                        let r = r.clone();
                        ui.label(theme::icon_text(icon::DOWNLOAD, 16.0).color(theme::ACCENT_HOVER));
                        ui.label(theme::strong(format!("Nouvelle version {} disponible", r.version), 14.5));
                        ui.label(theme::muted(format!("(tu as {})", env!("CARGO_PKG_VERSION"))));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if theme::button(ui, Kind::Ghost, None, "Plus tard", 30.0, true).clicked() {
                                *state = UpdateState::None;
                            }
                            if theme::button(ui, Kind::Primary, None, "Mettre à jour", 30.0, true).clicked() {
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
                        });
                    }
                    UpdateState::Installing(v) => {
                        ui.spinner();
                        ui.label(theme::strong(format!("Téléchargement de la version {v}…"), 14.5));
                        ui.label(theme::muted("MyStream va redémarrer tout seul."));
                    }
                    UpdateState::Failed(e) => {
                        let e = e.clone();
                        ui.label(RichText::new(format!("Mise à jour impossible : {e}")).color(LIVE));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if theme::button(ui, Kind::Ghost, None, "OK", 30.0, true).clicked() {
                                *state = UpdateState::None;
                            }
                            ui.hyperlink_to("Télécharger à la main", update::RELEASES_URL);
                        });
                    }
                    _ => {}
                });
            });
    }

    // ---------------------------------------------------------------- share

    fn share_page(&mut self, ui: &mut egui::Ui) {
        let hosting = self.node.live.lock().unwrap().hosting.clone();
        if !self.thumbs.requested {
            let ctx = ui.ctx().clone();
            self.refresh_sources(&ctx);
        }
        if let Some(h) = hosting {
            ui.ctx().request_repaint_after(Duration::from_secs(1));
            ui.add_space(8.0);
            ui.label(theme::title("Tu es en direct"));
            ui.label(theme::muted("Tes amis peuvent te regarder. Tu peux continuer à jouer normalement."));
            ui.add_space(14.0);
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    let key = self.sources.iter().find(|s| s.name == h.info.title).map(|s| source_key(&s.kind));
                    let tex = key.and_then(|k| self.thumbs.tex.get(&k).cloned());
                    thumb(ui, tex.as_ref(), vec2(240.0, 135.0), icon::MONITOR);
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        theme::badge(ui, "● EN DIRECT", LIVE, Color32::WHITE);
                        ui.add_space(2.0);
                        theme::truncated(ui, &h.info.title, theme::semibold(17.0), TEXT, ui.available_width());
                        ui.label(theme::muted(format!(
                            "{}×{} · {} i/s{}",
                            h.info.width,
                            h.info.height,
                            h.info.fps,
                            if h.info.audio { " · son partagé" } else { " · sans son" }
                        )));
                        let engine = if h.encoder.contains("OpenH264") {
                            "Encodage : logiciel (processeur)".to_string()
                        } else {
                            format!("Encodage : carte graphique ({})", h.encoder)
                        };
                        ui.label(RichText::new(engine).size(12.0).color(FAINT));
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label(theme::muted("Visible par"));
                            let mut a = h.audience.clone();
                            if self.audience_combo(ui, "live_audience", &mut a) {
                                self.node.set_audience(a.clone());
                                self.audience = a;
                            }
                        });
                    });
                });
            });
            ui.add_space(12.0);
            theme::card(ui, |ui| {
                ui.label(theme::strong(format!("Spectateurs · {}", h.viewers.len()), 16.0));
                ui.add_space(4.0);
                if h.viewers.is_empty() {
                    ui.label(theme::muted(
                        "Personne pour l'instant. Tes amis voient un bouton « Regarder » à côté de ton nom.",
                    ));
                }
                for v in &h.viewers {
                    let name = self.node.store.lock().display_name(v);
                    theme::row(ui, 44.0, false, |ui| {
                        theme::avatar(ui, v, &name, 30.0, Some(ONLINE), CARD);
                        ui.label(theme::strong(&name, 14.0));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if theme::button(ui, Kind::Ghost, Some(icon::CLOSE), "Exclure", 28.0, true).clicked() {
                                self.node.kick(v);
                            }
                        });
                    });
                }
            });
        } else {
            ui.add_space(8.0);
            ui.label(theme::title("Partager ton écran"));
            ui.label(theme::muted("Choisis ce que tes amis vont voir, puis lance le partage."));
            ui.add_space(14.0);
            let (monitors, windows): (Vec<usize>, Vec<usize>) =
                (0..self.sources.len()).partition(|&i| matches!(self.sources[i].kind, SourceKind::Monitor(_)));
            ui.horizontal(|ui| {
                theme::section(ui, "Écrans");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if theme::button(ui, Kind::Ghost, Some(icon::REFRESH), "Actualiser", 28.0, true).clicked() {
                        let ctx = ui.ctx().clone();
                        self.refresh_sources(&ctx);
                    }
                });
            });
            self.source_grid(ui, &monitors, icon::MONITOR);
            if !windows.is_empty() {
                ui.add_space(10.0);
                theme::section(ui, "Fenêtres");
                self.source_grid(ui, &windows, icon::WINDOW);
            }
            ui.add_space(16.0);
            self.share_options(ui);
        }
        if self.starting && self.node.live.lock().unwrap().hosting.is_some() {
            self.starting = false;
        }
        ui.add_space(12.0);
        self.screenshot_card(ui);
    }

    fn source_grid(&mut self, ui: &mut egui::Ui, items: &[usize], fallback: char) {
        let card_w = 214.0;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(12.0, 12.0);
            for &i in items {
                let s = &self.sources[i];
                let selected = self.selected == i;
                let (rect, resp) = ui.allocate_exact_size(vec2(card_w, 120.0 + 40.0), Sense::click());
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                let hovered = resp.hovered();
                let p = ui.painter();
                p.rect_filled(rect, CornerRadius::same(12), if hovered || selected { theme::CARD_HOVER } else { CARD });
                let tr = egui::Rect::from_min_size(rect.min + vec2(6.0, 6.0), vec2(card_w - 12.0, 114.0));
                let tex = self.thumbs.tex.get(&source_key(&s.kind)).cloned();
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(tr));
                thumb(&mut child, tex.as_ref(), tr.size(), fallback);
                let name_rect = egui::Rect::from_min_max(tr.left_bottom() + vec2(4.0, 8.0), rect.right_bottom() - vec2(8.0, 6.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(name_rect));
                child.horizontal(|ui| {
                    ui.label(theme::icon_text(fallback, 13.0).color(if selected { theme::ACCENT_HOVER } else { FAINT }));
                    theme::truncated(ui, &s.name, theme::semibold(13.0), if selected { TEXT } else { MUTED }, card_w - 44.0);
                });
                if selected {
                    theme::outline(ui, rect, ACCENT, 2.0, 12);
                    let c = tr.right_top() + vec2(-14.0, 14.0);
                    ui.painter().circle_filled(c, 10.0, ACCENT);
                    ui.painter().text(c, Align2::CENTER_CENTER, icon::CHECK, theme::icon_font(11.0), Color32::WHITE);
                }
                if resp.on_hover_text(&s.name).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.selected = i;
                }
            }
        });
    }

    fn share_options(&mut self, ui: &mut egui::Ui) {
        let mut settings = self.node.store.lock().settings.clone();
        let before = (settings.quality, settings.audio);
        theme::card(ui, |ui| {
            setting_row(
                ui,
                "Qualité",
                match settings.quality {
                    Quality::Eco => "720p, 30 images/s. Le plus léger, parfait pour une petite connexion.",
                    Quality::Standard => "1080p, 30 images/s. Le bon choix la plupart du temps.",
                    Quality::Smooth => "1080p, 60 images/s. Pour les jeux rapides. Demande plus de débit.",
                },
                |ui| {
                    theme::segmented(
                        ui,
                        &mut settings.quality,
                        &[(Quality::Eco, "Éco"), (Quality::Standard, "Standard"), (Quality::Smooth, "Fluide")],
                    );
                },
            );
            divider(ui);
            setting_row(ui, "Son de l'ordinateur", "Tes amis entendent ce que tu entends (pas ton micro).", |ui| {
                theme::toggle(ui, &mut settings.audio);
            });
            divider(ui);
            setting_row(
                ui,
                "Qui peut regarder",
                "Les amis en « Toujours autoriser » entrent directement, les autres te demandent.",
                |ui| {
                    let mut a = self.audience.clone();
                    if self.audience_combo(ui, "audience", &mut a) {
                        self.audience = a;
                    }
                },
            );
        });
        if (settings.quality, settings.audio) != before {
            self.node.store.update(|d| d.settings = settings);
        }
    }

    /// Bottom action bar of the share page: summary + start/stop.
    fn share_bar(&mut self, ui: &mut egui::Ui) {
        let hosting = self.node.live.lock().unwrap().hosting.clone();
        egui::Panel::bottom("share_bar")
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(CARD)
                    .stroke(Stroke::new(1.0, BORDER))
                    .inner_margin(Margin::symmetric(28, 12)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some(h) = &hosting {
                        theme::badge(ui, "● EN DIRECT", LIVE, Color32::WHITE);
                        let secs = h.started.elapsed().as_secs();
                        ui.label(theme::strong(format!("{:02}:{:02}", secs / 60, secs % 60), 15.0));
                        ui.label(theme::muted(format!("· {} spectateur(s)", h.viewers.len())));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if theme::button(ui, Kind::Danger, Some(icon::STOP), "Arrêter le partage", 44.0, true).clicked() {
                                let n = self.node.clone();
                                self.rt.spawn_blocking(move || n.stop_host());
                            }
                        });
                    } else {
                        let settings = self.node.store.lock().settings.clone();
                        let src = self.sources.get(self.selected).cloned();
                        let ic = match src.as_ref().map(|s| &s.kind) {
                            Some(SourceKind::Window(_)) => icon::WINDOW,
                            _ => icon::MONITOR,
                        };
                        ui.label(theme::icon_text(ic, 20.0).color(MUTED));
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            let name = src.as_ref().map(|s| s.name.as_str()).unwrap_or("Aucune source");
                            theme::truncated(ui, name, theme::semibold(14.5), TEXT, (ui.available_width() - 260.0).max(120.0));
                            let q = match settings.quality {
                                Quality::Eco => "Éco 720p",
                                Quality::Standard => "Standard 1080p",
                                Quality::Smooth => "Fluide 1080p60",
                            };
                            let a = if settings.audio { "avec le son" } else { "sans le son" };
                            ui.label(RichText::new(format!("{q} · {a} · {}", self.audience_label(&self.audience))).size(12.5).color(MUTED));
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let label = if self.starting { "Démarrage…" } else { "Démarrer le partage" };
                            let enabled = !self.starting && src.is_some();
                            if theme::button(ui, Kind::Primary, Some(icon::PLAY), label, 44.0, enabled).clicked()
                                && let Some(src) = src
                            {
                                self.starting = true;
                                let (n, a) = (self.node.clone(), self.audience.clone());
                                self.rt.spawn_blocking(move || {
                                    if let Err(e) = n.start_host(&src, a) {
                                        tracing::error!("partage: {e:#}");
                                        n.notify_ui(UiEvent::HostFailed(format!("Impossible de partager : {e:#}")));
                                    }
                                });
                            }
                        });
                    }
                });
            });
    }

    fn screenshot_card(&mut self, ui: &mut egui::Ui) {
        let hosting = self.node.live.lock().unwrap().hosting.is_some();
        theme::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::icon_text(icon::CAMERA, 18.0).color(theme::ACCENT_HOVER));
                ui.label(theme::strong("Envoyer une capture d'écran", 16.0));
            });
            let hint = if hosting {
                "Une image HD de ce que tu partages, envoyée en un clic.".to_string()
            } else {
                match self.sources.get(self.selected) {
                    Some(s) => format!("Une image HD de « {} », envoyée en un clic.", s.name),
                    None => "Choisis d'abord un écran ou une fenêtre.".into(),
                }
            };
            ui.label(theme::muted(hint));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(theme::muted("À"));
                let mut a = self.shot_audience.clone();
                if self.audience_combo(ui, "shot_audience", &mut a) {
                    self.shot_audience = a;
                }
                if theme::button(ui, Kind::Secondary, Some(icon::SEND), "Capturer et envoyer", 32.0, true).clicked() {
                    self.capture_and_send();
                }
                if theme::button(ui, Kind::Ghost, Some(icon::FOLDER), "Mes captures", 32.0, true).clicked() {
                    self.open_screenshots();
                }
            });
        });
    }

    fn open_screenshots(&self) {
        let dir = self.node.store.lock().settings.screenshots_dir.clone();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::process::Command::new("explorer").arg(dir).spawn();
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

    // ---------------------------------------------------------------- friends & groups

    fn friends_page(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.label(theme::title("Amis & groupes"));
        ui.label(theme::muted("Choisis qui peut regarder tes partages, et range tes amis en groupes."));
        ui.add_space(14.0);
        let friends = self.node.store.lock().friends.clone();
        let peers = self.node.live.lock().unwrap().peers.clone();

        theme::card(ui, |ui| {
            ui.label(theme::strong(format!("Mes amis · {}", friends.len()), 16.0));
            ui.label(theme::muted(
                "Demander : tu acceptes à chaque fois.   Toujours : il entre direct.   Bloqué : il ne voit rien.",
            ));
            ui.add_space(6.0);
            if friends.is_empty() {
                ui.add_space(6.0);
                if theme::button(ui, Kind::Primary, Some(icon::ADD_FRIEND), "Ajouter mon premier ami", 36.0, true).clicked() {
                    self.add_code.clear();
                    self.add_error = None;
                    self.open_modal(Modal::AddFriend);
                }
            }
            for f in &friends {
                let p = peers.get(&f.id).cloned().unwrap_or_default();
                theme::row(ui, 54.0, false, |ui| {
                    let status = if p.stream.is_some() { LIVE } else if p.online { ONLINE } else { FAINT };
                    theme::avatar(ui, &f.id, &f.name, 36.0, Some(status), CARD);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        theme::truncated(ui, &f.name, theme::semibold(14.5), TEXT, 200.0);
                        let t = if p.stream.is_some() { "en direct" } else if p.online { "en ligne" } else { "hors ligne" };
                        ui.label(RichText::new(t).size(12.0).color(MUTED));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if theme::icon_button(ui, icon::DELETE, "Retirer des amis").clicked() {
                            self.open_modal(Modal::RemoveFriend { id: f.id.clone(), name: f.name.clone() });
                        }
                        let mut perm = f.permission;
                        ui.push_id(&f.id, |ui| {
                            if theme::segmented(
                                ui,
                                &mut perm,
                                &[(Permission::Ask, "Demander"), (Permission::Always, "Toujours"), (Permission::Blocked, "Bloqué")],
                            ) {
                                self.set_permission(&f.id, perm);
                            }
                        });
                    });
                });
            }
        });

        ui.add_space(12.0);
        let groups = self.node.store.lock().groups.clone();
        theme::card(ui, |ui| {
            ui.label(theme::strong(format!("Groupes · {}", groups.len()), 16.0));
            ui.label(theme::muted("Un groupe = une liste d'amis. Quand tu partages, choisis « Visible par : ce groupe »."));
            ui.add_space(6.0);
            for g in &groups {
                egui::Frame::new()
                    .fill(INPUT)
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(theme::strong(&g.name, 15.0));
                            ui.label(theme::muted(format!("· {} membre(s)", g.members.len())));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if theme::icon_button(ui, icon::DELETE, "Supprimer le groupe").clicked() {
                                    self.node.store.update(|d| d.groups.retain(|x| x.id != g.id));
                                }
                            });
                        });
                        if friends.is_empty() {
                            ui.label(theme::muted("Ajoute d'abord des amis."));
                        }
                        ui.horizontal_wrapped(|ui| {
                            for f in &friends {
                                let member = g.members.contains(&f.id);
                                if chip(ui, &f.name, member).clicked() {
                                    self.node.store.update(|d| {
                                        if let Some(gr) = d.groups.iter_mut().find(|x| x.id == g.id) {
                                            gr.members.retain(|m| m != &f.id);
                                            if !member {
                                                gr.members.push(f.id.clone());
                                            }
                                        }
                                    });
                                }
                            }
                        });
                    });
                ui.add_space(4.0);
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.new_group)
                        .hint_text("Nom du nouveau groupe (ex. « Potes coop »)")
                        .desired_width(300.0)
                        .margin(Margin::symmetric(10, 7)),
                );
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let ok = !self.new_group.trim().is_empty();
                if (theme::button(ui, Kind::Primary, Some(icon::PEOPLE), "Créer le groupe", 32.0, ok).clicked() || enter) && ok {
                    let name = self.new_group.trim().to_string();
                    self.node.store.update(|d| {
                        let id = d.groups.iter().map(|g| g.id).max().unwrap_or(0) + 1;
                        d.groups.push(crate::store::Group { id, name, members: vec![] });
                    });
                    self.new_group.clear();
                }
            });
        });
    }

    // ---------------------------------------------------------------- settings

    fn settings_page(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.label(theme::title("Paramètres"));
        ui.add_space(14.0);
        theme::card(ui, |ui| {
            ui.label(theme::strong("Profil", 16.0));
            ui.add_space(4.0);
            setting_row(ui, "Pseudo", "C'est le nom que voient tes amis.", |ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.name_edit).desired_width(220.0).margin(Margin::symmetric(10, 7)));
                if r.lost_focus() {
                    let n = self.name_edit.trim().chars().take(32).collect::<String>();
                    if !n.is_empty() {
                        self.node.store.update(|d| d.name = n);
                    }
                }
            });
            divider(ui);
            let code = friend_code(&self.node.id());
            setting_row(ui, "Mon code ami", "Donne-le à tes amis pour qu'ils t'ajoutent. Il ne change jamais.", |ui| {
                if theme::button(ui, Kind::Secondary, Some(icon::COPY), "Copier", 32.0, true).clicked() {
                    ui.ctx().copy_text(code.clone());
                    self.toast("Code copié !");
                }
                ui.label(RichText::new(format!("{}…", &code[..16.min(code.len())])).monospace().color(MUTED));
            });
        });
        ui.add_space(12.0);
        theme::card(ui, |ui| {
            ui.label(theme::strong("Son & captures", 16.0));
            ui.add_space(4.0);
            setting_row(ui, "Volume des streams", "Le volume du son des amis que tu regardes.", |ui| {
                let mut vol = self.node.volume.load(Ordering::Relaxed);
                ui.spacing_mut().slider_width = 180.0;
                if ui.add(egui::Slider::new(&mut vol, 0..=150).suffix(" %")).changed() {
                    self.node.volume.store(vol, Ordering::Relaxed);
                    self.node.store.update(|d| d.settings.volume = vol);
                }
            });
            divider(ui);
            let dir = self.node.store.lock().settings.screenshots_dir.display().to_string();
            setting_row(ui, "Dossier des captures", &dir, |ui| {
                if theme::button(ui, Kind::Secondary, Some(icon::FOLDER), "Ouvrir", 32.0, true).clicked() {
                    self.open_screenshots();
                }
            });
        });
        ui.add_space(12.0);
        theme::card(ui, |ui| {
            ui.label(theme::strong("À propos", 16.0));
            ui.add_space(4.0);
            setting_row(
                ui,
                &format!("MyStream {}", env!("CARGO_PKG_VERSION")),
                "Libre et open source (licence MIT). Les mises à jour sont vérifiées à chaque démarrage.",
                |ui| {
                    let checking = matches!(*self.update.lock().unwrap(), UpdateState::Checking);
                    let label = if checking { "Recherche…" } else { "Rechercher une mise à jour" };
                    if theme::button(ui, Kind::Secondary, Some(icon::REFRESH), label, 32.0, !checking).clicked() {
                        spawn_update_check(&self.rt, self.update.clone(), ui.ctx().clone(), true);
                    }
                    ui.hyperlink_to("GitHub", "https://github.com/williamloree/mystream");
                },
            );
        });
    }

    // ================================================================ watch view

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

    fn watch_view(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
        let (watching, connecting) = {
            let l = self.node.live.lock().unwrap();
            (l.watching.clone(), l.connecting_to.clone())
        };
        if let Some(t) = &self.texture {
            let [tw, th] = t.size();
            let scale = (rect.width() / tw as f32).min(rect.height() / th as f32);
            let r = egui::Rect::from_center_size(rect.center(), vec2(tw as f32 * scale, th as f32 * scale));
            ui.painter().image(t.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        let resp = ui.allocate_rect(rect, Sense::click());
        if resp.double_clicked() {
            let f = !self.fullscreen;
            self.set_fullscreen(&ctx, f);
        }
        let hover = ctx.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p));
        if hover.is_some() && hover != self.last_pointer_pos {
            self.last_pointer_move = Instant::now();
        }
        self.last_pointer_pos = hover;

        // Waiting for the host to accept / for the first image.
        if watching.is_none() || self.texture.is_none() {
            let who = watching.as_ref().map(|w| w.friend.clone()).or(connecting.clone()).unwrap_or_default();
            let name = self.node.store.lock().display_name(&who);
            egui::Area::new(Id::unique("connecting"))
                .pivot(Align2::CENTER_CENTER)
                .fixed_pos(rect.center())
                .show(&ctx, |ui| {
                    egui::Frame::new()
                        .fill(ELEVATED)
                        .corner_radius(CornerRadius::same(16))
                        .inner_margin(Margin::same(28))
                        .show(ui, |ui| {
                            ui.set_width(320.0);
                            ui.vertical_centered(|ui| {
                                theme::avatar(ui, &who, &name, 64.0, None, ELEVATED);
                                ui.add_space(8.0);
                                if watching.is_some() {
                                    ui.label(theme::strong(format!("Connexion au partage de {name}…"), 16.0));
                                    ui.label(theme::muted("L'image arrive."));
                                } else {
                                    ui.label(theme::strong(format!("En attente de {name}"), 16.0));
                                    ui.label(theme::muted("Ton ami doit accepter que tu regardes."));
                                }
                                ui.add_space(8.0);
                                ui.spinner();
                                ui.add_space(8.0);
                                if theme::button(ui, Kind::Secondary, None, "Annuler", 34.0, true).clicked() {
                                    self.node.stop_watching();
                                }
                            });
                        });
                });
            return;
        }
        let Some(w) = watching else { return };

        let show = self.last_pointer_move.elapsed() < Duration::from_millis(2500);
        if show {
            ctx.request_repaint_after(Duration::from_millis(2600));
        }
        let t = ctx.animate_bool_with_time(Id::unique("watch_overlay"), show, 0.2);
        if t <= 0.0 {
            return;
        }
        let name = self.node.store.lock().display_name(&w.friend);
        let pill = |ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui)| {
            ui.multiply_opacity(t);
            egui::Frame::new()
                .fill(Color32::from_black_alpha(200))
                .stroke(Stroke::new(1.0, Color32::from_white_alpha(18)))
                .corner_radius(CornerRadius::same(14))
                .inner_margin(Margin::symmetric(12, 8))
                .show(ui, |ui| ui.horizontal(|ui| add(ui)));
        };
        egui::Area::new(Id::unique("watch_info"))
            .fixed_pos(rect.min + vec2(16.0, 16.0))
            .show(&ctx, |ui| {
                pill(ui, &mut |ui| {
                    theme::avatar(ui, &w.friend, &name, 28.0, None, Color32::BLACK);
                    ui.label(theme::strong(&name, 14.0));
                    theme::badge(ui, "EN DIRECT", LIVE, Color32::WHITE);
                    ui.label(RichText::new(format!("{} · {:.0} i/s", w.info.title, self.fps.2)).size(12.5).color(MUTED));
                });
            });
        egui::Area::new(Id::unique("watch_controls"))
            .pivot(Align2::CENTER_BOTTOM)
            .fixed_pos(rect.center_bottom() - vec2(0.0, 22.0))
            .show(&ctx, |ui| {
                pill(ui, &mut |ui| {
                    if w.info.audio {
                        let mut vol = self.node.volume.load(Ordering::Relaxed);
                        let ic = if vol == 0 { icon::MUTE } else { icon::VOLUME };
                        if theme::icon_button_sized(ui, ic, "Couper / remettre le son", 36.0, TEXT).clicked() {
                            vol = if vol == 0 { 100 } else { 0 };
                            self.set_volume(vol);
                        }
                        ui.spacing_mut().slider_width = 110.0;
                        if ui.add(egui::Slider::new(&mut vol, 0..=150).show_value(false)).changed() {
                            self.set_volume(vol);
                        }
                        ui.add_space(6.0);
                    }
                    if theme::icon_button_sized(ui, icon::CAMERA, "Enregistrer l'image", 36.0, TEXT).clicked() {
                        self.save_viewer_capture();
                    }
                    let (fi, ft) = if self.fullscreen {
                        (icon::EXIT_FULLSCREEN, "Quitter le plein écran (Échap)")
                    } else {
                        (icon::FULLSCREEN, "Plein écran (double-clic)")
                    };
                    if theme::icon_button_sized(ui, fi, ft, 36.0, TEXT).clicked() {
                        let f = !self.fullscreen;
                        self.set_fullscreen(ui.ctx(), f);
                    }
                    ui.add_space(6.0);
                    if theme::button(ui, Kind::Live, Some(icon::CLOSE), "Quitter", 34.0, true).clicked() {
                        self.node.stop_watching();
                        self.set_fullscreen(ui.ctx(), false);
                    }
                });
            });
    }

    fn set_volume(&self, vol: u32) {
        self.node.volume.store(vol, Ordering::Relaxed);
        self.node.store.update(|d| d.settings.volume = vol);
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

    // ================================================================ overlays

    fn modal_frame() -> egui::Frame {
        egui::Frame::new()
            .fill(ELEVATED)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(16))
            .inner_margin(Margin::same(24))
    }

    fn watch_requests(&mut self, ctx: &egui::Context) {
        let Some(r) = self.requests.first() else { return };
        let (name, friend) = (r.name.clone(), r.friend.clone());
        let mut answer = None;
        egui::Modal::new(Id::unique(("watchreq", r.req_id)))
            .frame(Self::modal_frame())
            .show(ctx, |ui| {
                ui.set_width(400.0);
                ui.vertical_centered(|ui| {
                    theme::avatar(ui, &friend, &name, 64.0, None, ELEVATED);
                    ui.add_space(8.0);
                    ui.label(theme::strong(format!("{name} veut regarder ton écran"), 18.0));
                    ui.label(theme::muted("Tu peux aussi lui faire confiance pour la suite."));
                });
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 16.0) / 3.0;
                    if theme::button_w(ui, Kind::Ghost, None, "Refuser", 38.0, true, Some(w)).clicked() {
                        answer = Some((false, false));
                    }
                    if theme::button_w(ui, Kind::Secondary, None, "Toujours", 38.0, true, Some(w))
                        .on_hover_text("Accepter, et ne plus demander pour cet ami")
                        .clicked()
                    {
                        answer = Some((true, true));
                    }
                    if theme::button_w(ui, Kind::Primary, Some(icon::CHECK), "Accepter", 38.0, true, Some(w)).clicked() {
                        answer = Some((true, false));
                    }
                });
            });
        if let Some((accept, always)) = answer {
            let r = self.requests.remove(0);
            if always {
                self.set_permission(&r.friend, Permission::Always);
            }
            self.node.answer_watch(r.req_id, accept);
        }
    }

    fn modals(&mut self, ctx: &egui::Context) {
        let Some(modal) = &self.modal else { return };
        let first = std::mem::take(&mut self.modal_opened);
        let mut close = false;
        match modal {
            Modal::Onboarding => {
                egui::Modal::new(Id::unique("onboarding")).frame(Self::modal_frame()).show(ctx, |ui| {
                    ui.set_width(440.0);
                    ui.vertical_centered(|ui| {
                        if let Some(l) = &self.logo {
                            ui.image((l.id(), vec2(72.0, 72.0)));
                        }
                        ui.add_space(6.0);
                        ui.label(theme::strong("Bienvenue sur MyStream", 22.0));
                        ui.label(theme::muted("Partage ton écran avec tes potes, sans faire chauffer ton PC."));
                    });
                    ui.add_space(16.0);
                    ui.label(theme::strong("Ton pseudo", 14.0));
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.name_edit)
                            .desired_width(f32::INFINITY)
                            .margin(Margin::symmetric(10, 8))
                            .hint_text("Comment tes amis te voient"),
                    );
                    if first {
                        r.request_focus();
                    }
                    ui.add_space(12.0);
                    for (n, t, d) in [
                        ("1", "Copie ton code ami", "Bouton « Copier mon code ami », en haut à gauche."),
                        ("2", "Envoie-le à tes amis", "Sur Discord, WhatsApp… Ils t'ajoutent, tu acceptes."),
                        ("3", "Partage ou regarde", "Choisis un écran, lance le partage. Tes amis voient « Regarder »."),
                    ] {
                        ui.horizontal(|ui| {
                            let (rect, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
                            ui.painter().circle_filled(rect.center(), 14.0, ACCENT_SOFT);
                            ui.painter().text(rect.center(), Align2::CENTER_CENTER, n, theme::semibold(13.0), TEXT);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                ui.label(theme::strong(t, 14.0));
                                ui.label(RichText::new(d).size(12.5).color(MUTED));
                            });
                        });
                        ui.add_space(4.0);
                    }
                    ui.add_space(12.0);
                    let ok = !self.name_edit.trim().is_empty();
                    let w = ui.available_width();
                    if theme::button_w(ui, Kind::Primary, None, "C'est parti", 44.0, ok, Some(w)).clicked() && ok {
                        let n = self.name_edit.trim().chars().take(32).collect::<String>();
                        self.name_edit = n.clone();
                        self.node.store.update(|d| {
                            d.name = n;
                            d.onboarded = true;
                        });
                        close = true;
                    }
                });
            }
            Modal::AddFriend => {
                let resp = egui::Modal::new(Id::unique("add_friend")).frame(Self::modal_frame()).show(ctx, |ui| {
                    ui.set_width(440.0);
                    ui.horizontal(|ui| {
                        ui.label(theme::icon_text(icon::ADD_FRIEND, 20.0).color(theme::ACCENT_HOVER));
                        ui.label(theme::strong("Ajouter un ami", 19.0));
                    });
                    ui.label(theme::muted(
                        "Demande-lui son code ami : il le copie avec le bouton « Copier mon code ami », en haut à gauche de son MyStream.",
                    ));
                    ui.add_space(10.0);
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.add_code)
                            .hint_text("Colle son code ici (ms-…)")
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY)
                            .margin(Margin::symmetric(10, 9)),
                    );
                    if first {
                        r.request_focus();
                    }
                    if r.changed() {
                        self.add_error = None;
                    }
                    if let Some(e) = &self.add_error {
                        ui.label(RichText::new(e).color(LIVE).size(12.5));
                    }
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add_space(12.0);
                    let mut send = enter;
                    ui.horizontal(|ui| {
                        if theme::button(ui, Kind::Ghost, Some(icon::COPY), "Copier mon code", 36.0, true)
                            .on_hover_text("Pour qu'il puisse t'ajouter lui aussi")
                            .clicked()
                        {
                            ui.ctx().copy_text(friend_code(&self.node.id()));
                            self.toast("Ton code est copié !");
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if theme::button(ui, Kind::Primary, Some(icon::SEND), "Envoyer la demande", 36.0, !self.add_code.trim().is_empty()).clicked() {
                                send = true;
                            }
                            if theme::button(ui, Kind::Ghost, None, "Annuler", 36.0, true).clicked() {
                                close = true;
                            }
                        });
                    });
                    if send {
                        match parse_friend_code(self.add_code.trim()) {
                            Ok(id) if id == self.node.id() => self.add_error = Some("C'est ton propre code 🙂".into()),
                            Ok(id) => {
                                self.rt.spawn(self.node.clone().send_friend_request(id));
                                self.toast("Demande envoyée ! Elle sera acceptée dès que ton ami ouvrira MyStream.");
                                self.add_code.clear();
                                close = true;
                            }
                            Err(_) => self.add_error = Some("Ce code n'est pas valide. Vérifie qu'il est complet (il commence par « ms- »).".into()),
                        }
                    }
                });
                if resp.should_close() {
                    close = true;
                }
            }
            Modal::RemoveFriend { id, name } => {
                let (id, name) = (id.clone(), name.clone());
                let resp = egui::Modal::new(Id::unique("remove_friend")).frame(Self::modal_frame()).show(ctx, |ui| {
                    ui.set_width(380.0);
                    ui.label(theme::strong(format!("Retirer {name} ?"), 18.0));
                    ui.label(theme::muted(
                        "Vous ne pourrez plus vous voir en ligne ni partager vos écrans. Tu pourras le rajouter plus tard avec son code.",
                    ));
                    ui.add_space(14.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if theme::button(ui, Kind::Danger, Some(icon::DELETE), "Retirer", 36.0, true).clicked() {
                            self.node.store.update(|d| d.remove_friend(&id));
                            close = true;
                        }
                        if theme::button(ui, Kind::Ghost, None, "Annuler", 36.0, true).clicked() {
                            close = true;
                        }
                    });
                });
                if resp.should_close() {
                    close = true;
                }
            }
        }
        if close {
            self.modal = None;
        }
    }

    fn toasts(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|t| t.at.elapsed() < Duration::from_secs(if t.path.is_some() { 12 } else { 5 }));
        if self.toasts.is_empty() {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        egui::Area::new(Id::unique("toasts"))
            .order(egui::Order::Tooltip)
            .anchor(Align2::RIGHT_BOTTOM, vec2(-18.0, -18.0))
            .show(ctx, |ui| {
                for t in &self.toasts {
                    egui::Frame::new()
                        .fill(ELEVATED)
                        .stroke(Stroke::new(1.0, BORDER))
                        .corner_radius(CornerRadius::same(12))
                        .shadow(ui.style().visuals.popup_shadow)
                        .inner_margin(Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.set_max_width(340.0);
                            ui.horizontal(|ui| {
                                let ic = if t.path.is_some() { icon::CAMERA } else { icon::INFO };
                                ui.label(theme::icon_text(ic, 16.0).color(theme::ACCENT_HOVER));
                                ui.add(egui::Label::new(RichText::new(&t.text).color(TEXT)).wrap());
                                if let Some(p) = &t.path
                                    && theme::button(ui, Kind::Secondary, None, "Ouvrir", 28.0, true).clicked()
                                {
                                    let _ = std::process::Command::new("explorer").arg(p).spawn();
                                }
                            });
                        });
                    ui.add_space(6.0);
                }
            });
    }

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
            egui::Panel::left("sidebar")
                .exact_size(290.0)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::new().fill(SIDEBAR))
                .show(ui, |ui| self.sidebar(ui));
        }
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::new().fill(if in_watch { Color32::BLACK } else { BG }))
            .show(ui, |ui| {
                if in_watch {
                    self.watch_view(ui);
                } else {
                    self.main_view(ui);
                }
            });
        self.watch_requests(&ctx);
        self.modals(&ctx);
        self.toasts(&ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.node.stop_watching();
        self.node.stop_host();
    }
}

// ================================================================ helpers

/// Label + description on the left, control on the right.
fn setting_row(ui: &mut egui::Ui, title: &str, desc: &str, right: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        let w = (ui.available_width() * 0.5).max(220.0);
        ui.allocate_ui_with_layout(vec2(w, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(theme::strong(title, 14.5));
            ui.add(egui::Label::new(RichText::new(desc).size(12.5).color(MUTED)).wrap());
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| right(ui));
    });
}

fn divider(ui: &mut egui::Ui) {
    ui.add_space(6.0);
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, BORDER));
    ui.add_space(8.0);
}

/// Toggleable pill (group membership).
fn chip(ui: &mut egui::Ui, text: &str, on: bool) -> egui::Response {
    let font = theme::semibold(13.0);
    let g = ui.painter().layout_no_wrap(text.to_string(), font, TEXT);
    let icon_w = if on { 18.0 } else { 0.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 24.0 + icon_w, 30.0), Sense::click());
    let (fill, stroke, fg) = if on {
        (ACCENT_SOFT, Stroke::new(1.0, ACCENT), TEXT)
    } else if resp.hovered() {
        (theme::ELEVATED_HOVER, Stroke::NONE, TEXT)
    } else {
        (ELEVATED, Stroke::NONE, MUTED)
    };
    ui.painter().rect(rect, CornerRadius::same(15), fill, stroke, egui::StrokeKind::Inside);
    let mut x = rect.left() + 12.0;
    if on {
        ui.painter().text(egui::pos2(x + 6.0, rect.center().y), Align2::CENTER_CENTER, icon::CHECK, theme::icon_font(11.0), theme::ACCENT_HOVER);
        x += icon_w;
    }
    ui.painter().galley(egui::pos2(x, rect.center().y - g.size().y / 2.0), g, fg);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Draws a preview (letterboxed) or a placeholder icon.
fn thumb(ui: &mut egui::Ui, tex: Option<&egui::TextureHandle>, size: egui::Vec2, fallback: char) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(8), INPUT);
    match tex {
        Some(t) => {
            let [tw, th] = t.size();
            let scale = (rect.width() / tw as f32).min(rect.height() / th as f32);
            let r = egui::Rect::from_center_size(rect.center(), vec2(tw as f32 * scale, th as f32 * scale));
            p.image(t.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        None => {
            p.text(rect.center(), Align2::CENTER_CENTER, fallback, theme::icon_font(30.0), FAINT);
        }
    }
}

fn spawn_update_check(
    rt: &tokio::runtime::Handle,
    state: Arc<std::sync::Mutex<UpdateState>>,
    ctx: egui::Context,
    manual: bool,
) {
    if manual {
        *state.lock().unwrap() = UpdateState::Checking;
    }
    rt.spawn(async move {
        let next = match update::check().await {
            Ok(Some(r)) => UpdateState::Available(r),
            Ok(None) if manual => UpdateState::Failed("aucune — tu as déjà la dernière version 👍".into()),
            Ok(None) => UpdateState::None,
            Err(e) if manual => UpdateState::Failed(format!("vérification impossible ({e})")),
            Err(e) => {
                tracing::info!("vérification de mise à jour impossible : {e}");
                UpdateState::None
            }
        };
        *state.lock().unwrap() = next;
        ctx.request_repaint();
    });
}

/// Captures every source once, at most 4 at a time, as small previews.
fn spawn_thumbnails(sources: Vec<Source>, tx: std::sync::mpsc::Sender<(String, egui::ColorImage)>, ctx: egui::Context) {
    let queue = Arc::new(std::sync::Mutex::new(sources));
    for _ in 0..4 {
        let (q, tx, ctx) = (queue.clone(), tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            crate::media::encoder::com_init();
            loop {
                let Some(s) = q.lock().unwrap().pop() else { break };
                let Ok((w, h, rgba)) = grab_once(&s.kind) else { continue };
                let Some(img) = image::RgbaImage::from_raw(w, h, rgba) else { continue };
                let tw = 400u32.min(w);
                let t = image::imageops::thumbnail(&img, tw, (tw * h / w.max(1)).max(1));
                let ci = egui::ColorImage::from_rgba_unmultiplied([t.width() as usize, t.height() as usize], t.as_raw());
                if tx.send((source_key(&s.kind), ci)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
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
