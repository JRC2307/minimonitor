#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("MiniMonitor currently targets macOS only.");
}

#[cfg(target_os = "macos")]
fn main() {
    macos_app::run();
}

#[cfg(target_os = "macos")]
mod macos_app {
    use std::{
        collections::{HashMap, HashSet},
        ffi::OsString,
        process::Command,
        time::{Duration, Instant},
    };

    use keyring::use_native_store;
    use keyring_core::Entry;
    use reqwest::blocking::Client;
    use serde::{Deserialize, Serialize};
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System, UpdateKind, Users};
    use tao::{
        event::{Event, StartCause, WindowEvent},
        event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
        platform::macos::{ActivationPolicy, EventLoopExtMacOS},
        window::{Window, WindowBuilder, WindowId},
    };
    use tiktoken_rs::get_bpe_from_model;
    use tray_icon::{
        MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    };
    use wry::{WebView, WebViewBuilder, http::Request};

    const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
    const NETWORK_REFRESH_INTERVAL: Duration = Duration::from_secs(10);
    const MAX_MENU_PROCESSES: usize = 8;
    const MAX_INSPECTOR_PROCESSES: usize = 200;
    const SERVICE_NAME: &str = "com.caguabot.minimonitor";

    pub fn run() {
        let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
        event_loop.set_dock_visibility(false);
        event_loop.set_activate_ignoring_other_apps(false);

        let proxy = event_loop.create_proxy();
        MenuEvent::set_event_handler(Some({
            let proxy = proxy.clone();
            move |event| {
                let _ = proxy.send_event(UserEvent::Menu(event));
            }
        }));
        TrayIconEvent::set_event_handler(Some({
            let proxy = proxy.clone();
            move |event| {
                let _ = proxy.send_event(UserEvent::Tray(event));
            }
        }));

        let mut state = AppState::new(proxy);

        event_loop.run(move |event, event_loop_target, control_flow| {
            *control_flow = ControlFlow::WaitUntil(Instant::now() + REFRESH_INTERVAL);

            match event {
                Event::NewEvents(StartCause::Init) => {
                    state.refresh_live_snapshot(true);
                    state.ensure_tray();
                    state.update_tray_title();
                    state.prepare_tray_menu();
                }
                Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                    state.refresh_live_snapshot(false);
                    state.update_tray_title();
                }
                Event::UserEvent(UserEvent::Tray(event)) => {
                    state.handle_tray_event(event);
                }
                Event::UserEvent(UserEvent::Menu(event)) => {
                    state.handle_menu_event(event, event_loop_target);
                }
                Event::UserEvent(UserEvent::Inspector(command)) => {
                    state.handle_inspector_command(command, event_loop_target);
                }
                Event::WindowEvent {
                    window_id,
                    event: WindowEvent::CloseRequested,
                    ..
                } => {
                    if Some(window_id) == state.inspector_window_id() {
                        state.hide_inspector();
                    }
                }
                _ => {}
            }
        });
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SortMode {
        Cpu,
        Memory,
    }

    impl SortMode {
        fn label(self) -> &'static str {
            match self {
                Self::Cpu => "CPU",
                Self::Memory => "RAM",
            }
        }
    }

    #[derive(Clone, Serialize)]
    struct ProcessRow {
        pid: u32,
        name: String,
        cpu_percent: f32,
        memory_bytes: u64,
        current_user: bool,
        user_name: String,
        localhost: bool,
        command: String,
        ai_label: Option<String>,
        ai_category: Option<String>,
    }

    #[derive(Clone, Serialize)]
    struct AiWorkload {
        label: String,
        category: String,
        process_count: usize,
        total_cpu_percent: f32,
        total_memory_bytes: u64,
        example_command: String,
    }

    #[derive(Clone, Serialize)]
    struct AiSnapshot {
        workload_count: usize,
        total_cpu_percent: f32,
        total_memory_bytes: u64,
        top_workloads: Vec<AiWorkload>,
    }

    #[derive(Clone)]
    struct MonitorSnapshot {
        total_memory_bytes: u64,
        used_memory_bytes: u64,
        total_swap_bytes: u64,
        used_swap_bytes: u64,
        total_cpu_percent: f32,
        ai_snapshot: AiSnapshot,
        processes: Vec<ProcessRow>,
        sort_mode: SortMode,
        captured_at: String,
    }

    #[derive(Clone, Serialize)]
    struct ProviderState {
        provider: &'static str,
        connected: bool,
        configured: bool,
        status: String,
        requires_sign_in: bool,
    }

    #[derive(Clone, Serialize)]
    struct TokenEstimateResult {
        model: String,
        token_count: usize,
        mode: String,
    }

    #[derive(Serialize)]
    struct InspectorView {
        summary: SummaryView,
        filters: FilterView,
        processes: Vec<ProcessRow>,
        ai_workloads: Vec<AiWorkload>,
        providers: Vec<ProviderState>,
        token_result: Option<TokenEstimateResult>,
        status_message: Option<String>,
        captured_at: String,
    }

    #[derive(Serialize)]
    struct SummaryView {
        ram_percent: f32,
        cpu_percent: f32,
        ram_label: String,
        swap_label: String,
        ai_label: String,
    }

    #[derive(Clone, Serialize)]
    struct FilterView {
        current_user_only: bool,
        localhost_only: bool,
        sort_mode: &'static str,
    }

    #[derive(Deserialize)]
    #[serde(tag = "type", rename_all = "kebab-case")]
    enum InspectorCommand {
        Refresh,
        Close,
        Kill { pid: u32 },
        SetSort { value: String },
        SetProviderKey { provider: String, key: String },
        ClearProviderKey { provider: String },
        ValidateProvider { provider: String },
        EstimateTokens { model: String, text: String },
    }

    enum UserEvent {
        Menu(MenuEvent),
        Tray(TrayIconEvent),
        Inspector(InspectorCommand),
    }

    struct AppState {
        proxy: EventLoopProxy<UserEvent>,
        system: System,
        users: Users,
        live_snapshot: MonitorSnapshot,
        presentation_snapshot: Option<MonitorSnapshot>,
        last_network_refresh: Instant,
        localhost_pids: HashSet<u32>,
        sort_mode: SortMode,
        tray: Option<TrayIcon>,
        pending_kill: Option<ProcessRow>,
        status_message: Option<String>,
        inspector: Option<InspectorState>,
        filter_state: FilterState,
        token_result: Option<TokenEstimateResult>,
    }

    struct InspectorState {
        window: Window,
        webview: WebView,
    }

    #[derive(Clone)]
    struct FilterState {
        current_user_only: bool,
        localhost_only: bool,
    }

    impl AppState {
        fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
            let _ = use_native_store(false);
            let mut system = System::new();
            let users = Users::new_with_refreshed_list();
            let localhost_pids = collect_localhost_pids();
            let live_snapshot =
                refresh_monitor_snapshot(&mut system, &users, SortMode::Cpu, &localhost_pids);

            Self {
                proxy,
                system,
                users,
                live_snapshot,
                presentation_snapshot: None,
                last_network_refresh: Instant::now(),
                localhost_pids,
                sort_mode: SortMode::Cpu,
                tray: None,
                pending_kill: None,
                status_message: None,
                inspector: None,
                filter_state: FilterState {
                    current_user_only: true,
                    localhost_only: false,
                },
                token_result: None,
            }
        }

        fn refresh_live_snapshot(&mut self, force_network: bool) {
            if force_network || self.last_network_refresh.elapsed() >= NETWORK_REFRESH_INTERVAL {
                self.localhost_pids = collect_localhost_pids();
                self.users.refresh();
                self.last_network_refresh = Instant::now();
            }

            self.live_snapshot = refresh_monitor_snapshot(
                &mut self.system,
                &self.users,
                self.sort_mode,
                &self.localhost_pids,
            );

            if let Some(pid) = self.pending_kill.as_ref().map(|process| process.pid) {
                if !self
                    .live_snapshot
                    .processes
                    .iter()
                    .any(|process| process.pid == pid)
                {
                    self.pending_kill = None;
                    self.status_message = Some(format!("PID {} is no longer running", pid));
                }
            }
        }

        fn ensure_tray(&mut self) {
            if self.tray.is_some() {
                return;
            }

            let tray = TrayIconBuilder::new()
                .with_title(self.status_title())
                .with_tooltip("MiniMonitor")
                .with_menu(Box::new(self.build_menu()))
                .with_menu_on_left_click(true)
                .build()
                .expect("failed to create tray icon");
            self.tray = Some(tray);
        }

        fn update_tray_title(&self) {
            if let Some(tray) = &self.tray {
                tray.set_title(Some(self.status_title()));
            }
        }

        fn prepare_tray_menu(&self) {
            if let Some(tray) = &self.tray {
                tray.set_menu(Some(Box::new(self.build_menu())));
            }
        }

        fn status_title(&self) -> String {
            let ram_percent = percentage(
                self.live_snapshot.used_memory_bytes,
                self.live_snapshot.total_memory_bytes,
            );
            let base = format!(
                "{:.0}% RAM {:.0}% CPU",
                ram_percent, self.live_snapshot.total_cpu_percent
            );
            if self.live_snapshot.ai_snapshot.workload_count > 0 {
                format!("{base} AI{}", self.live_snapshot.ai_snapshot.workload_count)
            } else {
                base
            }
        }

        fn build_menu(&self) -> Menu {
            let snapshot = self
                .presentation_snapshot
                .as_ref()
                .unwrap_or(&self.live_snapshot);
            let menu = Menu::new();

            let summary = MenuItem::new(
                format!(
                    "RAM {} | CPU {:.0}%",
                    format_bytes_pair(snapshot.used_memory_bytes, snapshot.total_memory_bytes),
                    snapshot.total_cpu_percent
                ),
                false,
                None,
            );
            let swap = MenuItem::new(
                format!(
                    "Swap {}",
                    format_bytes_pair(snapshot.used_swap_bytes, snapshot.total_swap_bytes)
                ),
                false,
                None,
            );
            let ai_summary = MenuItem::new(
                if snapshot.ai_snapshot.workload_count == 0 {
                    "AI workloads none detected".to_owned()
                } else {
                    format!(
                        "AI {} workloads | {:.0}% CPU | {} RAM",
                        snapshot.ai_snapshot.workload_count,
                        snapshot.ai_snapshot.total_cpu_percent,
                        format_bytes(snapshot.ai_snapshot.total_memory_bytes)
                    )
                },
                false,
                None,
            );
            let captured = MenuItem::new(format!("Snapshot {}", snapshot.captured_at), false, None);
            let show_inspector = MenuItem::with_id("show-inspector", "Show Inspector", true, None);
            let refresh = MenuItem::with_id("refresh-menu", "Refresh snapshot", true, None);
            let sort_cpu = MenuItem::with_id(
                "sort:cpu",
                if self.sort_mode == SortMode::Cpu {
                    "Use CPU sort  •"
                } else {
                    "Use CPU sort"
                },
                true,
                None,
            );
            let sort_ram = MenuItem::with_id(
                "sort:ram",
                if self.sort_mode == SortMode::Memory {
                    "Use RAM sort  •"
                } else {
                    "Use RAM sort"
                },
                true,
                None,
            );
            let processes = self.build_processes_submenu(snapshot);
            let ai_workloads = self.build_ai_workloads_submenu(snapshot);
            let quit = MenuItem::with_id("quit", "Quit MiniMonitor", true, None);

            let sep1 = PredefinedMenuItem::separator();
            let sep2 = PredefinedMenuItem::separator();
            let _ = menu.append_items(&[
                &summary,
                &swap,
                &ai_summary,
                &captured,
                &sep1,
                &show_inspector,
                &refresh,
                &sort_cpu,
                &sort_ram,
                &ai_workloads,
                &processes,
            ]);

            if let Some(confirm) = self.build_confirm_submenu() {
                let _ = menu.append(&confirm);
            }

            if let Some(message) = &self.status_message {
                let sep = PredefinedMenuItem::separator();
                let item = MenuItem::new(message, false, None);
                let _ = menu.append(&sep);
                let _ = menu.append(&item);
            }

            let _ = menu.append(&sep2);
            let _ = menu.append(&quit);

            menu
        }

        fn build_ai_workloads_submenu(&self, snapshot: &MonitorSnapshot) -> Submenu {
            let submenu = Submenu::new("AI workloads", true);

            if snapshot.ai_snapshot.top_workloads.is_empty() {
                let empty = MenuItem::new("No AI runtimes or agent tools inferred", false, None);
                let _ = submenu.append(&empty);
                return submenu;
            }

            for workload in &snapshot.ai_snapshot.top_workloads {
                let child = Submenu::with_id(
                    format!("ai:{}", slugify(&workload.label)),
                    format!(
                        "{} | {:.0}% CPU | {}",
                        workload.label,
                        workload.total_cpu_percent,
                        format_bytes(workload.total_memory_bytes)
                    ),
                    true,
                );
                let category = MenuItem::new(format!("Kind {}", workload.category), false, None);
                let count =
                    MenuItem::new(format!("Processes {}", workload.process_count), false, None);
                let cmd = MenuItem::new(
                    format!("Cmd {}", truncate_name(&workload.example_command, 42)),
                    false,
                    None,
                );
                let _ = child.append_items(&[&category, &count, &cmd]);
                let _ = submenu.append(&child);
            }

            submenu
        }

        fn build_processes_submenu(&self, snapshot: &MonitorSnapshot) -> Submenu {
            let submenu = Submenu::new("Top processes", true);

            for process in snapshot.processes.iter().take(MAX_MENU_PROCESSES) {
                let child = Submenu::with_id(
                    format!("process:{}", process.pid),
                    format!(
                        "{} | {:>5.1}% CPU | {}",
                        truncate_name(&process.name, 24),
                        process.cpu_percent,
                        format_bytes(process.memory_bytes)
                    ),
                    true,
                );
                let pid = MenuItem::new(format!("PID {}", process.pid), false, None);
                let cpu = MenuItem::new(format!("CPU {:>5.1}%", process.cpu_percent), false, None);
                let ram = MenuItem::new(
                    format!("RAM {}", format_bytes(process.memory_bytes)),
                    false,
                    None,
                );
                let local = MenuItem::new(
                    if process.localhost {
                        "Localhost yes"
                    } else {
                        "Localhost no"
                    },
                    false,
                    None,
                );
                let kill = MenuItem::with_id(
                    format!("request-kill:{}", process.pid),
                    "Kill process...",
                    true,
                    None,
                );
                let sep = PredefinedMenuItem::separator();
                let _ = child.append_items(&[&pid, &cpu, &ram, &local, &sep, &kill]);
                let _ = submenu.append(&child);
            }

            submenu
        }

        fn build_confirm_submenu(&self) -> Option<Submenu> {
            let pending = self.pending_kill.as_ref()?;
            let submenu = Submenu::new("Confirm kill", true);
            let details = MenuItem::new(
                format!("{} (PID {})", truncate_name(&pending.name, 28), pending.pid),
                false,
                None,
            );
            let confirm = MenuItem::with_id("confirm-kill", "Confirm", true, None);
            let cancel = MenuItem::with_id("cancel-kill", "Cancel", true, None);
            let _ = submenu.append_items(&[&details, &confirm, &cancel]);
            Some(submenu)
        }

        fn handle_tray_event(&mut self, event: TrayIconEvent) {
            if let TrayIconEvent::Click {
                button,
                button_state,
                ..
            } = event
            {
                if matches!(button, MouseButton::Left | MouseButton::Right)
                    && button_state == MouseButtonState::Down
                {
                    self.presentation_snapshot = Some(self.live_snapshot.clone());
                    self.prepare_tray_menu();
                }
            }
        }

        fn handle_menu_event(
            &mut self,
            event: MenuEvent,
            event_loop_target: &tao::event_loop::EventLoopWindowTarget<UserEvent>,
        ) {
            let id = event.id.as_ref();

            match id {
                "show-inspector" => {
                    self.open_inspector(event_loop_target);
                }
                "refresh-menu" => {
                    self.presentation_snapshot = Some(self.live_snapshot.clone());
                    self.status_message = Some("Snapshot refreshed".to_owned());
                    self.prepare_tray_menu();
                    self.push_inspector_state();
                }
                "sort:cpu" => {
                    self.sort_mode = SortMode::Cpu;
                    self.refresh_live_snapshot(true);
                    self.presentation_snapshot = Some(self.live_snapshot.clone());
                    self.status_message = Some("Sorting by CPU".to_owned());
                    self.prepare_tray_menu();
                    self.push_inspector_state();
                }
                "sort:ram" => {
                    self.sort_mode = SortMode::Memory;
                    self.refresh_live_snapshot(true);
                    self.presentation_snapshot = Some(self.live_snapshot.clone());
                    self.status_message = Some("Sorting by RAM".to_owned());
                    self.prepare_tray_menu();
                    self.push_inspector_state();
                }
                "confirm-kill" => {
                    if let Some(process) = self.pending_kill.clone() {
                        self.kill_pid(Pid::from_u32(process.pid));
                    }
                    self.pending_kill = None;
                    self.refresh_live_snapshot(true);
                    self.presentation_snapshot = Some(self.live_snapshot.clone());
                    self.prepare_tray_menu();
                    self.push_inspector_state();
                }
                "cancel-kill" => {
                    self.pending_kill = None;
                    self.status_message = Some("Kill cancelled".to_owned());
                    self.prepare_tray_menu();
                }
                "quit" => std::process::exit(0),
                _ if id.starts_with("request-kill:") => {
                    if let Some(pid) = parse_pid_suffix(id, "request-kill:") {
                        self.pending_kill = self.lookup_process(pid);
                        self.status_message = self.pending_kill.as_ref().map(|process| {
                            format!(
                                "Confirm killing {} ({})",
                                truncate_name(&process.name, 20),
                                process.pid
                            )
                        });
                        self.prepare_tray_menu();
                    }
                }
                _ => {}
            }
        }

        fn handle_inspector_command(
            &mut self,
            command: InspectorCommand,
            event_loop_target: &tao::event_loop::EventLoopWindowTarget<UserEvent>,
        ) {
            match command {
                InspectorCommand::Refresh => {
                    self.presentation_snapshot = Some(self.live_snapshot.clone());
                    self.status_message = Some("Inspector snapshot refreshed".to_owned());
                }
                InspectorCommand::Close => {
                    self.hide_inspector();
                    return;
                }
                InspectorCommand::Kill { pid } => {
                    self.kill_pid(Pid::from_u32(pid));
                    self.refresh_live_snapshot(true);
                    self.presentation_snapshot = Some(self.live_snapshot.clone());
                }
                InspectorCommand::SetSort { value } => {
                    self.sort_mode = if value.eq_ignore_ascii_case("ram") {
                        SortMode::Memory
                    } else {
                        SortMode::Cpu
                    };
                    self.refresh_live_snapshot(true);
                    self.presentation_snapshot = Some(self.live_snapshot.clone());
                }
                InspectorCommand::SetProviderKey { provider, key } => {
                    self.set_provider_key(&provider, &key);
                }
                InspectorCommand::ClearProviderKey { provider } => {
                    self.clear_provider_key(&provider);
                }
                InspectorCommand::ValidateProvider { provider } => {
                    self.validate_provider(&provider);
                }
                InspectorCommand::EstimateTokens { model, text } => {
                    self.token_result = Some(estimate_tokens(&model, &text));
                    self.status_message = Some(format!("Estimated tokens for {}", model));
                }
            }

            if self.inspector.is_none() {
                self.open_inspector(event_loop_target);
            }
            self.push_inspector_state();
        }

        fn open_inspector(
            &mut self,
            event_loop_target: &tao::event_loop::EventLoopWindowTarget<UserEvent>,
        ) {
            self.presentation_snapshot = Some(self.live_snapshot.clone());

            if let Some(inspector) = &self.inspector {
                inspector.window.set_visible(true);
                inspector.window.set_focus();
                self.push_inspector_state();
                return;
            }

            let window = WindowBuilder::new()
                .with_title("MiniMonitor Inspector")
                .with_inner_size(tao::dpi::LogicalSize::new(1120.0, 760.0))
                .build(event_loop_target)
                .expect("failed to build inspector window");
            window.set_visible(true);

            let html = inspector_shell_html();
            let proxy = self.proxy.clone();
            let webview = WebViewBuilder::new()
                .with_html(html)
                .with_ipc_handler(move |request: Request<String>| {
                    if let Ok(command) = serde_json::from_str::<InspectorCommand>(request.body()) {
                        let _ = proxy.send_event(UserEvent::Inspector(command));
                    }
                })
                .build(&window)
                .expect("failed to build inspector webview");

            self.inspector = Some(InspectorState { window, webview });
            self.push_inspector_state();
        }

        fn hide_inspector(&mut self) {
            if let Some(inspector) = &self.inspector {
                inspector.window.set_visible(false);
            }
        }

        fn inspector_window_id(&self) -> Option<WindowId> {
            self.inspector
                .as_ref()
                .map(|inspector| inspector.window.id())
        }

        fn push_inspector_state(&mut self) {
            let Some(inspector) = &self.inspector else {
                return;
            };

            let payload =
                serde_json::to_string(&self.inspector_view()).unwrap_or_else(|_| "{}".to_owned());
            let escaped = serde_json::to_string(&payload).unwrap();
            let script = format!("window.updateFromRust(JSON.parse({escaped}));");
            let _ = inspector.webview.evaluate_script(&script);
        }

        fn inspector_view(&self) -> InspectorView {
            let snapshot = self
                .presentation_snapshot
                .as_ref()
                .unwrap_or(&self.live_snapshot);
            let processes = snapshot
                .processes
                .iter()
                .filter(|process| !self.filter_state.current_user_only || process.current_user)
                .filter(|process| !self.filter_state.localhost_only || process.localhost)
                .take(MAX_INSPECTOR_PROCESSES)
                .cloned()
                .collect::<Vec<_>>();

            let providers = ["openai", "anthropic"]
                .iter()
                .map(|provider| self.provider_state(provider))
                .collect::<Vec<_>>();

            InspectorView {
                summary: SummaryView {
                    ram_percent: percentage(
                        snapshot.used_memory_bytes,
                        snapshot.total_memory_bytes,
                    ),
                    cpu_percent: snapshot.total_cpu_percent,
                    ram_label: format_bytes_pair(
                        snapshot.used_memory_bytes,
                        snapshot.total_memory_bytes,
                    ),
                    swap_label: format_bytes_pair(
                        snapshot.used_swap_bytes,
                        snapshot.total_swap_bytes,
                    ),
                    ai_label: if snapshot.ai_snapshot.workload_count == 0 {
                        "No inferred AI workloads".to_owned()
                    } else {
                        format!(
                            "{} AI workloads, {:.0}% CPU, {} RAM",
                            snapshot.ai_snapshot.workload_count,
                            snapshot.ai_snapshot.total_cpu_percent,
                            format_bytes(snapshot.ai_snapshot.total_memory_bytes)
                        )
                    },
                },
                filters: FilterView {
                    current_user_only: self.filter_state.current_user_only,
                    localhost_only: self.filter_state.localhost_only,
                    sort_mode: snapshot.sort_mode.label(),
                },
                processes,
                ai_workloads: snapshot.ai_snapshot.top_workloads.clone(),
                providers,
                token_result: self.token_result.clone(),
                status_message: self.status_message.clone(),
                captured_at: snapshot.captured_at.clone(),
            }
        }

        fn set_provider_key(&mut self, provider: &str, key: &str) {
            let provider = normalize_provider(provider);
            let entry = match Entry::new(SERVICE_NAME, provider) {
                Ok(entry) => entry,
                Err(error) => {
                    self.status_message = Some(format!("Keychain unavailable: {error}"));
                    return;
                }
            };

            match entry.set_password(key) {
                Ok(()) => {
                    self.status_message = Some(format!("Stored {} key in Keychain", provider));
                }
                Err(error) => {
                    self.status_message =
                        Some(format!("Failed to store {} key: {error}", provider));
                }
            }
        }

        fn clear_provider_key(&mut self, provider: &str) {
            let provider = normalize_provider(provider);
            match Entry::new(SERVICE_NAME, provider).and_then(|entry| entry.delete_credential()) {
                Ok(()) => {
                    self.status_message = Some(format!("Removed {} key", provider));
                }
                Err(error) => {
                    self.status_message =
                        Some(format!("Failed to remove {} key: {error}", provider));
                }
            }
        }

        fn validate_provider(&mut self, provider: &str) {
            let provider = normalize_provider(provider);
            let Some(api_key) = provider_key(provider) else {
                self.status_message = Some(format!("No {} key configured", provider));
                return;
            };

            let client = match Client::builder().timeout(Duration::from_secs(8)).build() {
                Ok(client) => client,
                Err(error) => {
                    self.status_message = Some(format!("HTTP client error: {error}"));
                    return;
                }
            };

            let response = match provider {
                "openai" => client
                    .get("https://api.openai.com/v1/models")
                    .bearer_auth(api_key)
                    .send(),
                "anthropic" => client
                    .get("https://api.anthropic.com/v1/models")
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01")
                    .send(),
                _ => return,
            };

            self.status_message = Some(match response {
                Ok(resp) if resp.status().is_success() => {
                    format!("{provider} connected, usage/quota endpoint unavailable in v1")
                }
                Ok(resp) => format!("{provider} validation failed with {}", resp.status()),
                Err(error) => format!("{provider} validation error: {error}"),
            });
        }

        fn provider_state(&self, provider: &'static str) -> ProviderState {
            let configured = provider_key(provider).is_some();
            let status = if configured {
                "Key stored in Keychain".to_owned()
            } else {
                "Disconnected".to_owned()
            };

            ProviderState {
                provider,
                connected: configured,
                configured,
                status,
                requires_sign_in: true,
            }
        }

        fn kill_pid(&mut self, pid: Pid) {
            let result = self.system.process(pid).map(|process| {
                process
                    .kill_with(Signal::Term)
                    .unwrap_or_else(|| process.kill())
            });

            self.status_message = Some(match result {
                Some(true) => format!("Sent kill signal to PID {pid}"),
                Some(false) => format!("Failed to kill PID {pid}"),
                None => format!("PID {pid} not found"),
            });
        }

        fn lookup_process(&self, pid: Pid) -> Option<ProcessRow> {
            self.live_snapshot
                .processes
                .iter()
                .find(|process| process.pid == pid.as_u32())
                .cloned()
        }
    }

    fn refresh_monitor_snapshot(
        system: &mut System,
        users: &Users,
        sort_mode: SortMode,
        localhost_pids: &HashSet<u32>,
    ) -> MonitorSnapshot {
        system.refresh_memory();
        system.refresh_cpu_usage();
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_cmd(UpdateKind::OnlyIfNotSet)
                .with_exe(UpdateKind::OnlyIfNotSet),
        );

        let current_user = std::env::var("USER").unwrap_or_default();
        let ai_snapshot = build_ai_snapshot(system);
        let mut processes = system
            .processes()
            .iter()
            .map(|(pid, process)| {
                let command = os_strings_to_string(process.cmd());
                let user_name = process
                    .user_id()
                    .and_then(|user_id| users.get_user_by_id(user_id))
                    .map(|user| user.name().to_owned())
                    .unwrap_or_else(|| "-".to_owned());
                let ai_match = detect_ai_workload(&process.name().to_string_lossy(), &command);

                ProcessRow {
                    pid: pid.as_u32(),
                    name: process.name().to_string_lossy().into_owned(),
                    cpu_percent: process.cpu_usage(),
                    memory_bytes: process.memory(),
                    current_user: !current_user.is_empty() && user_name == current_user,
                    user_name,
                    localhost: localhost_pids.contains(&pid.as_u32()),
                    command,
                    ai_label: ai_match.map(|(label, _)| label.to_owned()),
                    ai_category: ai_match.map(|(_, category)| category.to_owned()),
                }
            })
            .collect::<Vec<_>>();

        match sort_mode {
            SortMode::Cpu => processes.sort_by(|a, b| {
                b.cpu_percent
                    .partial_cmp(&a.cpu_percent)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.memory_bytes.cmp(&a.memory_bytes))
            }),
            SortMode::Memory => processes.sort_by(|a, b| {
                b.memory_bytes.cmp(&a.memory_bytes).then_with(|| {
                    b.cpu_percent
                        .partial_cmp(&a.cpu_percent)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            }),
        }

        MonitorSnapshot {
            total_memory_bytes: system.total_memory(),
            used_memory_bytes: system.used_memory(),
            total_swap_bytes: system.total_swap(),
            used_swap_bytes: system.used_swap(),
            total_cpu_percent: system.global_cpu_usage(),
            ai_snapshot,
            processes,
            sort_mode,
            captured_at: capture_label(),
        }
    }

    fn build_ai_snapshot(system: &System) -> AiSnapshot {
        let mut groups = HashMap::<String, AiWorkload>::new();

        for process in system.processes().values() {
            let command = os_strings_to_string(process.cmd());
            let name = process.name().to_string_lossy().into_owned();
            let Some((label, category)) = detect_ai_workload(&name, &command) else {
                continue;
            };

            let key = format!("{category}:{label}");
            let entry = groups.entry(key).or_insert_with(|| AiWorkload {
                label: label.to_owned(),
                category: category.to_owned(),
                process_count: 0,
                total_cpu_percent: 0.0,
                total_memory_bytes: 0,
                example_command: command.clone(),
            });
            entry.process_count += 1;
            entry.total_cpu_percent += process.cpu_usage();
            entry.total_memory_bytes += process.memory();
            if entry.example_command.is_empty() && !command.is_empty() {
                entry.example_command = command.clone();
            }
        }

        let mut top_workloads = groups.into_values().collect::<Vec<_>>();
        top_workloads.sort_by(|a, b| {
            b.total_cpu_percent
                .partial_cmp(&a.total_cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.total_memory_bytes.cmp(&a.total_memory_bytes))
        });
        let workload_count = top_workloads.len();
        let total_cpu_percent = top_workloads
            .iter()
            .map(|workload| workload.total_cpu_percent)
            .sum();
        let total_memory_bytes = top_workloads
            .iter()
            .map(|workload| workload.total_memory_bytes)
            .sum();
        top_workloads.truncate(6);

        AiSnapshot {
            workload_count,
            total_cpu_percent,
            total_memory_bytes,
            top_workloads,
        }
    }

    fn collect_localhost_pids() -> HashSet<u32> {
        let output = Command::new("lsof")
            .args(["-nP", "-iTCP", "-sTCP:LISTEN"])
            .output();

        let Ok(output) = output else {
            return HashSet::new();
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .skip(1)
            .filter_map(|line| {
                let parts = line.split_whitespace().collect::<Vec<_>>();
                if parts.len() < 9 {
                    return None;
                }
                let address = parts.last().copied().unwrap_or_default();
                if !(address.contains("127.0.0.1") || address.contains("[::1]")) {
                    return None;
                }
                parts.get(1)?.parse::<u32>().ok()
            })
            .collect()
    }

    fn estimate_tokens(model: &str, text: &str) -> TokenEstimateResult {
        let model = if model.trim().is_empty() {
            "gpt-4o-mini".to_owned()
        } else {
            model.to_owned()
        };

        if model.to_ascii_lowercase().contains("claude") {
            TokenEstimateResult {
                model,
                token_count: anthropic_estimate(text),
                mode: "estimated".to_owned(),
            }
        } else {
            let token_count = get_bpe_from_model(&model)
                .map(|bpe| bpe.encode_with_special_tokens(text).len())
                .unwrap_or_else(|_| anthropic_estimate(text));
            TokenEstimateResult {
                model,
                token_count,
                mode: "openai-compatible".to_owned(),
            }
        }
    }

    fn anthropic_estimate(text: &str) -> usize {
        let chars = text.chars().count();
        (chars / 4).max(1)
    }

    fn provider_key(provider: &str) -> Option<String> {
        Entry::new(SERVICE_NAME, provider)
            .ok()
            .and_then(|entry| entry.get_password().ok())
            .filter(|value| !value.is_empty())
    }

    fn normalize_provider(provider: &str) -> &'static str {
        if provider.eq_ignore_ascii_case("anthropic") {
            "anthropic"
        } else {
            "openai"
        }
    }

    fn percentage(used: u64, total: u64) -> f32 {
        if total == 0 {
            0.0
        } else {
            used as f32 / total as f32 * 100.0
        }
    }

    fn format_bytes_pair(used: u64, total: u64) -> String {
        format!("{} / {}", format_bytes(used), format_bytes(total))
    }

    fn format_bytes(bytes: u64) -> String {
        const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
        let mut value = bytes as f64;
        let mut unit_index = 0;

        while value >= 1024.0 && unit_index < UNITS.len() - 1 {
            value /= 1024.0;
            unit_index += 1;
        }

        if unit_index == 0 {
            format!("{bytes} {}", UNITS[unit_index])
        } else {
            format!("{value:.1} {}", UNITS[unit_index])
        }
    }

    fn capture_label() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        format!("epoch {}", now)
    }

    fn parse_pid_suffix(id: &str, prefix: &str) -> Option<Pid> {
        id.strip_prefix(prefix)?
            .parse::<u32>()
            .ok()
            .map(Pid::from_u32)
    }

    fn os_strings_to_string(parts: &[OsString]) -> String {
        parts
            .iter()
            .map(|part| part.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn detect_ai_workload(
        process_name: &str,
        cmdline: &str,
    ) -> Option<(&'static str, &'static str)> {
        let haystack = format!(
            "{} {}",
            process_name.to_ascii_lowercase(),
            cmdline.to_ascii_lowercase()
        );
        const RULES: [(&str, &str, &str); 20] = [
            ("openclaw", "OpenClaw", "agent"),
            ("ollama", "Ollama", "model-runtime"),
            ("llama.cpp", "llama.cpp", "model-runtime"),
            ("llamacpp", "llama.cpp", "model-runtime"),
            ("vllm", "vLLM", "model-runtime"),
            ("lm studio", "LM Studio", "model-runtime"),
            ("lm-studio", "LM Studio", "model-runtime"),
            ("mlx", "MLX", "model-runtime"),
            ("open-webui", "Open WebUI", "ai-ui"),
            ("anythingllm", "AnythingLLM", "ai-ui"),
            ("comfyui", "ComfyUI", "image-pipeline"),
            ("automatic1111", "Automatic1111", "image-pipeline"),
            ("invokeai", "InvokeAI", "image-pipeline"),
            ("whisper", "Whisper", "speech"),
            ("cursor", "Cursor", "agent-tool"),
            ("cline", "Cline", "agent-tool"),
            ("aider", "Aider", "agent-tool"),
            ("codex", "Codex", "agent-tool"),
            ("continue", "Continue", "agent-tool"),
            ("claude", "Claude", "agent-tool"),
        ];

        for (needle, label, category) in RULES {
            if haystack.contains(needle) {
                return Some((label, category));
            }
        }
        None
    }

    fn truncate_name(value: &str, max_chars: usize) -> String {
        let count = value.chars().count();
        if count <= max_chars {
            return value.to_owned();
        }
        let truncated = value
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>();
        format!("{truncated}…")
    }

    fn slugify(value: &str) -> String {
        value
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect()
    }

    fn inspector_shell_html() -> &'static str {
        r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8" />
  <title>MiniMonitor Inspector</title>
  <style>
    :root { color-scheme: dark; --bg:#0d1014; --panel:#151a20; --line:#27313b; --text:#edf1f6; --muted:#9aa8b9; --good:#55c27b; --warm:#ffb86c; --bad:#ff6b6b; }
    body { margin:0; font:13px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace; background:radial-gradient(circle at top,#1d2630 0%, #0d1014 55%); color:var(--text); }
    .app { padding:18px; display:grid; gap:14px; }
    .row { display:flex; gap:10px; align-items:center; flex-wrap:wrap; }
    .panel { background:rgba(21,26,32,.92); border:1px solid var(--line); border-radius:14px; padding:14px; box-shadow:0 10px 30px rgba(0,0,0,.18); }
    input, textarea, select, button { background:#0f1419; color:var(--text); border:1px solid #32404e; border-radius:10px; padding:8px 10px; font:inherit; }
    textarea { width:100%; min-height:120px; }
    button { cursor:pointer; }
    button.danger { border-color:#6a3434; color:#ff9e9e; }
    .stats { display:grid; grid-template-columns:repeat(4,minmax(0,1fr)); gap:10px; }
    .stat { background:#0f1419; border:1px solid #24303b; border-radius:12px; padding:10px; }
    .label { color:var(--muted); font-size:11px; text-transform:uppercase; letter-spacing:.08em; }
    .value { font-size:18px; margin-top:4px; }
    .muted { color:var(--muted); }
    table { width:100%; border-collapse:collapse; }
    th, td { text-align:left; padding:8px 6px; border-bottom:1px solid #1f2831; vertical-align:top; }
    th { color:var(--muted); font-size:11px; text-transform:uppercase; }
    .pill { display:inline-block; padding:2px 7px; border-radius:999px; border:1px solid #354556; color:var(--muted); font-size:11px; margin-right:6px; }
    .split { display:grid; grid-template-columns:1.4fr .9fr; gap:14px; }
  </style>
</head>
<body>
  <div class="app">
    <div class="row">
      <button onclick="send({type:'refresh'})">Refresh Snapshot</button>
      <button onclick="send({type:'close'})">Hide Inspector</button>
      <input id="search" placeholder="Search pid or process" style="min-width:260px" oninput="render()" />
      <label class="muted"><input id="current-user" type="checkbox" checked onchange="render()" /> current user</label>
      <label class="muted"><input id="localhost-only" type="checkbox" onchange="render()" /> localhost only</label>
      <select id="sort" onchange="send({type:'set-sort', value:this.value})"><option value="CPU">CPU</option><option value="RAM">RAM</option></select>
    </div>
    <div id="stats" class="stats"></div>
    <div class="split">
      <div class="panel">
        <div class="row" style="justify-content:space-between"><strong>Processes</strong><span id="capture" class="muted"></span></div>
        <table>
          <thead><tr><th>Process</th><th>PID</th><th>CPU</th><th>RAM</th><th>Flags</th><th></th></tr></thead>
          <tbody id="process-rows"></tbody>
        </table>
      </div>
      <div style="display:grid; gap:14px;">
        <div class="panel"><strong>AI Workloads</strong><div id="ai-workloads" class="muted" style="margin-top:10px"></div></div>
        <div class="panel">
          <strong>Token Checker</strong>
          <div class="row" style="margin-top:10px">
            <select id="token-model"><option>gpt-4o-mini</option><option>gpt-4o</option><option>claude-3-5-sonnet-latest</option><option>claude-3-7-sonnet-latest</option></select>
            <button onclick="estimateTokens()">Estimate</button>
          </div>
          <textarea id="token-input" placeholder="Paste prompt, log, or copied process text"></textarea>
          <div id="token-result" class="muted"></div>
        </div>
        <div class="panel">
          <strong>Provider Keys</strong>
          <div id="providers" class="muted" style="margin-top:10px"></div>
        </div>
        <div class="panel">
          <strong>Status</strong>
          <div id="status" class="muted" style="margin-top:10px"></div>
        </div>
      </div>
    </div>
  </div>
  <script>
    const state = { data: null };
    function send(payload){ window.ipc.postMessage(JSON.stringify(payload)); }
    function estimateTokens(){ send({ type:'estimate-tokens', model:document.getElementById('token-model').value, text:document.getElementById('token-input').value }); }
    function saveProvider(provider){
      const input = document.getElementById(`provider-${provider}`);
      send({ type:'set-provider-key', provider, key: input.value });
      input.value = '';
    }
    function validateProvider(provider){ send({ type:'validate-provider', provider }); }
    function clearProvider(provider){ send({ type:'clear-provider-key', provider }); }
    function escapeHtml(v){ return String(v ?? '').replaceAll('&','&amp;').replaceAll('<','&lt;').replaceAll('>','&gt;'); }
    function render(){
      if(!state.data) return;
      document.getElementById('capture').textContent = `Snapshot ${state.data.captured_at}`;
      document.getElementById('sort').value = state.data.filters.sort_mode;
      const stats = [
        ['RAM', `${state.data.summary.ram_label} (${state.data.summary.ram_percent.toFixed(0)}%)`],
        ['CPU', `${state.data.summary.cpu_percent.toFixed(0)}%`],
        ['Swap', state.data.summary.swap_label],
        ['AI', state.data.summary.ai_label]
      ];
      document.getElementById('stats').innerHTML = stats.map(([label,value]) => `<div class="stat"><div class="label">${label}</div><div class="value">${escapeHtml(value)}</div></div>`).join('');
      const q = document.getElementById('search').value.toLowerCase();
      const userOnly = document.getElementById('current-user').checked;
      const localhostOnly = document.getElementById('localhost-only').checked;
      const rows = state.data.processes.filter(p => (!userOnly || p.current_user) && (!localhostOnly || p.localhost) && (!q || p.name.toLowerCase().includes(q) || String(p.pid).includes(q) || p.command.toLowerCase().includes(q)));
      document.getElementById('process-rows').innerHTML = rows.map(p => `
        <tr>
          <td><div>${escapeHtml(p.name)}</div><div class="muted">${escapeHtml(p.command || '')}</div></td>
          <td>${p.pid}</td>
          <td>${p.cpu_percent.toFixed(1)}%</td>
          <td>${escapeHtml(formatBytes(p.memory_bytes))}</td>
          <td>${p.current_user ? '<span class="pill">user</span>' : ''}${p.localhost ? '<span class="pill">localhost</span>' : ''}${p.ai_label ? `<span class="pill">${escapeHtml(p.ai_label)}</span>` : ''}</td>
          <td><button class="danger" onclick="if(confirm('Kill PID '+${p.pid}+'?')) send({type:'kill', pid:${p.pid}})">Kill</button></td>
        </tr>`).join('');
      document.getElementById('ai-workloads').innerHTML = (state.data.ai_workloads.length ? state.data.ai_workloads.map(w => `<div style="margin-bottom:10px"><strong>${escapeHtml(w.label)}</strong><div class="muted">${escapeHtml(w.category)} · ${w.process_count} proc · ${w.total_cpu_percent.toFixed(0)}% CPU · ${escapeHtml(formatBytes(w.total_memory_bytes))}</div><div class="muted">${escapeHtml(w.example_command)}</div></div>`).join('') : 'No inferred AI workloads');
      document.getElementById('providers').innerHTML = state.data.providers.map(p => `
        <div style="margin-bottom:12px">
          <div><strong>${escapeHtml(p.provider)}</strong> <span class="muted">${escapeHtml(p.status)}</span></div>
          <div class="row" style="margin-top:6px">
            <input id="provider-${p.provider}" placeholder="${p.provider} API key" style="min-width:220px" />
            <button onclick="saveProvider('${p.provider}')">Save</button>
            <button onclick="validateProvider('${p.provider}')">Validate</button>
            <button class="danger" onclick="clearProvider('${p.provider}')">Clear</button>
          </div>
        </div>`).join('');
      document.getElementById('token-result').textContent = state.data.token_result ? `${state.data.token_result.token_count} tokens (${state.data.token_result.mode}) for ${state.data.token_result.model}` : 'Local estimation requires no sign-in. Provider checks require stored API keys.';
      document.getElementById('status').textContent = state.data.status_message || 'Live tray metrics continue updating while this window stays on a frozen snapshot until you refresh.';
    }
    function formatBytes(bytes){
      const units=['B','KB','MB','GB','TB']; let value=bytes; let i=0;
      while(value >= 1024 && i < units.length - 1){ value /= 1024; i++; }
      return i === 0 ? `${value} ${units[i]}` : `${value.toFixed(1)} ${units[i]}`;
    }
    window.updateFromRust = function(next){ state.data = next; render(); };
  </script>
</body>
</html>"#
    }
}
