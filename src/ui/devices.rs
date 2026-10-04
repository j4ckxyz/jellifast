//! The device picker: this computer and the other players signed in to the
//! same Jellyfin server.

use egui::{Align, CornerRadius, Layout, Rect, Sense, pos2, vec2};

use crate::api::models::Device;
use crate::app::App;
use crate::i18n::gettext;
use crate::model::Action;
use crate::theme::{self, Icon};

pub const BUTTON_RECT_ID: &str = "devices-button-rect";

pub fn device_icon(kind: &str) -> Icon {
    match kind.to_ascii_lowercase().as_str() {
        "computer" => Icon::Laptop,
        "smartphone" => Icon::Smartphone,
        "tablet" => Icon::Tablet,
        "tv" => Icon::Tv,
        "game_console" => Icon::Gamepad,
        "automobile" => Icon::Car,
        "cast_video" | "cast_audio" | "castaudio" | "castvideo" => Icon::Cast,
        "smartwatch" => Icon::Watch,
        "avr" | "stb" | "audio_dongle" => Icon::Monitor,
        _ => Icon::Speaker,
    }
}

pub fn popup(app: &mut App, ctx: &egui::Context) {
    if !app.show_devices {
        return;
    }
    let palette = app.palette;
    let locale = app.locale;
    let button = ctx
        .data(|data| data.get_temp::<Rect>(egui::Id::new(BUTTON_RECT_ID)))
        .unwrap_or_else(|| Rect::from_min_size(pos2(400.0, 400.0), vec2(0.0, 0.0)));
    let width = 320.0;
    let position = pos2(
        (button.right() - width).max(8.0),
        (button.top() - 12.0).max(8.0),
    );
    let area = egui::Area::new(egui::Id::new("devices-popup"))
        .order(egui::Order::Foreground)
        .fixed_pos(position)
        .pivot(egui::Align2::LEFT_BOTTOM)
        .show(ctx, |ui| {
            super::widgets::menu_frame(&palette).show(ui, |ui| {
                ui.set_width(width);
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    theme::text(
                        ui,
                        gettext(locale, "Connect to a device").as_ref(),
                        theme::bold(16.0),
                        palette.text,
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if app.devices_loading {
                            theme::spinner(ui, 16.0, palette.accent);
                        } else if theme::icon_button(
                            ui,
                            Icon::Refresh,
                            15.0,
                            palette.secondary,
                            palette.text,
                            &gettext(locale, "Refresh"),
                        )
                        .clicked()
                        {
                            app.actions.push(Action::RefreshDevices);
                        }
                    });
                });
                ui.add_space(4.0);
                let local_id = app.local_device_id.clone();
                let mut devices: Vec<Device> = app.devices.clone();
                if app.local_ready
                    && let Some(local_id) = &local_id
                    && !devices
                        .iter()
                        .any(|device| device.id.as_deref() == Some(local_id.as_str()))
                {
                    devices.insert(
                        0,
                        Device {
                            id: Some(local_id.clone()),
                            // Named without its suffix; the row below adds it.
                            name: app.settings.device_name.clone(),
                            is_active: app.local.is_active(),
                            is_restricted: false,
                            volume_percent: Some(crate::app::volume_to_percent(app.local.volume)),
                            supports_volume: Some(true),
                            kind: "computer".into(),
                        },
                    );
                }
                let active_id = match app.target() {
                    crate::app::Target::Local => local_id.clone(),
                    crate::app::Target::Remote(id) => id,
                };
                devices.sort_by_key(|device| device.id != active_id);
                let mut seen = std::collections::HashSet::new();
                devices
                    .retain(|device| device.id.as_ref().is_none_or(|id| seen.insert(id.clone())));

                let max_height = (position.y - ctx.content_rect().top() - 62.0).clamp(52.0, 416.0);
                crate::autoscroll::show(
                    ui,
                    egui::ScrollArea::vertical()
                        .id_salt("connect-device-list")
                        // Grow past the Area's remembered height when discovery
                        // adds devices. Short lists still shrink to their contents.
                        .min_scrolled_height(max_height)
                        .max_height(max_height),
                    egui::Vec2b::new(false, true),
                    |ui| {
                        if devices.is_empty() {
                            ui.add_space(8.0);
                            theme::subtle(
                                ui,
                                &palette,
                                &gettext(
                                    locale,
                                    "No players found. Open a Jellyfin app on another device, then refresh.",
                                ),
                            );
                            ui.add_space(8.0);
                        }

                        for device in &devices {
                            let is_local = device.id.is_some() && device.id == local_id;
                            let active = device.id.is_some() && device.id == active_id;
                            let name = if is_local {
                                // Translators: {name} is the name this computer shows the Jellyfin server.
                                gettext(locale, "{name} (this computer)")
                                    .replace("{name}", &device.name)
                            } else {
                                device.name.clone()
                            };
                            let (rect, response) = ui.allocate_exact_size(
                                vec2(ui.available_width(), 52.0),
                                Sense::click(),
                            );
                            if response.hovered() {
                                ui.painter().rect_filled(
                                    rect,
                                    CornerRadius::same(6),
                                    palette.surface_hover,
                                );
                            }
                            let color = if active { palette.accent } else { palette.text };
                            let icon_rect = Rect::from_center_size(
                                pos2(rect.left() + 24.0, rect.center().y),
                                egui::Vec2::splat(22.0),
                            );
                            device_icon(&device.kind)
                                .image(color, 22.0)
                                .paint_at(ui, icon_rect);
                            let painter = ui.painter().with_clip_rect(rect);
                            crate::bidi::paint_line(
                                &painter,
                                rect.left() + 48.0,
                                rect.right() - 12.0,
                                rect.center().y - 9.0,
                                &name,
                                theme::medium(14.0),
                                color,
                            );
                            let status = if active {
                                gettext(locale, "Listening on this device").into_owned()
                            } else if device.is_restricted {
                                gettext(locale, "Restricted").into_owned()
                            } else if is_local {
                                gettext(locale, "Play here").into_owned()
                            } else {
                                device.kind.replace('_', " ")
                            };
                            painter.text(
                                pos2(rect.left() + 48.0, rect.center().y + 10.0),
                                egui::Align2::LEFT_CENTER,
                                status,
                                theme::regular(12.0),
                                if active {
                                    palette.accent
                                } else {
                                    palette.secondary
                                },
                            );
                            if active {
                                let dot = pos2(rect.right() - 16.0, rect.center().y);
                                ui.painter().circle_filled(dot, 4.0, palette.accent);
                            }
                            if response.clicked()
                                && !active
                                && let Some(id) = &device.id
                            {
                                app.actions.push(Action::Transfer(id.clone()));
                            }
                        }
                    },
                );
            });
        });
    let popup_rect = area.response.rect;
    let clicked_outside = ctx.input(|input| {
        input.pointer.any_pressed()
            && input
                .pointer
                .interact_pos()
                .is_some_and(|pos| !popup_rect.contains(pos) && !button.contains(pos))
    });
    if clicked_outside {
        app.show_devices = false;
    }
}
