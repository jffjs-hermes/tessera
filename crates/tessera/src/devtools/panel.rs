//! Windowed egui devtools overlay (feature `devtools` only).
//!
//! Wires egui directly onto the demo's existing winit/wgpu objects — **no
//! eframe**: an [`egui_winit::State`] fed from the existing
//! [`winit::event::WindowEvent`] stream, and an [`egui_wgpu::Renderer`] created
//! from the *existing* wgpu `Device`/`Queue`/surface format.
//!
//! - [`Devtools::on_window_event`] feeds each window event through egui and
//!   reports whether egui *consumed* it. The caller (main.rs) suppresses the
//!   game's own input handling when it returns `true`, so typing `s` into the
//!   console does not move the player. Backtick toggles the console panel and
//!   F3 toggles the (read-only) entity inspector — both intercepted here so
//!   they never reach egui or the game.
//! - [`Devtools::paint`] runs the egui frame (console transcript/input panel +
//!   entity inspector) and renders it *after* the cell renderer into the same
//!   surface frame via [`Renderer::render_with`]'s overlay hook. The console
//!   panel renders the transcript/input through the ungated
//!   [`console::ConsolePanel`] state.

use egui::ViewportId;
use winit::event::{ElementState, WindowEvent};
use winit::keyboard::{Key, NamedKey};
use winit::window::Window;

use super::console::{self, ConsoleContext};
use super::inspector;
use crate::sim::Sim;

// egui-wgpu re-exports the wgpu version it compiles against (the same one the
// workspace pins in tessera-render); use that re-export so we never name two
// different wgpu types for the shared Device/Queue/surface.
use egui_wgpu::wgpu;

/// Which devtools panel a key press toggles, if any. Kept as a pure function so
/// the hotkey routing is unit-testable without a window/GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    /// Backtick: the console panel.
    Console,
    /// F3: the read-only entity inspector window.
    Inspector,
}

/// Map a pressed key to a devtools panel toggle, or `None` for ordinary keys.
pub fn toggle_for_key(key: &Key) -> Option<Toggle> {
    match key {
        Key::Character(c) if c == "`" => Some(Toggle::Console),
        Key::Named(NamedKey::F3) => Some(Toggle::Inspector),
        _ => None,
    }
}

/// The pieces of the current frame the devtools overlay paints into — the same
/// window/device/queue/surface-view/encoder the cell renderer just drew with.
/// Bundled so [`Devtools::paint`] stays under clippy's argument-count limit and
/// the renderer's overlay closure can hand over its context in one value.
pub struct SceneCtx<'a> {
    pub window: &'a Window,
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub view: &'a wgpu::TextureView,
    pub encoder: &'a mut wgpu::CommandEncoder,
}

impl<'a> SceneCtx<'a> {
    /// Construct from the renderer overlay hook's arguments.
    pub fn new(
        window: &'a Window,
        device: &'a wgpu::Device,
        queue: &'a wgpu::Queue,
        view: &'a wgpu::TextureView,
        encoder: &'a mut wgpu::CommandEncoder,
    ) -> Self {
        Self {
            window,
            device,
            queue,
            view,
            encoder,
        }
    }
}

/// The egui devtools overlay for one window.
pub struct Devtools {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
    /// Console panel transcript/input state (ungated, shared with headless).
    panel: console::ConsolePanel,
    /// Inspector window visibility (F3 toggles).
    inspector_visible: bool,
    /// Selected entity id in the inspector (None = default to first row).
    inspector_selected: Option<u64>,
    /// Current physical window size (for the egui screen descriptor).
    size_px: [u32; 2],
}

impl Devtools {
    /// Create the overlay from the existing winit window + wgpu objects. The
    /// surface format must be the same one the cell renderer paints into, so
    /// egui can composite on top of the same frame.
    pub fn new(
        window: &Window,
        device: &wgpu::Device,
        surface_format: wgpu::TextureFormat,
    ) -> Self {
        let ctx = egui::Context::default();
        let state = egui_winit::State::new(
            ctx.clone(),
            ViewportId::ROOT,
            window,
            None, // native_pixels_per_point: egui derives from the window
            window.theme(),
            None, // max_texture_side: default
        );
        let renderer = egui_wgpu::Renderer::new(
            device,
            surface_format,
            egui_wgpu::RendererOptions::default(),
        );
        let size = window.inner_size();
        Self {
            ctx,
            state,
            renderer,
            panel: console::ConsolePanel::new(),
            inspector_visible: false,
            inspector_selected: None,
            size_px: [size.width, size.height],
        }
    }

    /// Feed one winit window event through egui.
    ///
    /// Returns `true` if the event was consumed by the devtools layer (a panel
    /// toggle, or egui taking the input — e.g. typing into the console text
    /// field); the caller must then suppress the game's own handling. Panel
    /// toggles (backtick / F3) are intercepted here so they never reach egui or
    /// the game.
    pub fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        // Toggle hotkeys — handled before egui so they work even while the
        // console text field has focus, and never leak to the game.
        if let WindowEvent::KeyboardInput { event: ke, .. } = event
            && ke.state == ElementState::Pressed
            && !ke.repeat
            && let Some(t) = toggle_for_key(&ke.logical_key)
        {
            match t {
                Toggle::Console => self.panel.visible = !self.panel.visible,
                Toggle::Inspector => self.inspector_visible = !self.inspector_visible,
            }
            window.request_redraw();
            return true;
        }
        let resp = self.state.on_window_event(window, event);
        if resp.repaint {
            window.request_redraw();
        }
        resp.consumed
    }

    /// Render one egui frame into the same surface frame after the cell
    /// renderer. Called from the renderer's `render_with` overlay hook, which
    /// supplies the current frame's [`SceneCtx`].
    ///
    /// Returns the per-frame user command buffers egui produced (texture uploads
    /// from [`egui_wgpu::Renderer::update_buffers`]) — the caller submits them
    /// before the encoded frame, exactly as egui's own winit integration does.
    pub fn paint(
        &mut self,
        scene: &mut SceneCtx<'_>,
        sim: &mut Sim,
        cc: &ConsoleContext<'_>,
    ) -> Vec<wgpu::CommandBuffer> {
        let window = scene.window;
        let device = scene.device;
        let queue = scene.queue;
        let view = scene.view;
        let encoder = &mut scene.encoder;
        // 1. Build the frame's raw input from the winit state.
        let raw = self.state.take_egui_input(window);
        // 2. Run the UI pass on a cheap clone of the context (egui::Context is
        //    an Arc — cloning avoids borrowing self.ctx while ui needs &mut self).
        let output = self.ctx.clone().run_pass(raw, |ctx| {
            self.ui(ctx, sim, cc);
        });
        // 3. Apply platform output (cursor icon, clipboard, IME...).
        self.state
            .handle_platform_output(window, output.platform_output);
        // 4. Apply egui's texture changes (e.g. the font atlas) *before* the
        //    tessellated shapes that sample them are uploaded. The frees are
        //    deferred until after render (winit.rs applies them post-submit;
        //    dropping late is safe because we render same-frame).
        let mut textures_delta = output.textures_delta;
        for (id, image_deltas) in textures_delta.set.drain() {
            for image_delta in image_deltas {
                self.renderer
                    .update_texture(device, queue, id, &image_delta);
            }
        }
        // 5. Tessellate shapes into GPU-ready clipped primitives.
        let clipped = self.ctx.tessellate(output.shapes, output.pixels_per_point);
        // 6. Upload buffers (fonts/textures/vertices) — returns user cmd bufs.
        let size = window.inner_size();
        self.size_px = [size.width, size.height];
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: self.size_px,
            pixels_per_point: output.pixels_per_point,
        };
        let user_cmd_bufs = self
            .renderer
            .update_buffers(device, queue, encoder, &clipped, &screen);
        // 7. Composite egui on top of the cells (Load, never clear).
        {
            let rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            self.renderer
                .render(&mut rpass.forget_lifetime(), &clipped, &screen);
        }
        // 8. Free textures egui no longer needs (after the pass that may have
        //    referenced them has been recorded into the encoder).
        for id in textures_delta.free.drain() {
            self.renderer.free_texture(&id);
        }
        user_cmd_bufs
    }

    /// Build the egui UI for this frame: the console panel (when toggled open)
    /// and the read-only entity inspector (F3).
    fn ui(&mut self, ctx: &egui::Context, sim: &mut Sim, cc: &ConsoleContext<'_>) {
        if self.panel.visible {
            egui::Window::new("Console")
                .default_size([560.0, 320.0])
                .show(ctx, |ui| {
                    // Transcript, auto-scrolled to the newest line.
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(true)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for line in &self.panel.transcript {
                                ui.monospace(line);
                            }
                        });
                    // Input line at the bottom; Enter submits.
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.panel.input)
                            .hint_text("> type a command")
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace),
                    );
                    let enter =
                        resp.lost_focus() && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter));
                    if enter {
                        self.panel.submit(sim, cc);
                        resp.request_focus();
                    }
                });
        }
        if self.inspector_visible {
            // Build a read-only snapshot under the broker read lock, then drop
            // the lock before egui draws: egui only touches the owned snapshot
            // (the renderer stays a strict reader — no broker handles cross
            // into the draw).
            let snap = inspector::snapshot(sim, cc.world, cc.save_hooks);
            inspector::show_window(ctx, &snap, &mut self.inspector_selected);
        }
    }
}

#[cfg(all(test, feature = "devtools"))]
mod tests {
    use super::*;

    /// The toggle hotkeys never reach egui or the game. This pins the pure
    /// routing function the input hook uses (a full `Devtools` needs a
    /// window + GPU, so the decision itself is what's unit-testable).
    #[test]
    fn toggle_hotkeys_route_to_panels() {
        assert_eq!(
            toggle_for_key(&Key::Character("`".into())),
            Some(Toggle::Console)
        );
        assert_eq!(
            toggle_for_key(&Key::Named(NamedKey::F3)),
            Some(Toggle::Inspector)
        );
        // Ordinary game keys are not toggles.
        assert_eq!(toggle_for_key(&Key::Character("s".into())), None);
        assert_eq!(toggle_for_key(&Key::Named(NamedKey::ArrowUp)), None);
        assert_eq!(toggle_for_key(&Key::Character("1".into())), None);
    }
}
