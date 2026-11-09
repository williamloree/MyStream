//! Servers (groups of friends) and voice channels: the server rail, the
//! channel list, the voice "stage" and the voice control panel.

use std::sync::atomic::Ordering;

use eframe::egui::{self, Align, Align2, Color32, CornerRadius, Layout, Margin, RichText, Sense, Stroke, vec2};

use super::theme::{self, ACCENT, ACCENT_HOVER, BORDER, CARD, ELEVATED, FAINT, Kind, LIVE, MUTED, ONLINE, SIDEBAR, TEXT, icon};
use super::{App, Modal, Page, View};
use crate::net::{Audience, VoiceState};
use crate::store::{Server, parse_friend_code};

pub const RAIL: Color32 = Color32::from_rgb(0x0B, 0x0C, 0x0E);

/// One person in a voice channel, as displayed.
struct Participant {
    id: String,
    name: String,
    speaking: bool,
    muted: bool,
    deafened: bool,
    live: bool,
    me: bool,
}

impl App {
    fn participants(&self, server: &str, channel: u64) -> Vec<Participant> {
        let me = self.node.id().to_z32();
        let mine = self.node.live.lock().unwrap().voice.clone();
        let shared = &self.node.voice_shared;
        let mut v = vec![];
        if let Some(m) = mine.filter(|m| m.is_in(server, channel)) {
            let hosting = self.node.live.lock().unwrap().hosting.is_some();
            v.push(Participant {
                id: me.clone(),
                name: self.my_name(),
                speaking: shared.speaking.load(Ordering::Relaxed),
                muted: m.muted,
                deafened: m.deafened,
                live: hosting,
                me: true,
            });
        }
        let peers = self.node.live.lock().unwrap().peers.clone();
        let d = self.node.store.lock();
        let mut others: Vec<Participant> = peers
            .iter()
            .filter_map(|(id, p)| {
                let vs = p.voice.as_ref().filter(|vs| p.online && vs.is_in(server, channel))?;
                Some(Participant {
                    id: id.clone(),
                    name: d.display_name(id),
                    speaking: shared.is_speaking(id),
                    muted: vs.muted,
                    deafened: vs.deafened,
                    live: p.stream.is_some(),
                    me: false,
                })
            })
            .collect();
        others.sort_by_key(|p| p.name.to_lowercase());
        v.extend(others);
        v
    }

    pub(super) fn join_voice(&self, server: &str, channel: u64) {
        let (n, s) = (self.node.clone(), server.to_string());
        self.rt.spawn_blocking(move || n.join_voice(&s, channel));
    }

    pub(super) fn leave_voice(&self) {
        let n = self.node.clone();
        self.rt.spawn_blocking(move || n.leave_voice());
    }

    // ================================================================ rail

    pub(super) fn rail(&mut self, ui: &mut egui::Ui) {
        let servers = self.node.store.lock().servers.clone();
        let invites = self.node.store.lock().server_invites.len();
        let voice = self.node.live.lock().unwrap().voice.clone();
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            let home = self.view == View::Home;
            let r = rail_item(ui, home, |p, rect| {
                if let Some(l) = &self.logo {
                    p.image(l.id(), rect.shrink(6.0), egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                } else {
                    p.text(rect.center(), Align2::CENTER_CENTER, icon::HOME, theme::icon_font(20.0), TEXT);
                }
            });
            if invites > 0 {
                let c = r.rect.right_bottom() - vec2(6.0, 6.0);
                ui.painter().circle_filled(c, 9.0, LIVE);
                ui.painter().text(c, Align2::CENTER_CENTER, invites.to_string(), theme::semibold(11.0), Color32::WHITE);
            }
            if r.on_hover_text("Accueil : amis et partage d'écran").clicked() {
                self.view = View::Home;
            }
            let (sep, _) = ui.allocate_exact_size(vec2(32.0, 2.0), Sense::hover());
            ui.painter().rect_filled(sep, CornerRadius::same(1), theme::BORDER);
            for s in &servers {
                let selected = self.view == View::Server(s.id.clone());
                let in_voice = voice.as_ref().is_some_and(|v| v.server == s.id);
                let r = rail_item(ui, selected, |p, rect| {
                    let fill = if selected { ACCENT } else { theme::avatar_color(&s.id).gamma_multiply(0.85) };
                    p.rect_filled(rect, CornerRadius::same(if selected { 14 } else { 22 }), fill);
                    p.text(rect.center(), Align2::CENTER_CENTER, theme::initials(&s.admin.name), theme::semibold(16.0), Color32::WHITE);
                });
                if in_voice {
                    let c = r.rect.right_bottom() - vec2(5.0, 5.0);
                    ui.painter().circle_filled(c, 8.0, RAIL);
                    ui.painter().text(c, Align2::CENTER_CENTER, icon::SPEAKER, theme::icon_font(9.0), ONLINE);
                }
                if r.on_hover_text(&s.admin.name).clicked() {
                    self.view = View::Server(s.id.clone());
                }
            }
            let r = rail_item(ui, false, |p, rect| {
                p.rect_filled(rect, CornerRadius::same(22), theme::CARD);
                p.text(rect.center(), Align2::CENTER_CENTER, icon::ADD, theme::icon_font(16.0), ONLINE);
            });
            if r.on_hover_text("Créer un serveur").clicked() {
                self.new_server_name.clear();
                self.open_modal(Modal::CreateServer);
            }
        });
    }

    // ================================================================ voice panel

    /// Shown at the bottom of the sidebar while connected to a voice channel.
    pub(super) fn voice_panel(&mut self, ui: &mut egui::Ui) {
        let Some(v) = self.node.live.lock().unwrap().voice.clone() else { return };
        let (server, channel) = {
            let d = self.node.store.lock();
            let s = d.server(&v.server);
            (
                s.map(|s| s.admin.name.clone()).unwrap_or_default(),
                s.and_then(|s| s.admin.channels.iter().find(|c| c.id == v.channel)).map(|c| c.name.clone()).unwrap_or_default(),
            )
        };
        egui::Frame::new()
            .fill(CARD)
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new("Vocal connecté").font(theme::semibold(13.5)).color(ONLINE));
                        let r = theme::truncated(ui, &format!("{channel} / {server}"), egui::FontId::proportional(12.0), MUTED, 150.0);
                        if r.interact(Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            self.view = View::Server(v.server.clone());
                        }
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if theme::icon_button_sized(ui, icon::HANGUP, "Quitter le vocal", 34.0, LIVE).clicked() {
                            self.leave_voice();
                        }
                    });
                });
                ui.add_space(2.0);
                self.voice_buttons(ui, &v, 36.0);
            });
        ui.add_space(8.0);
    }

    fn voice_buttons(&mut self, ui: &mut egui::Ui, v: &VoiceState, size: f32) {
        ui.horizontal(|ui| {
            let ptt = self.node.voice_shared.push_to_talk.load(Ordering::Relaxed);
            let mic_tip = if v.muted {
                "Réactiver le micro".to_string()
            } else if ptt {
                format!("Couper le micro (appuie sur {} pour parler)", crate::media::voice::key_name(self.node.voice_shared.ptt_key.load(Ordering::Relaxed)))
            } else {
                "Couper le micro".into()
            };
            if theme::toggle_icon(ui, icon::MIC, v.muted, &mic_tip, size).clicked() {
                // Unmuting also undeafens, like Discord.
                let muted = !v.muted;
                self.node.set_voice_flags(muted, v.deafened && muted);
            }
            let tip = if v.deafened { "Réactiver le son" } else { "Couper le son (et le micro)" };
            if theme::toggle_icon(ui, icon::HEADPHONE, v.deafened, tip, size).clicked() {
                let deaf = !v.deafened;
                self.node.set_voice_flags(deaf || (v.muted && !v.deafened), deaf);
            }
            if theme::icon_button_sized(ui, icon::SETTINGS, "Réglages de la voix", size, MUTED).clicked() {
                self.view = View::Home;
                self.page = Page::Settings;
            }
        });
    }

    // ================================================================ server sidebar

    pub(super) fn server_sidebar(&mut self, ui: &mut egui::Ui, server: &Server) {
        let me = self.node.id().to_z32();
        let owner = server.owner == me;
        egui::Frame::new().inner_margin(Margin { left: 16, right: 10, top: 14, bottom: 10 }).show(ui, |ui| {
            ui.horizontal(|ui| {
                theme::truncated(ui, &server.admin.name, theme::semibold(17.0), TEXT, 190.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let r = theme::icon_button(ui, icon::MORE, "Options du serveur");
                    egui::Popup::menu(&r).show(|ui| {
                        ui.set_min_width(200.0);
                        if ui.button("Inviter des amis").clicked() {
                            self.open_modal(Modal::Invite { sid: server.id.clone() });
                        }
                        if owner && ui.button("Paramètres du serveur").clicked() {
                            self.rename_edit = server.admin.name.clone();
                            self.new_channel.clear();
                            self.open_modal(Modal::ServerSettings { sid: server.id.clone() });
                        }
                        ui.separator();
                        let label = if owner { "Supprimer le serveur" } else { "Quitter le serveur" };
                        if ui.button(RichText::new(label).color(LIVE)).clicked() {
                            self.open_modal(Modal::LeaveServer { sid: server.id.clone() });
                        }
                    });
                });
            });
        });
        let r = ui.available_rect_before_wrap();
        ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, BORDER));

        egui::Panel::bottom("server_sidebar_bottom")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(Margin::same(12)))
            .show(ui, |ui| {
                self.voice_panel(ui);
                let w = ui.available_width();
                if theme::button_w(ui, Kind::Primary, Some(icon::ADD_FRIEND), "Inviter des amis", 38.0, true, Some(w)).clicked() {
                    self.open_modal(Modal::Invite { sid: server.id.clone() });
                }
            });

        let my_voice = self.node.live.lock().unwrap().voice.clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::new().inner_margin(Margin { left: 10, right: 10, top: 12, bottom: 8 }).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    theme::section(ui, "Salons vocaux");
                });
                for c in &server.admin.channels {
                    let here = my_voice.as_ref().is_some_and(|v| v.is_in(&server.id, c.id));
                    let (resp, _) = theme::row(ui, 36.0, here, |ui| {
                        ui.label(theme::icon_text(icon::SPEAKER, 15.0).color(if here { ONLINE } else { MUTED }));
                        theme::truncated(ui, &c.name, theme::semibold(14.5), if here { TEXT } else { MUTED }, 190.0);
                    });
                    if resp.on_hover_text(if here { "Tu es dans ce salon" } else { "Clique pour rejoindre" }).clicked() && !here {
                        self.join_voice(&server.id, c.id);
                        self.view = View::Server(server.id.clone());
                    }
                    for p in self.participants(&server.id, c.id) {
                        let (resp, _) = theme::row(ui, 32.0, false, |ui| {
                            ui.add_space(18.0);
                            theme::voice_avatar(ui, &p.id, &p.name, 26.0, p.speaking, SIDEBAR);
                            let w = 120.0 - if p.live { 34.0 } else { 0.0 };
                            theme::truncated(ui, &p.name, egui::FontId::proportional(14.0), if p.speaking { TEXT } else { MUTED }, w);
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.spacing_mut().item_spacing.x = 2.0;
                                if p.deafened {
                                    theme::status_icon(ui, icon::HEADPHONE, 16.0);
                                }
                                if p.muted {
                                    theme::status_icon(ui, icon::MIC, 16.0);
                                }
                                if p.live {
                                    theme::badge(ui, "LIVE", LIVE, Color32::WHITE);
                                }
                            });
                        });
                        if !p.me {
                            self.member_menu(&resp, &p.id, &p.name, server, p.live);
                        }
                    }
                }
            });
        });
    }

    /// Right-click menu on someone: volume, watch, kick (creator).
    fn member_menu(&mut self, resp: &egui::Response, id: &str, name: &str, server: &Server, live: bool) {
        let me = self.node.id().to_z32();
        resp.context_menu(|ui| {
            ui.set_min_width(220.0);
            ui.label(theme::strong(name, 14.0));
            if live && ui.button("Regarder son écran").clicked()
                && let Ok(eid) = parse_friend_code(id)
            {
                self.rt.spawn(self.node.clone().watch(eid));
                ui.close();
            }
            ui.label(RichText::new("Volume pour moi").size(12.0).color(MUTED));
            let mut vol = self.node.voice_shared.gains.lock().unwrap().get(id).copied().unwrap_or(100);
            ui.spacing_mut().slider_width = 160.0;
            if ui.add(egui::Slider::new(&mut vol, 0..=200).suffix(" %")).changed() {
                self.set_gain(id, vol);
            }
            let muted = vol == 0;
            if ui.button(if muted { "Réactiver pour moi" } else { "Rendre muet pour moi" }).clicked() {
                self.set_gain(id, if muted { 100 } else { 0 });
                ui.close();
            }
            if server.owner == me && id != me {
                ui.separator();
                if ui.button(RichText::new("Retirer du serveur").color(LIVE)).clicked() {
                    let target = id.to_string();
                    self.node.admin_update(&server.id, |a| {
                        if !a.banned.contains(&target) {
                            a.banned.push(target.clone());
                        }
                    });
                    ui.close();
                }
            }
        });
    }

    fn set_gain(&self, id: &str, vol: u32) {
        self.node.voice_shared.gains.lock().unwrap().insert(id.to_string(), vol);
        self.node.store.update(|d| {
            d.settings.voice.gains.insert(id.to_string(), vol);
        });
    }

    // ================================================================ server main view

    pub(super) fn server_view(&mut self, ui: &mut egui::Ui, server: &Server) {
        let me = self.node.id().to_z32();
        let my_voice = self.node.live.lock().unwrap().voice.clone().filter(|v| v.server == server.id);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::new().inner_margin(Margin { left: 28, right: 28, top: 20, bottom: 28 }).show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(980.0));
                self.update_banner(ui);
                ui.add_space(4.0);
                match &my_voice {
                    Some(v) => self.stage(ui, server, v),
                    None => self.channel_cards(ui, server),
                }
                ui.add_space(16.0);
                self.member_list(ui, server, &me);
            });
        });
    }

    fn channel_cards(&mut self, ui: &mut egui::Ui, server: &Server) {
        ui.label(theme::title(&server.admin.name));
        ui.label(theme::muted("Rejoins un salon pour parler avec tes potes. Personne n'enregistre rien : la voix va directement de PC à PC."));
        ui.add_space(14.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(12.0, 12.0);
            for c in &server.admin.channels {
                let people = self.participants(&server.id, c.id);
                let (rect, resp) = ui.allocate_exact_size(vec2(300.0, 132.0), Sense::click());
                let hovered = resp.hovered();
                ui.painter().rect(
                    rect,
                    CornerRadius::same(14),
                    if hovered { theme::CARD_HOVER } else { CARD },
                    Stroke::new(1.0, if hovered { ACCENT } else { BORDER }),
                    egui::StrokeKind::Inside,
                );
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(16.0)).layout(Layout::top_down(Align::Min)));
                child.horizontal(|ui| {
                    ui.label(theme::icon_text(icon::SPEAKER, 18.0).color(ACCENT_HOVER));
                    ui.label(theme::strong(&c.name, 16.0));
                });
                child.add_space(4.0);
                if people.is_empty() {
                    child.label(theme::muted("Personne pour l'instant"));
                } else {
                    child.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = -6.0;
                        for p in people.iter().take(6) {
                            theme::voice_avatar(ui, &p.id, &p.name, 34.0, p.speaking, CARD);
                        }
                        ui.add_space(14.0);
                        ui.label(theme::muted(format!("{} connecté(s)", people.len())));
                    });
                }
                let br = egui::Rect::from_min_size(rect.right_bottom() - vec2(120.0, 46.0), vec2(106.0, 34.0));
                let mut b = ui.new_child(egui::UiBuilder::new().max_rect(br));
                let clicked = theme::button_w(&mut b, Kind::Primary, Some(icon::MIC), "Rejoindre", 34.0, true, Some(106.0)).clicked();
                if clicked || resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.join_voice(&server.id, c.id);
                }
            }
        });
    }

    /// The voice channel we are in: participant tiles and controls.
    fn stage(&mut self, ui: &mut egui::Ui, server: &Server, v: &VoiceState) {
        let channel = server.admin.channels.iter().find(|c| c.id == v.channel).map(|c| c.name.clone()).unwrap_or_default();
        let people = self.participants(&server.id, v.channel);
        if people.iter().any(|p| p.speaking) || self.node.voice_shared.speaking.load(Ordering::Relaxed) {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
        }
        ui.horizontal(|ui| {
            ui.label(theme::icon_text(icon::SPEAKER, 22.0).color(ONLINE));
            ui.label(theme::title(&channel));
            ui.label(theme::muted(format!("· {}", server.admin.name)));
        });
        ui.add_space(14.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(12.0, 12.0);
            for p in &people {
                let (rect, resp) = ui.allocate_exact_size(vec2(200.0, 170.0), Sense::click());
                let painter = ui.painter();
                painter.rect(
                    rect,
                    CornerRadius::same(16),
                    CARD,
                    Stroke::new(if p.speaking { 2.0 } else { 1.0 }, if p.speaking { ONLINE } else { BORDER }),
                    egui::StrokeKind::Inside,
                );
                let av = egui::Rect::from_center_size(rect.center_top() + vec2(0.0, 66.0), vec2(80.0, 80.0));
                theme::paint_avatar(painter, av.shrink(4.0), &p.id, &p.name, None, CARD);
                if p.speaking {
                    painter.circle_stroke(av.center(), 41.0, Stroke::new(3.0, ONLINE));
                }
                let label = if p.me { format!("{} (toi)", p.name) } else { p.name.clone() };
                let mut job = egui::text::LayoutJob::simple_singleline(label, theme::semibold(14.5), TEXT);
                job.wrap = egui::text::TextWrapping { max_width: 170.0, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                let g = painter.layout_job(job);
                painter.galley(egui::pos2(rect.center().x - g.size().x / 2.0, rect.top() + 116.0), g, TEXT);
                let mut x = rect.left() + 10.0;
                if p.live {
                    let g = painter.layout_no_wrap("● EN DIRECT".into(), theme::semibold(10.5), Color32::WHITE);
                    let br = egui::Rect::from_min_size(egui::pos2(x, rect.top() + 10.0), g.size() + vec2(12.0, 4.0));
                    painter.rect_filled(br, CornerRadius::same(5), LIVE);
                    painter.galley(br.center() - g.size() / 2.0, g, Color32::WHITE);
                    x = br.right() + 6.0;
                }
                let _ = x;
                let mut ix = rect.right() - 22.0;
                for (on, ic) in [(p.deafened, icon::HEADPHONE), (p.muted, icon::MIC)] {
                    if on {
                        let c = egui::pos2(ix, rect.top() + 20.0);
                        painter.circle_filled(c, 12.0, Color32::from_rgb(0x3A, 0x1E, 0x23));
                        painter.text(c, Align2::CENTER_CENTER, ic, theme::icon_font(11.0), theme::LIVE_HOVER);
                        painter.line_segment([c + vec2(-6.0, -6.0), c + vec2(6.0, 6.0)], Stroke::new(1.6, theme::LIVE_HOVER));
                        ix -= 28.0;
                    }
                }
                if p.live && !p.me {
                    let br = egui::Rect::from_min_size(egui::pos2(rect.center().x - 55.0, rect.bottom() - 40.0), vec2(110.0, 30.0));
                    let mut b = ui.new_child(egui::UiBuilder::new().max_rect(br));
                    if theme::button_w(&mut b, Kind::Live, Some(icon::PLAY), "Regarder", 30.0, true, Some(110.0)).clicked()
                        && let Ok(eid) = parse_friend_code(&p.id)
                    {
                        self.rt.spawn(self.node.clone().watch(eid));
                    }
                } else if !p.me {
                    let vol = self.node.voice_shared.gains.lock().unwrap().get(&p.id).copied().unwrap_or(100);
                    if vol != 100 {
                        let t = if vol == 0 { "muet pour toi".to_string() } else { format!("volume {vol} %") };
                        ui.painter().text(rect.center_bottom() - vec2(0.0, 22.0), Align2::CENTER_CENTER, t, egui::FontId::proportional(12.0), FAINT);
                    }
                }
                if !p.me {
                    self.member_menu(&resp, &p.id, &p.name, server, p.live);
                }
            }
        });
        if people.len() == 1 {
            ui.add_space(4.0);
            ui.label(theme::muted("Tu es seul ici. Tes potes te verront dans ce salon et pourront te rejoindre en un clic."));
        }

        // Control bar.
        ui.add_space(16.0);
        egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(14))
            .inner_margin(Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    self.voice_buttons(ui, v, 42.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if theme::button(ui, Kind::Danger, Some(icon::HANGUP), "Quitter", 42.0, true).clicked() {
                            self.leave_voice();
                        }
                        let hosting = self.node.live.lock().unwrap().hosting.clone();
                        match hosting {
                            Some(h) => {
                                if theme::button(ui, Kind::Secondary, Some(icon::STOP), "Arrêter mon partage", 42.0, true).clicked() {
                                    let n = self.node.clone();
                                    self.rt.spawn_blocking(move || n.stop_host());
                                }
                                if h.audience != Audience::Server(server.id.clone()) {
                                    ui.label(theme::muted("Ton partage n'est pas visible par ce serveur"));
                                }
                            }
                            None => {
                                if theme::button(ui, Kind::Primary, Some(icon::MONITOR), "Partager mon écran", 42.0, true)
                                    .on_hover_text("Les membres du serveur pourront regarder sans demander")
                                    .clicked()
                                {
                                    self.audience = Audience::Server(server.id.clone());
                                    self.view = View::Home;
                                    self.page = Page::Share;
                                }
                            }
                        }
                    });
                });
            });
    }

    fn member_list(&mut self, ui: &mut egui::Ui, server: &Server, me: &str) {
        let peers = self.node.live.lock().unwrap().peers.clone();
        let mut members: Vec<_> = server.active_members().cloned().collect();
        members.sort_by_key(|m| (m.id != me && !peers.get(&m.id).is_some_and(|p| p.online), m.name.to_lowercase()));
        let online = members.iter().filter(|m| m.id == me || peers.get(&m.id).is_some_and(|p| p.online)).count();
        theme::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::strong(format!("Membres · {online}/{} en ligne", members.len()), 16.0));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if theme::button(ui, Kind::Secondary, Some(icon::ADD_FRIEND), "Inviter", 32.0, true).clicked() {
                        self.open_modal(Modal::Invite { sid: server.id.clone() });
                    }
                });
            });
            ui.add_space(4.0);
            for m in &members {
                let p = peers.get(&m.id).cloned().unwrap_or_default();
                let is_me = m.id == me;
                let on = is_me || p.online;
                let name = if is_me { self.my_name() } else { self.node.store.lock().display_name(&m.id) };
                let (resp, _) = theme::row(ui, 48.0, false, |ui| {
                    if !on {
                        ui.multiply_opacity(0.5);
                    }
                    let status = if p.stream.is_some() { LIVE } else if on { ONLINE } else { FAINT };
                    theme::avatar(ui, &m.id, &name, 32.0, Some(status), CARD);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.horizontal(|ui| {
                            theme::truncated(ui, &name, theme::semibold(14.0), TEXT, 220.0);
                            if m.id == server.owner {
                                ui.label(theme::icon_text(icon::STAR, 12.0).color(Color32::from_rgb(0xF0, 0xB2, 0x32)))
                                    .on_hover_text("Créateur du serveur");
                            }
                            if is_me {
                                ui.label(RichText::new("(toi)").size(12.0).color(FAINT));
                            }
                        });
                        let chan = |v: &VoiceState| server.admin.channels.iter().find(|c| c.id == v.channel).map(|c| c.name.clone());
                        let voice = if is_me { self.node.live.lock().unwrap().voice.clone() } else { p.voice.clone() };
                        let sub = match voice.as_ref().filter(|v| v.server == server.id).and_then(chan) {
                            Some(c) => format!("dans « {c} »"),
                            None if p.stream.is_some() => "en direct".into(),
                            None if on => "en ligne".into(),
                            None => "hors ligne".into(),
                        };
                        ui.label(RichText::new(sub).size(12.0).color(MUTED));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if p.stream.is_some() && !is_me
                            && theme::button(ui, Kind::Live, Some(icon::PLAY), "Regarder", 28.0, true).clicked()
                            && let Ok(eid) = parse_friend_code(&m.id)
                        {
                            self.rt.spawn(self.node.clone().watch(eid));
                        }
                    });
                });
                if !is_me {
                    self.member_menu(&resp, &m.id, &name, server, p.stream.is_some());
                }
            }
        });
    }

    // ================================================================ modals

    pub(super) fn server_modal(&mut self, ctx: &egui::Context, modal: &Modal, first: bool) -> bool {
        let mut close = false;
        match modal {
            Modal::CreateServer => {
                let resp = egui::Modal::new(egui::Id::unique("create_server")).frame(Self::modal_frame()).show(ctx, |ui| {
                    ui.set_width(420.0);
                    ui.label(theme::strong("Créer un serveur", 19.0));
                    ui.label(theme::muted(
                        "Un serveur = ta bande de potes, avec des salons vocaux. Rien n'est hébergé : chaque membre garde juste la liste chez lui.",
                    ));
                    ui.add_space(10.0);
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.new_server_name)
                            .hint_text("Nom du serveur (ex. « Les potes du jeudi »)")
                            .desired_width(f32::INFINITY)
                            .margin(Margin::symmetric(10, 9)),
                    );
                    if first {
                        r.request_focus();
                    }
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add_space(12.0);
                    let ok = !self.new_server_name.trim().is_empty();
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if (theme::button(ui, Kind::Primary, Some(icon::ADD), "Créer", 36.0, ok).clicked() || enter) && ok {
                            let name: String = self.new_server_name.trim().chars().take(40).collect();
                            let id = self.node.create_server(&name);
                            self.view = View::Server(id.clone());
                            self.next_modal = Some(Modal::Invite { sid: id });
                            close = true;
                        }
                        if theme::button(ui, Kind::Ghost, None, "Annuler", 36.0, true).clicked() {
                            close = true;
                        }
                    });
                });
                close |= resp.should_close();
            }
            Modal::Invite { sid } => {
                let Some(server) = self.node.store.lock().server(sid).cloned() else { return true };
                let friends = self.node.store.lock().friends.clone();
                let peers = self.node.live.lock().unwrap().peers.clone();
                let owner = server.owner == self.node.id().to_z32();
                let resp = egui::Modal::new(egui::Id::unique("invite")).frame(Self::modal_frame()).show(ctx, |ui| {
                    ui.set_width(440.0);
                    ui.label(theme::strong(format!("Inviter dans « {} »", server.admin.name), 19.0));
                    ui.label(theme::muted("Tu peux inviter tes amis. Ils reçoivent l'invitation dans MyStream."));
                    ui.add_space(10.0);
                    let candidates: Vec<_> = friends
                        .iter()
                        .filter(|f| !server.is_member(&f.id))
                        .filter(|f| owner || !server.admin.banned.contains(&f.id))
                        .collect();
                    if candidates.is_empty() {
                        ui.label(theme::muted("Tous tes amis sont déjà dans ce serveur. Ajoute d'autres amis avec leur code depuis l'accueil."));
                    }
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for f in candidates {
                            let on = peers.get(&f.id).is_some_and(|p| p.online);
                            theme::row(ui, 48.0, false, |ui| {
                                theme::avatar(ui, &f.id, &f.name, 32.0, Some(if on { ONLINE } else { FAINT }), ELEVATED);
                                theme::truncated(ui, &f.name, theme::semibold(14.0), TEXT, 200.0);
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    let sent = self.invited.contains(&(sid.clone(), f.id.clone()));
                                    let label = if sent { "Invité" } else { "Inviter" };
                                    if theme::button(ui, Kind::Secondary, None, label, 30.0, !sent).clicked()
                                        && let Ok(eid) = parse_friend_code(&f.id)
                                    {
                                        self.invited.push((sid.clone(), f.id.clone()));
                                        self.rt.spawn(self.node.clone().invite_to_server(sid.clone(), eid));
                                    }
                                });
                            });
                        }
                    });
                    ui.add_space(12.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if theme::button(ui, Kind::Primary, None, "Terminé", 36.0, true).clicked() {
                            close = true;
                        }
                    });
                });
                close |= resp.should_close();
            }
            Modal::ServerSettings { sid } => {
                let Some(server) = self.node.store.lock().server(sid).cloned() else { return true };
                let resp = egui::Modal::new(egui::Id::unique("server_settings")).frame(Self::modal_frame()).show(ctx, |ui| {
                    ui.set_width(460.0);
                    ui.label(theme::strong("Paramètres du serveur", 19.0));
                    ui.add_space(8.0);
                    ui.label(theme::strong("Nom", 14.0));
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.rename_edit).desired_width(300.0).margin(Margin::symmetric(10, 7)));
                        let name: String = self.rename_edit.trim().chars().take(40).collect();
                        let ok = !name.is_empty() && name != server.admin.name;
                        if theme::button(ui, Kind::Secondary, None, "Renommer", 32.0, ok).clicked() {
                            self.node.admin_update(sid, |a| a.name = name);
                        }
                    });
                    ui.add_space(10.0);
                    ui.label(theme::strong("Salons vocaux", 14.0));
                    for c in &server.admin.channels {
                        theme::row(ui, 38.0, false, |ui| {
                            ui.label(theme::icon_text(icon::SPEAKER, 14.0).color(MUTED));
                            ui.label(&c.name);
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let last = server.admin.channels.len() <= 1;
                                if !last && theme::icon_button(ui, icon::DELETE, "Supprimer ce salon").clicked() {
                                    let cid = c.id;
                                    self.node.admin_update(sid, |a| a.channels.retain(|x| x.id != cid));
                                }
                            });
                        });
                    }
                    ui.horizontal(|ui| {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.new_channel)
                                .hint_text("Nouveau salon")
                                .desired_width(300.0)
                                .margin(Margin::symmetric(10, 7)),
                        );
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let name: String = self.new_channel.trim().chars().take(32).collect();
                        let ok = !name.is_empty() && server.admin.channels.len() < 20;
                        if (theme::button(ui, Kind::Secondary, Some(icon::ADD), "Ajouter", 32.0, ok).clicked() || enter) && ok {
                            self.node.admin_update(sid, |a| {
                                let id = a.channels.iter().map(|c| c.id).max().unwrap_or(0) + 1;
                                a.channels.push(crate::store::Channel { id, name });
                            });
                            self.new_channel.clear();
                        }
                    });
                    ui.add_space(14.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if theme::button(ui, Kind::Primary, None, "Terminé", 36.0, true).clicked() {
                            close = true;
                        }
                        if theme::button(ui, Kind::Danger, Some(icon::DELETE), "Supprimer le serveur", 36.0, true).clicked() {
                            self.next_modal = Some(Modal::LeaveServer { sid: sid.clone() });
                            close = true;
                        }
                    });
                });
                close |= resp.should_close();
            }
            Modal::LeaveServer { sid } => {
                let Some(server) = self.node.store.lock().server(sid).cloned() else { return true };
                let owner = server.owner == self.node.id().to_z32();
                let resp = egui::Modal::new(egui::Id::unique("leave_server")).frame(Self::modal_frame()).show(ctx, |ui| {
                    ui.set_width(400.0);
                    if owner {
                        ui.label(theme::strong(format!("Supprimer « {} » ?", server.admin.name), 18.0));
                        ui.label(theme::muted("Le serveur disparaît pour tout le monde. Vous restez amis."));
                    } else {
                        ui.label(theme::strong(format!("Quitter « {} » ?", server.admin.name), 18.0));
                        ui.label(theme::muted("Un membre pourra te réinviter plus tard."));
                    }
                    ui.add_space(14.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let label = if owner { "Supprimer" } else { "Quitter" };
                        if theme::button(ui, Kind::Danger, None, label, 36.0, true).clicked() {
                            self.view = View::Home;
                            self.rt.spawn(self.node.clone().leave_server(sid.clone()));
                            close = true;
                        }
                        if theme::button(ui, Kind::Ghost, None, "Annuler", 36.0, true).clicked() {
                            close = true;
                        }
                    });
                });
                close |= resp.should_close();
            }
            _ => {}
        }
        close
    }

    /// Server invitations, shown at the top of the home sidebar.
    pub(super) fn invitations(&mut self, ui: &mut egui::Ui) {
        let invites = self.node.store.lock().server_invites.clone();
        if invites.is_empty() {
            return;
        }
        ui.add_space(4.0);
        theme::section(ui, &format!("Invitations · {}", invites.len()));
        for inv in &invites {
            theme::row(ui, 52.0, false, |ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::hover());
                ui.painter().rect_filled(rect, CornerRadius::same(10), theme::avatar_color(&inv.server.id));
                ui.painter().text(rect.center(), Align2::CENTER_CENTER, theme::initials(&inv.server.admin.name), theme::semibold(13.0), Color32::WHITE);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    theme::truncated(ui, &inv.server.admin.name, theme::semibold(14.0), TEXT, 110.0);
                    theme::truncated(ui, &format!("de {}", inv.from_name), egui::FontId::proportional(12.0), MUTED, 110.0);
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if theme::icon_button_sized(ui, icon::CLOSE, "Refuser", 30.0, MUTED).clicked() {
                        self.node.decline_invite(&inv.server.id);
                    }
                    if theme::icon_button_sized(ui, icon::CHECK, "Rejoindre", 30.0, ONLINE).clicked() {
                        let n = self.node.clone();
                        let sid = inv.server.id.clone();
                        self.rt.spawn(async move { n.accept_invite(&sid) });
                        self.view = View::Server(inv.server.id.clone());
                    }
                });
            });
        }
        ui.add_space(8.0);
    }
}

/// 48 px square slot in the server rail with the Discord-style left pill.
fn rail_item(ui: &mut egui::Ui, selected: bool, paint: impl FnOnce(&egui::Painter, egui::Rect)) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::click());
    paint(ui.painter(), rect);
    let h = if selected {
        36.0
    } else if resp.hovered() {
        18.0
    } else {
        0.0
    };
    if h > 0.0 {
        let x = ui.max_rect().left();
        let pill = egui::Rect::from_center_size(egui::pos2(x + 2.0, rect.center().y), vec2(4.0, h));
        ui.painter().rect_filled(pill, CornerRadius::same(2), TEXT);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
