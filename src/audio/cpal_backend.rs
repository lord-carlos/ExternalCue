use crate::audio::backend::{AudioBackend, BackendError, DeviceEntry, Mode};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use ringbuf::HeapRb;
use std::sync::{Arc, atomic::AtomicBool, atomic::Ordering};

const BUFFER_SIZE: usize = 16384;

pub struct CpalBackend {
    host: cpal::Host,
    // keep streams alive
    active_streams: Vec<cpal::Stream>,
    // cached devices corresponding to enumerate_devices ordering (one per unique friendly name)
    devices: Vec<cpal::Device>,
}

impl CpalBackend {
    pub fn new() -> Result<Self, BackendError> {
        let host = cpal::default_host();
        Ok(Self { host, active_streams: Vec::new(), devices: Vec::new() })
    }

    fn build_stream_config_from_device(device: &cpal::Device) -> Result<StreamConfig, BackendError> {
        let cfg = device.default_output_config()
            .map_err(|e| BackendError::InitError(format!("Failed to get default config: {}", e)))?;
        Ok(cfg.into())
    }
}

impl AudioBackend for CpalBackend {
    fn enumerate_devices(&self) -> Result<Vec<DeviceEntry>, BackendError> {
        let mut out = Vec::new();

        match self.host.devices() {
            Ok(devices_iter) => {
                // Collect friendly names and avoid duplicates
                let mut seen = Vec::new();
                for device in devices_iter {
                    let name = device.name().unwrap_or_else(|_| "Unknown Device".to_string());
                    if seen.contains(&name) { continue; }
                    seen.push(name.clone());

                    let is_input = device.default_input_config().is_ok();
                    let is_output = device.default_output_config().is_ok();

                    out.push(DeviceEntry { name: format!("{} (SHARED)", name), device_id: None, mode: Mode::Shared, is_input, is_output });
                    out.push(DeviceEntry { name: format!("{} (EXCLUSIVE)", name), device_id: None, mode: Mode::Exclusive, is_input, is_output });
                }
                out.sort_by(|a, b| a.name.cmp(&b.name));
                Ok(out)
            }
            Err(e) => Err(BackendError::InitError(format!("Failed to enumerate devices: {}", e))),
        }
    }

    fn start(&mut self, input_a: Option<usize>, input_b: Option<usize>, output: Option<usize>, listen_a: Arc<AtomicBool>, listen_b: Arc<AtomicBool>) -> Result<(), BackendError> {
        // Clear any existing streams
        self.active_streams.clear();

        // Rebuild devices vector aligned with unique friendly names
        self.devices.clear();
        let mut seen = Vec::new();
        if let Ok(devices_iter) = self.host.devices() {
            for device in devices_iter {
                let name = device.name().unwrap_or_else(|_| "Unknown Device".to_string());
                if seen.contains(&name) { continue; }
                seen.push(name.clone());
                self.devices.push(device);
            }
        }

        // Map selected indices (which were on duplicated list) to device indices
        let map_index = |opt_idx: Option<usize>| -> Option<usize> {
            opt_idx.map(|i| i / 2)
        };

        let in_a_dev = map_index(input_a);
        let in_b_dev = map_index(input_b);
        let out_dev = map_index(output);

        // Prepare ring buffers for each input
        let rb_a = HeapRb::<f32>::new(BUFFER_SIZE);
        let (mut prod_a, mut cons_a) = rb_a.split();

        let rb_b = HeapRb::<f32>::new(BUFFER_SIZE);
        let (mut prod_b, mut cons_b) = rb_b.split();

        // Create input streams
        if let Some(idx) = in_a_dev {
            if let Some(device) = self.devices.get(idx) {
                let cfg = device.default_input_config().map_err(|e| BackendError::StartError(format!("Failed to get default input config: {}", e)))?;
                let stream_cfg: StreamConfig = cfg.clone().into();
                match cfg.sample_format() {
                    SampleFormat::F32 => {
                        let mut prod = prod_a; // move producer into closure
                        let stream = device.build_input_stream(
                            &stream_cfg,
                            move |data: &[f32], _| {
                                for &s in data { let _ = prod.push(s); }
                            },
                            move |err| eprintln!("Input stream error: {:?}", err),
                            None,
                        ).map_err(|e| BackendError::StartError(format!("Failed to build input stream: {}", e)))?;
                        stream.play().map_err(|e| BackendError::StartError(format!("Failed to play input stream: {}", e)))?;
                        self.active_streams.push(stream);
                    }
                    SampleFormat::I16 => {
                        let mut prod = prod_a;
                        let stream = device.build_input_stream(
                            &stream_cfg,
                            move |data: &[i16], _| {
                                for &s in data {
                                    let f = (s as f32) / 32768.0;
                                    let _ = prod.push(f);
                                }
                            },
                            move |err| eprintln!("Input stream error: {:?}", err),
                            None,
                        ).map_err(|e| BackendError::StartError(format!("Failed to build input stream: {}", e)))?;
                        stream.play().map_err(|e| BackendError::StartError(format!("Failed to play input stream: {}", e)))?;
                        self.active_streams.push(stream);
                    }
                    SampleFormat::U16 => {
                        let mut prod = prod_a;
                        let stream = device.build_input_stream(
                            &stream_cfg,
                            move |data: &[u16], _| {
                                for &s in data {
                                    let f = (s as f32 - 32768.0) / 32768.0;
                                    let _ = prod.push(f);
                                }
                            },
                            move |err| eprintln!("Input stream error: {:?}", err),
                            None,
                        ).map_err(|e| BackendError::StartError(format!("Failed to build input stream: {}", e)))?;
                        stream.play().map_err(|e| BackendError::StartError(format!("Failed to play input stream: {}", e)))?;
                        self.active_streams.push(stream);
                    }
                    _ => {
                        return Err(BackendError::StartError("Unsupported input sample format".into()));
                    }
                }
            }
        }

        if let Some(idx) = in_b_dev {
            if let Some(device) = self.devices.get(idx) {
                let cfg = device.default_input_config().map_err(|e| BackendError::StartError(format!("Failed to get default input config: {}", e)))?;
                let stream_cfg: StreamConfig = cfg.clone().into();
                match cfg.sample_format() {
                    SampleFormat::F32 => {
                        let mut prod = prod_b; // move producer into closure
                        let stream = device.build_input_stream(
                            &stream_cfg,
                            move |data: &[f32], _| {
                                for &s in data { let _ = prod.push(s); }
                            },
                            move |err| eprintln!("Input stream error: {:?}", err),
                            None,
                        ).map_err(|e| BackendError::StartError(format!("Failed to build input stream: {}", e)))?;
                        stream.play().map_err(|e| BackendError::StartError(format!("Failed to play input stream: {}", e)))?;
                        self.active_streams.push(stream);
                    }
                    SampleFormat::I16 => {
                        let mut prod = prod_b;
                        let stream = device.build_input_stream(
                            &stream_cfg,
                            move |data: &[i16], _| {
                                for &s in data {
                                    let f = (s as f32) / 32768.0;
                                    let _ = prod.push(f);
                                }
                            },
                            move |err| eprintln!("Input stream error: {:?}", err),
                            None,
                        ).map_err(|e| BackendError::StartError(format!("Failed to build input stream: {}", e)))?;
                        stream.play().map_err(|e| BackendError::StartError(format!("Failed to play input stream: {}", e)))?;
                        self.active_streams.push(stream);
                    }
                    SampleFormat::U16 => {
                        let mut prod = prod_b;
                        let stream = device.build_input_stream(
                            &stream_cfg,
                            move |data: &[u16], _| {
                                for &s in data {
                                    let f = (s as f32 - 32768.0) / 32768.0;
                                    let _ = prod.push(f);
                                }
                            },
                            move |err| eprintln!("Input stream error: {:?}", err),
                            None,
                        ).map_err(|e| BackendError::StartError(format!("Failed to build input stream: {}", e)))?;
                        stream.play().map_err(|e| BackendError::StartError(format!("Failed to play input stream: {}", e)))?;
                        self.active_streams.push(stream);
                    }
                    _ => {
                        return Err(BackendError::StartError("Unsupported input sample format".into()));
                    }
                }
            }
        }

        // Create output stream that mixes from both consumers
        let idx = match out_dev {
            Some(i) => i,
            None => return Err(BackendError::StartError("No output device selected".into())),
        };
        if let Some(device) = self.devices.get(idx) {
                let cfg = device.default_output_config().map_err(|e| BackendError::StartError(format!("Failed to get default output config: {}", e)))?;
                let stream_cfg: StreamConfig = cfg.clone().into();

                match cfg.sample_format() {
                    SampleFormat::F32 => {
                        let mut cons_a = cons_a; // move consumer into closure
                        let mut cons_b = cons_b; // move consumer into closure
                        let channels = stream_cfg.channels as usize;

                        let stream = device.build_output_stream(
                            &stream_cfg,
                            move |data: &mut [f32], _| {
                                let use_a = listen_a.load(Ordering::Relaxed);
                                let use_b = listen_b.load(Ordering::Relaxed);
                                for frame in data.chunks_mut(channels) {
                                    let sample_a = if use_a { cons_a.pop().unwrap_or(0.0) } else { let _ = cons_a.pop(); 0.0 };
                                    let sample_b = if use_b { cons_b.pop().unwrap_or(0.0) } else { let _ = cons_b.pop(); 0.0 };
                                    let mixed = sample_a + sample_b;
                                    for sample in frame.iter_mut() { *sample = mixed; }
                                }
                            },
                            move |err| eprintln!("Output stream error: {:?}", err),
                            None,
                        ).map_err(|e| BackendError::StartError(format!("Failed to build output stream: {}", e)))?;

                        stream.play().map_err(|e| BackendError::StartError(format!("Failed to play output stream: {}", e)))?;
                        self.active_streams.push(stream);
                    }
                    SampleFormat::I16 | SampleFormat::U16 => {
                        return Err(BackendError::StartError("Only f32 output sample format supported in prototype".into()));
                    }
                    _ => {
                        return Err(BackendError::StartError("Only f32 output sample format supported in prototype".into()));
                    }
                }
            } else {
                return Err(BackendError::StartError("Selected output device not found".into()));
            }

        Ok(())
    }

    fn stop(&mut self) -> Result<(), BackendError> {
        // Dropping streams will stop audio
        self.active_streams.clear();
        Ok(())
    }
}
