//! Startup update prompt dialog for egui frontends.

use egui::{Align, Align2, Color32, Layout, RichText, Vec2};

use crate::app::App;
use crate::release::Release;

/// What the user chose in the startup update prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupPromptResponse {
    /// Dialog is still open and waiting for user input.
    Pending,
    /// User chose to update and restart now.
    UpdateAndRestart,
    /// User chose to download manually via browser.
    Download,
    /// User dismissed the dialog for now.
    Later,
}

/// State for the startup update prompt dialog.
#[derive(Clone, Debug)]
pub struct StartupPrompt {
    pub app: &'static App,
    pub release: Release,
    pub running_version: String,
    pub is_self_updating: bool,
    pub open: bool,
    pub status_message: Option<String>,
    pub is_installing: bool,
}

impl StartupPrompt {
    /// Creates a new prompt dialog state for the given app and newly discovered release.
    pub fn new(app: &'static App, release: Release, running_version: impl Into<String>) -> Self {
        let is_self_updating = crate::self_update::is_self_updating(app);
        Self { app, release, running_version: running_version.into(), is_self_updating, open: true, status_message: None, is_installing: false }
    }

    /// Renders the modal window on the given egui context.
    pub fn show(&mut self, ctx: &egui::Context) -> StartupPromptResponse {
        if !self.open {
            return StartupPromptResponse::Later;
        }

        let mut response = StartupPromptResponse::Pending;

        // Modal dimmed background overlay; it senses clicks so they don't reach widgets underneath.
        let screen_rect = ctx.content_rect();
        egui::Area::new(egui::Id::new("craft_update_backdrop")).order(egui::Order::Middle).fixed_pos(screen_rect.min).show(ctx, |ui| {
            let (rect, _) = ui.allocate_exact_size(screen_rect.size(), egui::Sense::click_and_drag());
            ui.painter().rect_filled(rect, 0.0, Color32::from_black_alpha(150));
        });

        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            response = StartupPromptResponse::Later;
        }

        let is_dark = ctx.theme() == egui::Theme::Dark;
        let card_bg = if is_dark { Color32::from_rgb(26, 26, 28) } else { Color32::from_rgb(255, 255, 255) };
        let border_color = if is_dark { Color32::from_rgb(52, 52, 56) } else { Color32::from_rgb(215, 215, 220) };
        let text_primary = if is_dark { Color32::from_rgb(245, 245, 248) } else { Color32::from_rgb(20, 20, 25) };
        let text_muted = if is_dark { Color32::from_rgb(150, 150, 160) } else { Color32::from_rgb(105, 105, 115) };
        let accent_color = Color32::from_rgb(52, 120, 246);

        egui::Area::new(egui::Id::new("craft_update.startup_prompt")).anchor(Align2::CENTER_CENTER, Vec2::ZERO).order(egui::Order::Foreground).show(
            ctx,
            |ui| {
                egui::Frame::new()
                    .fill(card_bg)
                    .stroke(egui::Stroke::new(1.0, border_color))
                    .corner_radius(egui::CornerRadius::ZERO)
                    .inner_margin(egui::Margin::symmetric(20, 18))
                    .show(ui, |ui| {
                        ui.set_width(400.0);
                        ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);

                        // Header row: App badge + title + close
                        ui.horizontal(|ui| {
                            let (badge_rect, _) = ui.allocate_exact_size(Vec2::new(34.0, 34.0), egui::Sense::hover());
                            ui.painter().rect_filled(badge_rect, egui::CornerRadius::ZERO, accent_color);
                            let initial = self.app.name.chars().next().map(|c| c.to_string()).unwrap_or_else(|| "A".to_string());
                            ui.painter().text(badge_rect.center(), Align2::CENTER_CENTER, initial, egui::FontId::monospace(18.0), Color32::WHITE);

                            ui.add_space(4.0);

                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(self.app.name).size(15.0).strong().color(text_primary));
                                    ui.label(RichText::new("UPDATE").size(10.0).monospace().color(accent_color));
                                });
                                let version_pill = format!("v{} → {}", self.running_version, self.release.version_label());
                                ui.label(RichText::new(version_pill).size(11.0).monospace().color(text_muted));
                            });

                            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                                let x_btn = egui::Button::new(RichText::new("✕").size(12.0).color(text_muted)).frame(false);
                                if ui.add(x_btn).clicked() {
                                    response = StartupPromptResponse::Later;
                                }
                            });
                        });

                        ui.add_space(8.0);

                        if let Some(msg) = &self.status_message {
                            let alert_bg = if is_dark { Color32::from_rgb(50, 30, 20) } else { Color32::from_rgb(255, 242, 235) };
                            let alert_color = Color32::from_rgb(225, 95, 30);
                            egui::Frame::new()
                                .fill(alert_bg)
                                .stroke(egui::Stroke::new(1.0, alert_color))
                                .corner_radius(egui::CornerRadius::ZERO)
                                .inner_margin(egui::Margin::symmetric(10, 6))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.label(RichText::new(msg).size(12.0).color(alert_color));
                                });
                            ui.add_space(4.0);
                        }

                        if self.is_installing {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(RichText::new("Downloading and installing update…").size(12.0).color(text_primary));
                            });
                        } else {
                            let desc =
                                format!("{} {} is available with the latest features and bug fixes.", self.app.name, self.release.version_label());
                            ui.label(RichText::new(desc).size(12.5).color(text_muted));
                        }

                        ui.add_space(10.0);
                        let (hairline_rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), egui::Sense::hover());
                        ui.painter().hline(hairline_rect.x_range(), hairline_rect.top(), egui::Stroke::new(1.0, border_color));
                        ui.add_space(8.0);

                        // Footer actions
                        ui.horizontal(|ui| {
                            let what_new = egui::Hyperlink::from_label_and_url(
                                RichText::new("What's new").size(11.5).color(accent_color),
                                &self.release.html_url,
                            );
                            ui.add(what_new);

                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if self.is_installing {
                                    let dismiss_btn = egui::Button::new(RichText::new("DISMISS").size(10.5).monospace().color(text_muted))
                                        .stroke(egui::Stroke::new(1.0, border_color))
                                        .corner_radius(egui::CornerRadius::ZERO)
                                        .min_size(Vec2::new(72.0, 26.0));
                                    if ui.add(dismiss_btn).clicked() {
                                        response = StartupPromptResponse::Later;
                                    }
                                } else {
                                    if self.is_self_updating {
                                        let update_btn = egui::Button::new(
                                            RichText::new("UPDATE AND RESTART").size(10.5).strong().monospace().color(Color32::WHITE),
                                        )
                                        .fill(accent_color)
                                        .corner_radius(egui::CornerRadius::ZERO)
                                        .min_size(Vec2::new(140.0, 26.0));
                                        if ui.add(update_btn).clicked() {
                                            response = StartupPromptResponse::UpdateAndRestart;
                                        }
                                    } else {
                                        let dl_btn =
                                            egui::Button::new(RichText::new("DOWNLOAD").size(10.5).strong().monospace().color(Color32::WHITE))
                                                .fill(accent_color)
                                                .corner_radius(egui::CornerRadius::ZERO)
                                                .min_size(Vec2::new(90.0, 26.0));
                                        if ui.add(dl_btn).clicked() {
                                            response = StartupPromptResponse::Download;
                                        }
                                    }

                                    let later_btn = egui::Button::new(RichText::new("LATER").size(10.5).monospace().color(text_primary))
                                        .stroke(egui::Stroke::new(1.0, border_color))
                                        .corner_radius(egui::CornerRadius::ZERO)
                                        .min_size(Vec2::new(65.0, 26.0));
                                    if ui.add(later_btn).clicked() {
                                        response = StartupPromptResponse::Later;
                                    }
                                }
                            });
                        });
                    });
            },
        );

        if response == StartupPromptResponse::Later {
            self.open = false;
        }

        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::PHOTOCRAFT;

    #[test]
    fn creates_startup_prompt_dialog() {
        let release = Release { version: crate::version::parse("0.2.0"), tag: "v0.2.0".to_string(), ..Release::default() };
        let prompt = StartupPrompt::new(&PHOTOCRAFT, release, "0.1.0");
        assert_eq!(prompt.app.name, "PhotoCraft");
        assert_eq!(prompt.running_version, "0.1.0");
        assert!(prompt.open);
    }
}
