use std::time::{Duration, Instant};

use tao::{
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget},
    platform::macos::{ActivationPolicy, EventLoopExtMacOS},
    window::WindowId,
};
use tray_icon::{
    MouseButton, MouseButtonState, TrayIcon, TrayIconEvent,
    menu::MenuEvent,
};

use crate::inspector::{self, FilterState, InspectorCommand, InspectorWindow};
use minimonitor_core::snapshot::{MonitorSnapshot, Sampler, SortMode};
use crate::tray;
use crate::util::percentage;

const REFRESH: Duration = Duration::from_secs(1);

pub enum UserEvent {
    Menu(MenuEvent),
    Tray(TrayIconEvent),
    Inspector(InspectorCommand),
}

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

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + REFRESH);
        match event {
            Event::NewEvents(StartCause::Init) => {
                state.refresh_live();
                state.ensure_tray();
                state.update_tray_title();
            }
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                state.refresh_live();
                state.update_tray_title();
            }
            Event::UserEvent(UserEvent::Tray(event)) => state.handle_tray(event),
            Event::UserEvent(UserEvent::Menu(event)) => state.handle_menu(event, target),
            Event::UserEvent(UserEvent::Inspector(cmd)) => state.handle_inspector_cmd(cmd, target),
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

struct AppState {
    proxy: EventLoopProxy<UserEvent>,
    sampler: Sampler,
    live: MonitorSnapshot,
    presentation: Option<MonitorSnapshot>,
    sort_mode: SortMode,
    tray: Option<TrayIcon>,
    status_message: Option<String>,
    inspector: Option<InspectorWindow>,
    filters: FilterState,
    caffeinate: crate::actions::Caffeinate,
}

impl AppState {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        let mut sampler = Sampler::new();
        let live = sampler.sample(SortMode::Cpu);
        Self {
            proxy,
            sampler,
            live,
            presentation: None,
            sort_mode: SortMode::Cpu,
            tray: None,
            status_message: None,
            inspector: None,
            filters: FilterState::default(),
            caffeinate: crate::actions::Caffeinate::new(),
        }
    }

    fn refresh_live(&mut self) {
        self.live = self.sampler.sample(self.sort_mode);
    }

    fn status_title(&self) -> String {
        let ram_percent = percentage(self.live.used_memory_bytes, self.live.total_memory_bytes);
        let mut title = format!(
            "{:.0}%C {:.0}%R",
            self.live.total_cpu_percent, ram_percent
        );
        if let Some(gpu) = self.live.gpu_percent {
            title.push_str(&format!(" {gpu:.0}%G"));
        }
        if self.live.ai_snapshot.workload_count > 0 {
            title.push_str(&format!(" AI{}", self.live.ai_snapshot.workload_count));
        }
        title
    }

    fn ensure_tray(&mut self) {
        if self.tray.is_some() {
            return;
        }
        let tray = tray::build_tray(&self.status_title());
        let _ = tray.set_menu(Some(Box::new(self.menu())));
        self.tray = Some(tray);
    }

    fn update_tray_title(&self) {
        if let Some(tray) = &self.tray {
            tray.set_title(Some(self.status_title()));
        }
    }

    fn refresh_tray_menu(&self) {
        if let Some(tray) = &self.tray {
            tray.set_menu(Some(Box::new(self.menu())));
        }
    }

    fn menu(&self) -> tray_icon::menu::Menu {
        let snapshot = self.presentation.as_ref().unwrap_or(&self.live);
        tray::build_menu(snapshot, self.sort_mode, self.status_message.as_deref())
    }

    fn handle_tray(&mut self, event: TrayIconEvent) {
        if let TrayIconEvent::Click {
            button,
            button_state,
            ..
        } = event
        {
            if matches!(button, MouseButton::Left | MouseButton::Right)
                && button_state == MouseButtonState::Down
            {
                self.presentation = Some(self.live.clone());
            }
        }
    }

    fn handle_menu(&mut self, event: MenuEvent, target: &EventLoopWindowTarget<UserEvent>) {
        let id = event.id.as_ref();
        match id {
            "show-inspector" => self.open_inspector(target),
            "refresh-menu" => {
                self.presentation = Some(self.live.clone());
                self.status_message = Some("Snapshot refreshed".to_owned());
            }
            "sort:cpu" => {
                self.sort_mode = SortMode::Cpu;
                self.refresh_live();
                self.presentation = Some(self.live.clone());
                self.status_message = Some("Sorting by CPU".to_owned());
            }
            "sort:ram" => {
                self.sort_mode = SortMode::Memory;
                self.refresh_live();
                self.presentation = Some(self.live.clone());
                self.status_message = Some("Sorting by RAM".to_owned());
            }
            "quit" => std::process::exit(0),
            "action:caffeinate" => {
                let on = self.caffeinate.set(!self.caffeinate.is_on());
                self.status_message = Some(
                    if on { "Keep-awake ON (caffeinate)" } else { "Keep-awake OFF" }.to_owned());
            }
            "action:flush-dns" => {
                self.status_message = Some(match crate::actions::flush_dns() {
                    Ok(()) => "Flushed DNS cache".to_owned(),
                    Err(e) => format!("Flush DNS failed: {e}"),
                });
            }
            _ if id.starts_with("kill:") => {
                if let Some(pid) = tray::parse_pid_suffix(id, "kill:") {
                    self.kill(pid);
                    self.refresh_live();
                    self.presentation = Some(self.live.clone());
                }
            }
            _ => {}
        }
        self.refresh_tray_menu();
        self.push_inspector();
    }

    fn handle_inspector_cmd(
        &mut self,
        cmd: InspectorCommand,
        target: &EventLoopWindowTarget<UserEvent>,
    ) {
        match cmd {
            InspectorCommand::Refresh => {
                self.presentation = Some(self.live.clone());
                self.status_message = Some("Snapshot refreshed".to_owned());
            }
            InspectorCommand::Close => {
                self.hide_inspector();
                return;
            }
            InspectorCommand::Kill { pid } => {
                self.kill(pid);
                self.refresh_live();
                self.presentation = Some(self.live.clone());
            }
            InspectorCommand::SetSort { value } => {
                self.sort_mode = if value.eq_ignore_ascii_case("ram") {
                    SortMode::Memory
                } else {
                    SortMode::Cpu
                };
                self.refresh_live();
                self.presentation = Some(self.live.clone());
            }
        }

        if self.inspector.is_none() {
            self.open_inspector(target);
        }
        self.refresh_tray_menu();
        self.push_inspector();
    }

    fn open_inspector(&mut self, target: &EventLoopWindowTarget<UserEvent>) {
        self.presentation = Some(self.live.clone());
        if let Some(inspector) = &self.inspector {
            inspector.window.set_visible(true);
            inspector.window.set_focus();
            self.push_inspector();
            return;
        }
        let inspector = inspector::open(target, self.proxy.clone());
        self.inspector = Some(inspector);
        self.push_inspector();
    }

    fn hide_inspector(&mut self) {
        if let Some(inspector) = &self.inspector {
            inspector.window.set_visible(false);
        }
    }

    fn inspector_window_id(&self) -> Option<WindowId> {
        self.inspector.as_ref().map(|i| i.window.id())
    }

    fn push_inspector(&self) {
        let Some(inspector) = &self.inspector else {
            return;
        };
        let snapshot = self.presentation.as_ref().unwrap_or(&self.live);
        let view = inspector::build_view(
            snapshot,
            &self.filters,
            self.status_message.clone(),
        );
        inspector::push_state(inspector, &view);
    }

    fn kill(&mut self, pid: u32) {
        self.status_message = Some(match self.sampler.kill(pid) {
            Some(true) => format!("Sent kill signal to PID {pid}"),
            Some(false) => format!("Failed to kill PID {pid}"),
            None => format!("PID {pid} not found"),
        });
    }
}
