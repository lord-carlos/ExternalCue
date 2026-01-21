mod audio;
use eframe::egui;
use egui::{Color32, RichText};
use crate::audio::WasapiBackend;
use crate::audio::backend::{AudioBackend, BackendError};
use rdev::Key;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

// --- CONFIGURATION (fixed hotkeys as variables) ---
// Later we can make these configurable via UI
const KEY_TOGGLE_A: Key = Key::F9;
const KEY_TOGGLE_B: Key = Key::F10;

fn main() -> eframe::Result<()> {
    let mut native_options = eframe::NativeOptions::default();
    let (w, h) = AudioApp::WINDOW_SIZE;
    native_options.initial_window_size = Some(egui::vec2(w, h));

    eframe::run_native(
        "ExternalCue - Audio Router",
        native_options,
        Box::new(|cc| Box::new(AudioApp::new(cc))),
    )
}

struct AudioApp {
    backend: WasapiBackend,
    device_entries: Vec<crate::audio::backend::DeviceEntry>,     // entries provided by backend (SHARED/EXCLUSIVE)

    // Selection Indices
    input_a_idx: Option<usize>,
    input_b_idx: Option<usize>,
    output_idx: Option<usize>,

    // Toggles controlled by global hotkeys
    listen_a: Arc<AtomicBool>,
    listen_b: Arc<AtomicBool>,
    audio_started: bool,
    last_error: Option<String>,
}

impl AudioApp {
    // Window size constant (width, height) — change here to resize the app window
    pub const WINDOW_SIZE: (f32, f32) = (700.0, 240.0);

    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        // Initialize backend and get device entries
        let backend = WasapiBackend::new().unwrap_or_else(|_| panic!("Failed to initialize WASAPI backend"));
        let entries = match backend.enumerate_devices() {
            Ok(vec) => vec,
            Err(_) => Vec::new(),
        };

        let listen_a = Arc::new(AtomicBool::new(false));
        let listen_b = Arc::new(AtomicBool::new(false));

        Self {
            backend,
            device_entries: entries,
            input_a_idx: None,
            input_b_idx: None,
            output_idx: None,
            listen_a,
            listen_b,
            audio_started: false,
            last_error: None,
        }
    }

    fn start_audio(&mut self) {
        match self.backend.start(self.input_a_idx, self.input_b_idx, self.output_idx, self.listen_a.clone(), self.listen_b.clone()) {
            Ok(()) => {
                self.audio_started = true;
                self.last_error = None;
                println!("Audio started");
            }
            Err(e) => {
                self.audio_started = false;
                let msg = match e {
                    BackendError::InitError(msg) => msg,
                    BackendError::StartError(msg) => msg,
                };
                self.last_error = Some(msg.clone());
                eprintln!("Failed to start audio backend: {}", msg);
            }
        }
    }

    fn stop_audio(&mut self) {
        match self.backend.stop() {
            Ok(()) => {
                self.audio_started = false;
                self.last_error = None;
                println!("Audio stopped");
            }
            Err(e) => {
                eprintln!("Failed to stop audio backend: {:?}", e);
            }
        }
    }
}

impl eframe::App for AudioApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Local (in-app) hotkeys only
        if ctx.input(|i| i.key_pressed(egui::Key::F9)) {
            let val = self.listen_a.load(Ordering::Relaxed);
            self.listen_a.store(!val, Ordering::Relaxed);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F10)) {
            let val = self.listen_b.load(Ordering::Relaxed);
            self.listen_b.store(!val, Ordering::Relaxed);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(RichText::new("ExternalCue").heading());
            ui.label(RichText::new("Low-Latency Audio Router").strong());
            ui.add_space(6.0);

            egui::Frame::group(ui.style()).show(ui, |ui| {
                egui::Grid::new("device_grid").spacing([16.0, 8.0]).show(ui, |ui| {
                        ui.label(RichText::new("Input Channel A:").strong());
                        render_device_picker_filtered(ui, &self.device_entries, &mut self.input_a_idx, 480.0, |d| d.is_input);
                        ui.end_row();

                        ui.label(RichText::new("Input Channel B:").strong());
                        render_device_picker_filtered(ui, &self.device_entries, &mut self.input_b_idx, 480.0, |d| d.is_input);
                        ui.end_row();

                        ui.label(RichText::new("Output Device:").strong());
                        render_device_picker_filtered(ui, &self.device_entries, &mut self.output_idx, 480.0, |d| d.is_output);
                        ui.end_row();
                    });
            });

            ui.add_space(8.0);

            ui.horizontal(|ui| {
                if !self.audio_started {
                    if ui.add_sized([180.0, 30.0], egui::Button::new("Start Audio")).clicked() {
                        self.start_audio();
                    }
                } else {
                    if ui.add_sized([120.0, 30.0], egui::Button::new("Stop Audio")).clicked() {
                        self.stop_audio();
                    }
                }

                ui.add_space(12.0);

                // Listen toggles with colored labels
                let a_state = self.listen_a.load(Ordering::Relaxed);
                let b_state = self.listen_b.load(Ordering::Relaxed);

                if ui.selectable_label(a_state, RichText::new(format!("LISTEN A ({:?})", KEY_TOGGLE_A)).color(if a_state { Color32::from_rgb(120, 220, 120) } else { Color32::LIGHT_GRAY })).clicked() {
                    let new = !a_state;
                    self.listen_a.store(new, Ordering::Relaxed);
                }

                ui.add_space(8.0);

                if ui.selectable_label(b_state, RichText::new(format!("LISTEN B ({:?})", KEY_TOGGLE_B)).color(if b_state { Color32::from_rgb(220, 120, 120) } else { Color32::LIGHT_GRAY })).clicked() {
                    let new = !b_state;
                    self.listen_b.store(new, Ordering::Relaxed);
                }
            });

            ui.add_space(10.0);
            // Status strip
            egui::Frame::none().show(ui, |ui| {
                let status_text = if self.audio_started { RichText::new("Audio: Running").color(Color32::from_rgb(120,220,120)).strong() } else { RichText::new("Audio: Stopped").color(Color32::LIGHT_RED) };
                ui.horizontal(|ui| {
                    ui.label(status_text);
                    ui.add_space(12.0);
                    ui.label(format!("Hotkeys: A={}  B={}", format!("{:?}", KEY_TOGGLE_A), format!("{:?}", KEY_TOGGLE_B)));
                });
                if let Some(msg) = &self.last_error {
                    ui.add_space(6.0);
                    ui.label(RichText::new(format!("Warning: {}", msg)).color(Color32::YELLOW));
                }
            });
        });
    }
}

fn render_device_picker(ui: &mut egui::Ui, entries: &[String], selected: &mut Option<usize>, width: f32) {
    let id = format!("device_picker_{:p}", selected);
    let selected_text = selected
        .and_then(|i| entries.get(i))
        .cloned()
        .unwrap_or_else(|| "Select...".to_string());

    egui::ComboBox::from_id_source(id)
        .selected_text(selected_text)
        .width(width)
        .show_ui(ui, |ui| {
            for (i, entry) in entries.iter().enumerate() {
                ui.selectable_value(selected, Some(i), entry.clone());
            }
        });
}

fn render_device_picker_filtered<F>(ui: &mut egui::Ui, entries: &[crate::audio::backend::DeviceEntry], selected: &mut Option<usize>, width: f32, mut filter: F)
    where F: FnMut(&crate::audio::backend::DeviceEntry) -> bool
{
    let id = format!("device_picker_{:p}", selected);
    let selected_text = selected
        .and_then(|i| entries.get(i))
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "Select...".to_string());

    egui::ComboBox::from_id_source(id)
        .selected_text(selected_text)
        .width(width)
        .show_ui(ui, |ui| {
            for (i, entry) in entries.iter().enumerate() {
                if filter(entry) {
                    ui.selectable_value(selected, Some(i), entry.name.clone());
                }
            }
        });
}
