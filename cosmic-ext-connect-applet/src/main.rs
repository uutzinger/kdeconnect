#[macro_use]
extern crate cosmic_ext_connect_applet;

use cosmic_ext_connect_applet::{backend, messages, models, portal, ui};

use messages::Message;
use models::{Device, NowPlaying};

use cosmic::app::Core;
use cosmic::iced::window::Id as SurfaceId;
use cosmic::iced::{Limits, Subscription};
use cosmic::surface::action::{app_popup, destroy_popup};
use cosmic::{Element, Task, widget};
use std::collections::HashMap;
use tracing::{debug, error, info, warn};

pub struct KdeConnectApplet {
    core: Core,
    popup: Option<SurfaceId>,
    devices: HashMap<String, Device>,
    expanded_device: Option<String>,
    /// Pending pairing requests: device_id → device_name
    pairing_requests: HashMap<String, String>,
    /// device_id -> has unread SMS, for the quick-actions menu indicator.
    unread_sms: HashMap<String, bool>,
    /// Set when an action fails in a way the user needs to know about and
    /// can act on (e.g. browse-device preflight checks); shown as a
    /// dismissible banner in the popup.
    error_banner: Option<String>,
    /// Media section state, keyed by MPRIS D-Bus bus name. Refreshed by
    /// `backend::mpris_subscription`.
    now_playing: HashMap<String, NowPlaying>,
    /// Recent payload transfers, newest first — one entry per transfer ID,
    /// fed by the service's `transfer_status` signal and seeded from its
    /// bounded recent-results list. Bounded to `MAX_RECENT_TRANSFERS`.
    recent_transfers: std::collections::VecDeque<kdeconnect_core::event::TransferStatus>,
}

/// Cap on the applet-side recent transfer list shown in the popup.
const MAX_RECENT_TRANSFERS: usize = 20;

impl cosmic::Application for KdeConnectApplet {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = "io.github.hepp3n.kdeconnect";

    fn core(&self) -> &Core {
        &self.core
    }
    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn on_close_requested(&self, id: cosmic::iced::window::Id) -> Option<Self::Message> {
        Some(Message::PopupClosed(id))
    }

    fn init(core: Core, _flags: Self::Flags) -> (Self, Task<cosmic::Action<Self::Message>>) {
        tokio::spawn(async {
            if let Err(e) = backend::initialize().await {
                error!("Backend init failed: {:?}", e);
            }
        });

        let app = KdeConnectApplet {
            core,
            popup: None,
            devices: HashMap::new(),
            expanded_device: None,
            pairing_requests: HashMap::new(),
            unread_sms: HashMap::new(),
            error_banner: None,
            now_playing: HashMap::new(),
            recent_transfers: std::collections::VecDeque::new(),
        };

        (app, Task::none())
    }

    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        match message {
            Message::Noop => {}
            Message::TogglePopup => {
                return if let Some(p) = self.popup.take() {
                    cosmic::surface::surface_task(destroy_popup(p))
                } else {
                    let show_popup = cosmic::surface::surface_task(app_popup(
                        |_| Default::default(),
                        |app: &mut KdeConnectApplet| {
                            let new_id = cosmic::iced::window::Id::unique();

                            app.popup.replace(new_id);

                            let mut popup_settings = app.core.applet.get_popup_settings(
                                app.core.main_window_id().unwrap(),
                                new_id,
                                None,
                                None,
                                None,
                            );
                            popup_settings.positioner.size_limits = Limits::NONE
                                .max_width(400.0)
                                .min_width(300.0)
                                .min_height(200.0)
                                .max_height(600.0);
                            popup_settings
                        },
                        None,
                    ));

                    // Fetch devices right away — the polling subscriptions
                    // only run while the popup is open, and their first tick
                    // is a full interval away. The unread-SMS check follows
                    // from the DevicesUpdated this produces.
                    Task::batch(vec![
                        show_popup,
                        Task::perform(backend::fetch_devices(), |devices| {
                            cosmic::Action::App(Message::DevicesUpdated(devices))
                        }),
                    ])
                };
            }
            Message::PopupClosed(id) => {
                if self.popup.as_ref() == Some(&id) {
                    self.popup = None;
                }
            }
            Message::RefreshDevices => {
                // The unread-SMS check follows from the resulting
                // DevicesUpdated, against the fresh device list.
                return Task::perform(backend::fetch_devices(), |devices| {
                    cosmic::Action::App(Message::DevicesUpdated(devices))
                });
            }
            Message::UnreadSmsUpdated(unread) => {
                self.unread_sms = unread;
            }
            Message::DevicesUpdated(devices) => {
                self.devices.clear();
                for device in devices {
                    self.devices.insert(device.id.clone(), device);
                }
                // Refresh the unread-SMS indicators for the devices we just
                // learned about, but only while the popup showing them is
                // open (this also covers the first-ever popup open, when
                // TogglePopup ran with an empty device map).
                if self.popup.is_some() && !self.devices.is_empty() {
                    let device_ids: Vec<String> = self.devices.keys().cloned().collect();
                    return Task::perform(backend::check_unread_sms(device_ids), |unread| {
                        cosmic::Action::App(Message::UnreadSmsUpdated(unread))
                    });
                }
            }
            Message::DelayedRefresh => {
                return Task::perform(backend::fetch_devices(), |devices| {
                    cosmic::Action::App(Message::DevicesUpdated(devices))
                });
            }
            Message::ToggleDeviceMenu(ref device_id) => {
                if self.expanded_device.as_ref() == Some(device_id) {
                    self.expanded_device = None;
                } else {
                    self.expanded_device = Some(device_id.clone());
                    let id = device_id.clone();
                    return Task::perform(
                        async move {
                            backend::request_run_commands(id).await.ok();
                        },
                        |_| cosmic::Action::App(Message::RefreshDevices),
                    );
                }
            }
            Message::SendSMS(ref device_id) => {
                // Look up device name for the window title
                let device_name = self
                    .devices
                    .get(device_id)
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| "Unknown Device".to_string());
                let id = device_id.clone();

                info!(
                    "Launching SMS window for device={} name={}",
                    id, device_name
                );

                // Spawn in a thread so the process::Command doesn't block the executor.
                // The thread waits on the child so it doesn't become a zombie.
                std::thread::spawn(move || {
                    match std::process::Command::new("cosmic-ext-connect-sms")
                        .arg(&id)
                        .arg(&device_name)
                        .spawn()
                    {
                        Ok(mut child) => {
                            info!("cosmic-ext-connect-sms launched");
                            let _ = child.wait();
                        }
                        Err(e) => error!("Failed to launch cosmic-ext-connect-sms: {:?}", e),
                    }
                });
            }
            Message::PingDevice(ref device_id) => {
                let id = device_id.clone();
                return Task::perform(
                    async move {
                        backend::ping_device(id).await.ok();
                    },
                    |_| cosmic::Action::App(Message::RefreshDevices),
                );
            }
            Message::RingDevice(ref device_id) => {
                let id = device_id.clone();
                return Task::perform(
                    async move {
                        backend::ring_device(id).await.ok();
                    },
                    |_| cosmic::Action::App(Message::RefreshDevices),
                );
            }
            Message::BrowseDevice(ref device_id) => {
                let id = device_id.clone();
                return Task::perform(
                    async move { backend::browse_device_filesystem(id).await },
                    |result| match result {
                        Ok(()) => cosmic::Action::App(Message::RefreshDevices),
                        Err(e) => cosmic::Action::App(Message::BrowseDeviceFailed(e.to_string())),
                    },
                );
            }
            Message::UnmountDevice(ref device_id) => {
                let id = device_id.clone();
                return Task::perform(async move { backend::unmount_device(id).await }, |result| {
                    match result {
                        Ok(()) => cosmic::Action::App(Message::RefreshDevices),
                        Err(e) => cosmic::Action::App(Message::BrowseDeviceFailed(e.to_string())),
                    }
                });
            }
            Message::BrowseDeviceFailed(message) => {
                self.error_banner = Some(message);
                return Task::none();
            }
            Message::DismissError => {
                self.error_banner = None;
                return Task::none();
            }
            Message::PairDevice(ref device_id) => {
                let id = device_id.clone();
                return Task::perform(
                    async move {
                        backend::pair_device(id).await.ok();
                    },
                    |_| cosmic::Action::App(Message::RefreshDevices),
                );
            }
            Message::UnpairDevice(ref device_id) => {
                let id = device_id.clone();
                return Task::perform(
                    async move {
                        backend::unpair_device(id).await.ok();
                    },
                    |_| cosmic::Action::App(Message::RefreshDevices),
                );
            }
            Message::SendFiles(ref device_id) => {
                let id = device_id.clone();
                return Task::perform(
                    async move {
                        let files = portal::pick_files(&fl!("file-picker-title"), true, None).await;
                        if !files.is_empty() {
                            backend::send_files(id, files).await.ok();
                        }
                    },
                    |_| cosmic::Action::App(Message::RefreshDevices),
                );
            }
            Message::UpdateTransferProgress(progress) => {
                if let Some(ref current_device) = self.expanded_device {
                    if let Some(device) = self.devices.get_mut(current_device) {
                        device.share_progress = if progress < 100 { Some(progress) } else { None };
                    }
                }
            }
            Message::TransferStatusReceived(status) => {
                if let Some(existing) = self
                    .recent_transfers
                    .iter_mut()
                    .find(|s| s.transfer_id == status.transfer_id)
                {
                    *existing = status;
                } else {
                    self.recent_transfers.push_front(status);
                    while self.recent_transfers.len() > MAX_RECENT_TRANSFERS {
                        self.recent_transfers.pop_back();
                    }
                }
            }
            Message::ShareClipboard(ref device_id) => {
                let id = device_id.clone();
                let result_device_id = id.clone();
                return Task::perform(
                    async move {
                        backend::share_clipboard(id)
                            .await
                            .map_err(|e| e.to_string())
                    },
                    move |result| {
                        cosmic::Action::App(Message::ClipboardSendFinished {
                            device_id: result_device_id.clone(),
                            result,
                        })
                    },
                );
            }
            Message::ClipboardSendFinished { device_id, result } => match result {
                Ok(()) => debug!("Manual clipboard sent to {}", device_id),
                Err(e) => warn!("Manual clipboard send to {} failed: {}", device_id, e),
            },
            Message::BatteryUpdated(device_id, level, charging) => {
                if let Some(device) = self.devices.get_mut(&device_id) {
                    device.battery_level = Some(level);
                    device.is_charging = Some(charging);
                    // Also patch the backend cache so the next fetch_devices() preserves it
                    let d = device.clone();
                    tokio::spawn(async move {
                        backend::update_device(device_id, d).await;
                    });
                }
            }
            Message::ConnectivityUpdated(device_id, strength) => {
                if let Some(device) = self.devices.get_mut(&device_id) {
                    device.signal_strength = Some(strength);
                    let d = device.clone();
                    tokio::spawn(async move {
                        backend::update_device(device_id, d).await;
                    });
                }
            }
            Message::AcceptPairing(ref device_id) => {
                self.pairing_requests.remove(device_id);
                let id = device_id.clone();
                return Task::perform(
                    async move {
                        backend::accept_pairing(id).await.ok();
                    },
                    |_| cosmic::Action::App(Message::RefreshDevices),
                );
            }
            Message::RejectPairing(ref device_id) => {
                self.pairing_requests.remove(device_id);
                let id = device_id.clone();
                return Task::perform(
                    async move {
                        backend::reject_pairing(id).await.ok();
                    },
                    |_| cosmic::Action::App(Message::RefreshDevices),
                );
            }
            Message::PairingRequestReceived(device_id, device_name) => {
                info!(
                    "Pairing request received from {} ({})",
                    device_name, device_id
                );
                self.pairing_requests.insert(device_id, device_name.clone());

                // Show a system notification so the user is alerted even if they
                // are not looking at the panel. COSMIC's daemon doesn't support
                // action buttons so we just point them to the applet.
                let notif_body = format!(
                    "'{}' wants to pair with this device. Click the KDE Connect applet to accept or decline.",
                    device_name
                );
                tokio::task::spawn_blocking(move || {
                    let _ = notify_rust::Notification::new()
                        .appname("KDE Connect")
                        .summary(&fl!("notification-pairing-summary"))
                        .body(&notif_body)
                        .icon("network-wireless-symbolic")
                        .show();
                });
            }
            Message::MprisReceived(device_id, mpris_data) => {
                debug!("MPRIS from {}: {:?}", device_id, mpris_data);
            }
            Message::MprisSnapshot(snapshot) => {
                self.now_playing = snapshot;
            }
            Message::MprisPlayPause(ref bus_name) => {
                let bus_name = bus_name.clone();
                return Task::perform(
                    backend::mpris_control(bus_name, backend::MprisControlAction::PlayPause),
                    |_| cosmic::Action::App(Message::RefreshDevices),
                );
            }
            Message::MprisNext(ref bus_name) => {
                let bus_name = bus_name.clone();
                return Task::perform(
                    backend::mpris_control(bus_name, backend::MprisControlAction::Next),
                    |_| cosmic::Action::App(Message::RefreshDevices),
                );
            }
            Message::MprisPrevious(ref bus_name) => {
                let bus_name = bus_name.clone();
                return Task::perform(
                    backend::mpris_control(bus_name, backend::MprisControlAction::Previous),
                    |_| cosmic::Action::App(Message::RefreshDevices),
                );
            }
            Message::OpenSettings => {
                if let Ok(mut child) =
                    std::process::Command::new("cosmic-ext-connect-settings").spawn()
                {
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                }
            }
            Message::RemoteInput(ref device_id) => {
                debug!("Remote input: {}", device_id);
            }
            Message::LockDevice(ref device_id) => {
                debug!("Lock device: {}", device_id);
            }
            Message::PresenterMode(ref device_id) => {
                debug!("Presenter mode: {}", device_id);
            }
            Message::UseAsMonitor(ref device_id) => {
                debug!("Use as monitor: {}", device_id);
            }
            Message::ShareText(ref device_id) => {
                debug!("Share text: {}", device_id);
            }
            Message::ShareUrl(ref device_id) => {
                debug!("Share URL: {}", device_id);
            }
            Message::RequestRunCommands(ref device_id) => {
                let id = device_id.clone();
                return Task::perform(
                    async move {
                        backend::request_run_commands(id).await.ok();
                    },
                    |_| cosmic::Action::App(Message::RefreshDevices),
                );
            }
            Message::RunCommandsReceived(ref device_id, ref commands_json) => {
                let commands: Vec<(String, String)> =
                    serde_json::from_str::<Vec<serde_json::Value>>(commands_json)
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|v| {
                            let key = v["key"].as_str()?.to_string();
                            let name = v["name"].as_str()?.to_string();
                            Some((key, name))
                        })
                        .collect();
                if let Some(device) = self.devices.get_mut(device_id) {
                    device.run_commands = commands;
                    let d = device.clone();
                    let did = device_id.clone();
                    tokio::spawn(async move {
                        backend::update_device(did, d).await;
                    });
                }
            }
            Message::ExecuteRunCommand(ref device_id, ref key) => {
                let id = device_id.clone();
                let k = key.clone();
                return Task::perform(
                    async move {
                        backend::execute_run_command(id, k).await.ok();
                    },
                    |_| cosmic::Action::App(Message::RefreshDevices),
                );
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Self::Message> {
        self.core
            .applet
            .icon_button("phone-symbolic")
            .on_press(Message::TogglePopup)
            .into()
    }

    fn view_window(&self, id: SurfaceId) -> Element<'_, Self::Message> {
        let Some(popup_id) = self.popup else {
            return widget::text("").into();
        };
        if id != popup_id {
            return widget::text("").into();
        }
        ui::popup::create_popup_view(
            &self.core,
            &ui::popup::PopupState {
                devices: &self.devices,
                expanded_device: self.expanded_device.as_ref(),
                pairing_requests: Some(&self.pairing_requests),
                unread_sms: &self.unread_sms,
                error_banner: self.error_banner.as_ref(),
                now_playing: &self.now_playing,
                recent_transfers: &self.recent_transfers,
            },
        )
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        use futures::StreamExt as _;

        let mut subscriptions = vec![
            backend::filetransfer_subscription(),
            backend::service_watcher_subscription(),
            // D-Bus event stream — delivers pairing requests and device state
            // changes in real time without waiting for the poll below.
            Subscription::run(|| {
                async_stream::stream! {
                    let mut stream = backend::event_stream().await;
                    while let Some(event) = stream.next().await {
                        match event {
                            kdeconnect_dbus_client::ServiceEvent::PairingRequested(id, name) => {
                                yield Message::PairingRequestReceived(id, name);
                            }
                            kdeconnect_dbus_client::ServiceEvent::ClipboardReceived(_) => {}
                            kdeconnect_dbus_client::ServiceEvent::BatteryReceived(id, level, charging) => {
                                yield Message::BatteryUpdated(id, level, charging);
                            }
                            kdeconnect_dbus_client::ServiceEvent::ConnectivityReceived(id, strength) => {
                                yield Message::ConnectivityUpdated(id, strength);
                            }
                            kdeconnect_dbus_client::ServiceEvent::RunCommandListReceived(id, commands_json) => {
                                yield Message::RunCommandsReceived(id, commands_json);
                            }
                            kdeconnect_dbus_client::ServiceEvent::BrowseFailed(_id, message) => {
                                yield Message::BrowseDeviceFailed(message);
                            }
                            // Mount finished (or the share was unmounted,
                            // possibly straight from the file manager) —
                            // refetch so the Browse/Unmount buttons match.
                            kdeconnect_dbus_client::ServiceEvent::MountStateChanged(_, _) => {
                                yield Message::RefreshDevices;
                            }
                            kdeconnect_dbus_client::ServiceEvent::DeviceConnected(id, _)
                            | kdeconnect_dbus_client::ServiceEvent::DevicePaired(id, _)
                            | kdeconnect_dbus_client::ServiceEvent::DeviceDisconnected(id) => {
                                let _ = id;
                                yield Message::RefreshDevices;
                            }
                            _ => {}
                        }
                    }
                }
            }),
        ];

        // Polling is only useful while its results are on screen. With the
        // popup closed the applet is a static icon fed by the event stream
        // above, so don't wake up to poll devices, unread SMS, or MPRIS —
        // every such wakeup also wakes the panel that hosts us. Both
        // subscriptions poll immediately on popup open (TogglePopup fetches
        // devices/SMS, the MPRIS stream yields its first snapshot at once).
        if self.popup.is_some() {
            subscriptions.push(
                cosmic::iced::time::every(std::time::Duration::from_secs(10))
                    .map(|_| Message::RefreshDevices),
            );
            subscriptions.push(backend::mpris_subscription());
        }

        Subscription::batch(subscriptions)
    }
}

fn main() -> cosmic::iced::Result {
    use tracing_subscriber::prelude::*;

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));

    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    if std::env::var("KDECONNECT_LOG_FILE").is_ok_and(|v| !v.is_empty())
        && std::path::Path::new("/.flatpak-info").exists()
    {
        let log_dir = dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
        let _ = std::fs::create_dir_all(&log_dir);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("applet.log"))
            .expect("failed to open applet.log");
        let (non_blocking, _guard) = tracing_appender::non_blocking(file);
        let file_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(non_blocking);
        tracing_subscriber::registry()
            .with(env_filter)
            .with(stderr_layer)
            .with(file_layer)
            .init();
        std::mem::forget(_guard);
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(stderr_layer)
            .init();
    }

    ctrlc::set_handler(move || std::process::exit(0)).ok();

    // Spawn the service in the same process group so it exits when the session ends.
    // If the service is already running it exits immediately (D-Bus name already taken).
    // Explicitly forward HOME so the service reads config from the correct path
    // regardless of the environment the COSMIC panel provides.
    let home = std::env::var("HOME").unwrap_or_else(|_| {
        dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .to_string_lossy()
            .to_string()
    });

    // Retain the service's diagnostics instead of discarding them: route its
    // stdout/stderr into a bounded log file (one generation of rotation at
    // launch, capped at 1 MiB per generation). Transfer failures are only
    // diagnosable after the fact if these survive the session.
    let service_log = {
        let log_dir = dirs::data_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .join("kdeconnect");
        let _ = std::fs::create_dir_all(&log_dir);
        let log_path = log_dir.join("service.log");
        const MAX_LOG_BYTES: u64 = 1024 * 1024;
        if std::fs::metadata(&log_path)
            .map(|m| m.len() > MAX_LOG_BYTES)
            .unwrap_or(false)
        {
            let _ = std::fs::rename(&log_path, log_dir.join("service.log.1"));
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .ok()
    };

    let mut command = std::process::Command::new("kdeconnect-service");
    command
        .env("HOME", &home)
        .env(
            "XDG_RUNTIME_DIR",
            std::env::var("XDG_RUNTIME_DIR").unwrap_or_default(),
        )
        .env(
            "XDG_CONFIG_HOME",
            std::env::var("XDG_CONFIG_HOME").unwrap_or_default(),
        )
        // The service defaults to `warn`; keep `info`-level transfer
        // diagnostics in the retained log unless the user chose a level.
        .env("RUST_LOG", std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .stdin(std::process::Stdio::null());

    match service_log
        .as_ref()
        .and_then(|f| f.try_clone().ok().zip(f.try_clone().ok()))
    {
        Some((out, err)) => {
            command.stdout(std::process::Stdio::from(out));
            command.stderr(std::process::Stdio::from(err));
        }
        None => {
            command.stdout(std::process::Stdio::null());
            command.stderr(std::process::Stdio::null());
        }
    }

    if let Ok(mut child) = command.spawn()
    {
        // Reap the child process when it exits so it does not become a zombie.
        // If the service is already running it exits immediately (D-Bus name
        // already taken), but the kernel still requires the parent to wait on it.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }

    cosmic::applet::run::<KdeConnectApplet>(())
}
