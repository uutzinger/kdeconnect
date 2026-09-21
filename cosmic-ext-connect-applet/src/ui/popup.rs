use crate::messages::Message;
use crate::models::{Device, NowPlaying};
use cosmic::app::Core;
use cosmic::iced::{Alignment, Length};
use cosmic::widget::Row;
use cosmic::{Element, widget};
use std::collections::HashMap;

/// Everything the popup view renders, borrowed from the applet.
pub struct PopupState<'a> {
    pub devices: &'a HashMap<String, Device>,
    pub expanded_device: Option<&'a String>,
    pub pairing_requests: Option<&'a HashMap<String, String>>,
    pub unread_sms: &'a HashMap<String, bool>,
    pub error_banner: Option<&'a String>,
    pub now_playing: &'a HashMap<String, NowPlaying>,
    pub recent_transfers: &'a std::collections::VecDeque<kdeconnect_core::event::TransferStatus>,
}

/// Build the popup view using the real application Core so popup_container
/// has proper applet context, theme, and sizing.
pub fn create_popup_view<'a>(core: &'a Core, state: &PopupState<'a>) -> Element<'a, Message> {
    let PopupState {
        devices,
        expanded_device,
        pairing_requests,
        unread_sms,
        error_banner,
        now_playing,
        recent_transfers,
    } = *state;
    let spacing = cosmic::theme::active().cosmic().spacing;
    let mut content = widget::Column::new()
        .spacing(spacing.space_s)
        .padding(spacing.space_s);

    // Header
    content = content.push(
        Row::new()
            .push(
                widget::text(fl!("applet-title"))
                    .size(18)
                    .width(Length::Fill),
            )
            .push(widget::button::standard(fl!("applet-settings")).on_press(Message::OpenSettings))
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center),
    );

    content = content.push(widget::divider::horizontal::default());

    // Dismissible error banner — surfaces failures (e.g. browse-device
    // preflight checks) that used to be silently dropped.
    if let Some(message) = error_banner {
        content = content.push(
            widget::container(
                Row::new()
                    .push(widget::text(message).size(12).width(Length::Fill))
                    .push(
                        widget::button::icon(
                            widget::icon::from_name("window-close-symbolic").handle(),
                        )
                        .on_press(Message::DismissError),
                    )
                    .spacing(spacing.space_xs)
                    .align_y(Alignment::Center),
            )
            .padding(spacing.space_s)
            .style(|_: &cosmic::Theme| cosmic::widget::container::Style {
                border: cosmic::iced::Border {
                    color: cosmic::iced::Color::from_rgb(0.8, 0.2, 0.2),
                    width: 1.5,
                    radius: 8.0.into(),
                },
                ..Default::default()
            })
            .class(cosmic::theme::Container::Card)
            .width(Length::Fill),
        );
    }

    // Pairing requests — sourced from the applet's live pairing_requests map,
    // not from Device.pairing_requests which is never populated via D-Bus.
    if let Some(requests) = pairing_requests {
        if !requests.is_empty() {
            content = content.push(
                widget::text(fl!("pairing-requests"))
                    .size(14)
                    .font(cosmic::font::bold()),
            );

            let mut sorted: Vec<(&String, &String)> = requests.iter().collect();
            sorted.sort_by(|a, b| a.1.cmp(b.1));

            for (device_id, device_name) in sorted {
                let device_id_accept = device_id.clone();
                let device_id_reject = device_id.clone();

                let request_card = widget::container(
                    widget::Column::new()
                        .push(
                            Row::new()
                                .push(widget::icon::from_name("phone-symbolic").size(24))
                                .push(
                                    widget::Column::new()
                                        .push(widget::text(device_name).size(14))
                                        .push(widget::text(fl!("pairing-wants-to-pair")).size(11))
                                        .spacing(spacing.space_xxxs),
                                )
                                .spacing(spacing.space_s)
                                .align_y(Alignment::Center),
                        )
                        .push(widget::Space::new().height(Length::Fixed(spacing.space_xs as f32)))
                        .push(
                            Row::new()
                                .push(
                                    widget::button::suggested(fl!("pairing-accept"))
                                        .on_press(Message::AcceptPairing(device_id_accept))
                                        .width(Length::Fill),
                                )
                                .push(
                                    widget::button::destructive(fl!("pairing-reject"))
                                        .on_press(Message::RejectPairing(device_id_reject))
                                        .width(Length::Fill),
                                )
                                .spacing(spacing.space_xs),
                        )
                        .spacing(spacing.space_xs),
                )
                .padding(spacing.space_s)
                .class(cosmic::theme::Container::Card)
                .width(Length::Fill);

                content = content.push(request_card);
            }

            content = content.push(widget::divider::horizontal::default());
        }
    }

    // All paired devices — reachable and unreachable — sorted alphabetically
    let mut paired_devices: Vec<_> = devices.values().filter(|d| d.is_paired).collect();
    paired_devices.sort_by(|a, b| a.name.cmp(&b.name));

    if paired_devices.is_empty() {
        content = content.push(
            widget::container(widget::text(fl!("devices-none-paired")).size(14))
                .padding(spacing.space_m)
                .width(Length::Fill)
                .center_x(Length::Fill),
        );
    } else {
        content = content.push(
            widget::text(fl!("devices-header"))
                .size(14)
                .font(cosmic::font::bold()),
        );

        for device in paired_devices {
            let device_unread = unread_sms.get(&device.id).copied().unwrap_or(false);
            let device_transfers: Vec<&kdeconnect_core::event::TransferStatus> = recent_transfers
                .iter()
                .filter(|s| s.device_id == device.id)
                .take(3)
                .collect();
            content = content.push(create_device_card(
                device,
                &spacing,
                expanded_device,
                device_unread,
                device_transfers,
            ));
        }
    }

    // Media section — one card per active, actually-controllable phone
    // player, at the bottom. Players that advertise none of the four
    // transport controls (e.g. some browser tabs' media notifications)
    // are skipped entirely rather than shown as a box of dead buttons.
    let mut players: Vec<(&String, &NowPlaying)> = now_playing
        .iter()
        .filter(|(_, p)| p.can_play || p.can_pause || p.can_go_next || p.can_go_previous)
        .collect();
    players.sort_by(|a, b| a.1.identity.cmp(&b.1.identity));

    if !players.is_empty() {
        content = content.push(widget::divider::horizontal::default());
        content = content.push(
            widget::text(fl!("media-header"))
                .size(14)
                .font(cosmic::font::bold()),
        );

        for (bus_name, player) in players {
            content = content.push(create_media_card(bus_name, player, &spacing));
        }
    }

    let popup_content = widget::container(widget::scrollable(content))
        .width(Length::Fixed(400.0))
        .max_height(700.0)
        .padding(spacing.space_xs);

    // Use the real Core so the popup has proper applet context and theme
    core.applet.popup_container(popup_content).into()
}

fn create_device_card<'a>(
    device: &'a Device,
    spacing: &cosmic::cosmic_theme::Spacing,
    expanded_device: Option<&'a String>,
    has_unread_sms: bool,
    transfers: Vec<&'a kdeconnect_core::event::TransferStatus>,
) -> Element<'a, Message> {
    let is_expanded = expanded_device == Some(&device.id);
    let is_online = device.is_reachable;

    let mut name_row = Row::new()
        .push(widget::icon::from_name(device.device_icon()).size(20))
        .push(widget::text(&device.name).size(14).width(Length::Fill))
        .spacing(spacing.space_xs)
        .align_y(Alignment::Center);

    if !is_online {
        name_row = name_row.push(widget::text(fl!("devices-offline")).size(11));
    } else {
        if let Some(signal_icon) = device.signal_icon() {
            name_row = name_row.push(widget::icon::from_name(signal_icon).size(16));
        }
        if let Some(level) = device.battery_level {
            name_row = name_row.push(
                Row::new()
                    .spacing(2)
                    .align_y(Alignment::Center)
                    .push(widget::icon::from_name(device.battery_icon()).size(16))
                    .push(widget::text(format!("{}%", level)).size(11)),
            );
        }
    }

    name_row = name_row.push(
        widget::button::icon(widget::icon::from_name(if is_expanded {
            "go-up-symbolic"
        } else {
            "go-down-symbolic"
        }))
        .on_press(Message::ToggleDeviceMenu(device.id.clone()))
        .class(cosmic::theme::Button::Icon),
    );

    let device_button = widget::button::custom(name_row)
        .on_press(Message::ToggleDeviceMenu(device.id.clone()))
        .width(Length::Fill);

    let mut col = widget::Column::new()
        .width(Length::Fill)
        .push(device_button);

    if is_expanded && is_online {
        let mut menu_items = widget::Column::new().spacing(spacing.space_xxs);

        let mut quick_actions_list =
            widget::list_column().style(cosmic::theme::Container::Transparent);

        quick_actions_list =
            quick_actions_list.add(widget::text::caption_heading(fl!("quick-actions-header")));

        quick_actions_list = quick_actions_list.add(
            widget::button::custom(
                widget::text::caption(fl!("quick-actions-ping")).class(cosmic::theme::Text::Accent),
            )
            .on_press(Message::PingDevice(device.id.clone()))
            .class(cosmic::theme::Button::Link),
        );

        if device.has_findmyphone {
            quick_actions_list = quick_actions_list.add(
                widget::button::custom(
                    widget::text::caption(fl!("quick-actions-find-phone"))
                        .class(cosmic::theme::Text::Accent),
                )
                .on_press(Message::RingDevice(device.id.clone()))
                .class(cosmic::theme::Button::Link),
            );
        }

        if device.has_clipboard {
            quick_actions_list = quick_actions_list.add(
                widget::button::custom(
                    widget::text::caption(fl!("quick-actions-share-clipboard"))
                        .class(cosmic::theme::Text::Accent),
                )
                .on_press(Message::ShareClipboard(device.id.clone()))
                .class(cosmic::theme::Button::Link),
            );
        }

        let mut sms_label = Row::new()
            .push(widget::text::caption_heading(fl!("quick-actions-sms")))
            .align_y(Alignment::Center)
            .spacing(spacing.space_xs);

        if has_unread_sms {
            sms_label = sms_label.push(
                widget::container(
                    widget::Space::new()
                        .width(Length::Fixed(8.0))
                        .height(Length::Fixed(8.0)),
                )
                .class(cosmic::theme::Container::custom(move |_theme| {
                    cosmic::iced::widget::container::Style {
                        border: cosmic::iced::Border {
                            radius: cosmic::iced::Radius::from(4.0),
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                })),
            );
        }

        quick_actions_list = quick_actions_list.add(
            widget::button::custom(sms_label)
                .on_press(Message::SendSMS(device.id.clone()))
                .class(cosmic::theme::Button::Standard)
                .width(Length::Fill),
        );

        if device.has_share || device.has_sftp {
            quick_actions_list = quick_actions_list.add(widget::text::caption_heading(fl!(
                "quick-actions-files-header"
            )));

            if device.has_share {
                quick_actions_list = quick_actions_list.add(
                    widget::button::custom(
                        widget::text::caption(fl!("quick-actions-send-file"))
                            .class(cosmic::theme::Text::Accent),
                    )
                    .on_press(Message::SendFiles(device.id.clone()))
                    .class(cosmic::theme::Button::Link),
                );

                if device.share_progress.is_some_and(|p| p > 0) {
                    quick_actions_list =
                        quick_actions_list.add(device.share_progress.map(|progress| {
                            widget::progress_bar::determinate_linear(progress as f32 / 100.0)
                        }));
                }

                // Recent incoming transfers: receiving progress, saved path
                // on completion, stage/reason on failure.
                for transfer in transfers {
                    use kdeconnect_core::event::TransferState;
                    let name = transfer
                        .filename
                        .clone()
                        .unwrap_or_else(|| "file".to_string());
                    match &transfer.state {
                        TransferState::Receiving => {
                            let mut col = widget::Column::new().push(widget::text::caption(
                                fl!("transfer-receiving", name = name.as_str()),
                            ));
                            if let Some(expected) = transfer.expected_size.filter(|e| *e > 0) {
                                col = col.push(widget::progress_bar::determinate_linear(
                                    (transfer.received_bytes as f32 / expected as f32).min(1.0),
                                ));
                            }
                            quick_actions_list = quick_actions_list.add(col);
                        }
                        TransferState::Completed => {
                            let mut col = widget::Column::new().push(widget::text::caption(
                                fl!("transfer-completed", name = name.as_str()),
                            ));
                            if let Some(path) = &transfer.saved_path {
                                col = col.push(widget::text(path).size(10));
                            }
                            quick_actions_list = quick_actions_list.add(col);
                        }
                        TransferState::Failed { stage, reason } => {
                            quick_actions_list = quick_actions_list.add(
                                widget::Column::new()
                                    .push(widget::text::caption(fl!(
                                        "transfer-failed",
                                        name = name.as_str()
                                    )))
                                    .push(widget::text(format!("{stage}: {reason}")).size(10)),
                            );
                        }
                    }
                }
            }

            if device.has_sftp {
                quick_actions_list = quick_actions_list.add(
                    widget::button::custom(
                        widget::text::caption(fl!("quick-actions-browse-device"))
                            .class(cosmic::theme::Text::Accent),
                    )
                    .on_press(Message::BrowseDevice(device.id.clone()))
                    .class(cosmic::theme::Button::Link),
                );
                if device.is_mounted {
                    quick_actions_list = quick_actions_list.add(
                        widget::button::custom(
                            widget::text::caption(fl!("quick-actions-unmount-device"))
                                .class(cosmic::theme::Text::Accent),
                        )
                        .on_press(Message::UnmountDevice(device.id.clone()))
                        .class(cosmic::theme::Button::Link),
                    );
                }
            }
        }

        if !device.run_commands.is_empty() {
            quick_actions_list = quick_actions_list.add(widget::text::caption_heading(fl!(
                "quick-actions-run-commands-header"
            )));
            for (key, name) in &device.run_commands {
                let key = key.clone();
                quick_actions_list = quick_actions_list.add(
                    widget::button::custom(
                        widget::text::caption(name.as_str()).class(cosmic::theme::Text::Accent),
                    )
                    .on_press(Message::ExecuteRunCommand(device.id.clone(), key))
                    .class(cosmic::theme::Button::Link),
                );
            }
        }

        menu_items = menu_items.push(quick_actions_list);

        col = col.push(widget::container(menu_items).padding([spacing.space_xs, spacing.space_m]));
    } else if is_expanded && !is_online {
        col = col.push(
            widget::container(widget::text(fl!("devices-not-reachable")).size(12))
                .padding([spacing.space_xs, spacing.space_m])
                .class(cosmic::theme::Container::Card),
        );
    }

    col.into()
}

/// One card per active phone media player: album art (if downloaded),
/// title/artist, and prev/play-pause/next controls that call straight
/// through to the player's own MPRIS D-Bus service.
fn create_media_card<'a>(
    bus_name: &'a str,
    player: &'a NowPlaying,
    spacing: &cosmic::cosmic_theme::Spacing,
) -> Element<'a, Message> {
    let art: Element<'a, Message> = if let Some(ref path) = player.art_path {
        widget::image(widget::image::Handle::from_path(path))
            .width(Length::Fixed(64.0))
            .height(Length::Fixed(64.0))
            .into()
    } else {
        widget::icon::from_name("audio-x-generic-symbolic")
            .size(64)
            .into()
    };

    let title = player.title.clone();
    let artist = player.artist.clone().unwrap_or_default();

    let mut info_col = widget::Column::new()
        .spacing(2)
        .width(Length::Fill)
        .align_x(Alignment::Center);

    // Always label which app this card controls — with metadata present that's
    // a small caption above the song title; without it (e.g. a browser tab
    // whose media session doesn't report a track) it's the only line shown,
    // so the card never just reads as an empty box.
    if let Some(ref title) = title {
        info_col = info_col.push(widget::text(player.identity.clone()).size(10));
        info_col = info_col.push(
            widget::text(title.clone())
                .size(13)
                .font(cosmic::font::bold()),
        );
    } else {
        info_col = info_col.push(
            widget::text(player.identity.clone())
                .size(13)
                .font(cosmic::font::bold()),
        );
    }
    info_col = info_col.push_maybe((!artist.is_empty()).then(|| widget::text(artist).size(11)));

    let play_icon = if player.is_playing {
        "media-playback-pause-symbolic"
    } else {
        "media-playback-start-symbolic"
    };

    let mut prev_btn =
        widget::button::icon(widget::icon::from_name("media-skip-backward-symbolic"));
    if player.can_go_previous {
        prev_btn = prev_btn.on_press(Message::MprisPrevious(bus_name.to_string()));
    }

    // Some phone-side players (browser tabs in particular) advertise a media
    // session with no working transport controls at all — leave the button
    // disabled rather than pretend it does something.
    let mut play_pause_btn =
        widget::button::icon(widget::icon::from_name(play_icon)).class(cosmic::theme::Button::Icon);
    if player.can_play || player.can_pause {
        play_pause_btn = play_pause_btn.on_press(Message::MprisPlayPause(bus_name.to_string()));
    }

    let mut next_btn = widget::button::icon(widget::icon::from_name("media-skip-forward-symbolic"))
        .class(cosmic::theme::Button::Icon);
    if player.can_go_next {
        next_btn = next_btn.on_press(Message::MprisNext(bus_name.to_string()));
    }

    let controls = Row::new()
        .push(prev_btn)
        .push(play_pause_btn)
        .push(next_btn)
        .spacing(spacing.space_xxs)
        .align_y(Alignment::Center);

    let top_row = Row::new()
        .push(art)
        .push(widget::Space::new().width(Length::Fill))
        .push(controls)
        .align_y(Alignment::Center);

    widget::container(
        widget::Column::new()
            .push(top_row)
            .push(info_col)
            .spacing(spacing.space_s),
    )
    .padding(spacing.space_s)
    .class(cosmic::theme::Container::Card)
    .width(Length::Fill)
    .into()
}
