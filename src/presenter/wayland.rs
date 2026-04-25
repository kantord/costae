use std::sync::mpsc;

use costae::presentation::{PanelCommand, PresentationThread, PresenterEvent};
use costae::windowing::wayland::WaylandDisplayServer;
use costae::windowing::{DisplayServer, WindowEvent};
use costae::display_manager::DisplayManager;

use super::drain_commands;

fn apply_wayland_cmd(
    pt: &mut PresentationThread<WaylandDisplayServer>,
    cmd: PanelCommand,
) {
    match cmd {
        PanelCommand::RenderAll => {
            let PresentationThread { ref mut dm, ref mut presenter } = pt;
            presenter.flush_pixels(dm);
            dm.flush();
        }
        PanelCommand::UpdateOutputMap { .. } => {}
        PanelCommand::Shutdown => {}
        cmd => {
            let PresentationThread { ref mut dm, ref mut presenter } = pt;
            if let Err(e) = presenter.apply(cmd, dm) {
                tracing::error!(error = %e, "wayland presenter apply failed");
            }
        }
    }
}

pub(crate) fn run_wayland_presenter_thread(
    mut pt: PresentationThread<WaylandDisplayServer>,
    command_rx: mpsc::Receiver<PanelCommand>,
    event_tx: mpsc::Sender<PresenterEvent>,
) {
    loop {
        if drain_commands(&command_rx, |cmd| apply_wayland_cmd(&mut pt, cmd)) { return; }

        for (surface_id, new_size) in pt.dm.take_pending_configures() {
            for panel in pt.presenter.panels.values_mut() {
                if panel.surface_id != surface_id { continue; }
                if new_size.0 > 0 { panel.width = new_size.0; }
                if new_size.1 > 0 { panel.height = new_size.1; }
                panel.configured = true;
                let _ = event_tx.send(PresenterEvent::NeedsRender);
            }
        }

        match pt.dm.dispatch() {
            Ok(events) => {
                for event in events {
                    match event {
                        WindowEvent::OutputsChanged => {
                            if let Some((w, h)) = pt.dm.primary_output_size() {
                                let _ = event_tx.send(PresenterEvent::OutputsChanged {
                                    screen_width: w,
                                    screen_height: h,
                                });
                            }
                        }
                        WindowEvent::Click { panel_id, x_logical, y_logical, .. } => {
                            if let Some((id, panel)) = pt.presenter.panels.iter()
                                .find(|(_, p)| p.surface_id.to_string() == panel_id)
                            {
                                let _ = event_tx.send(PresenterEvent::Click {
                                    panel_id: id.clone(),
                                    x: x_logical,
                                    y: y_logical,
                                    phys_width: panel.width,
                                    phys_height: panel.height,
                                    dpr: 1.0,
                                });
                            }
                        }
                    }
                }
            }
            Err(_) => {
                tracing::info!("Wayland compositor disconnected, exiting");
                return;
            }
        }
    }
}
