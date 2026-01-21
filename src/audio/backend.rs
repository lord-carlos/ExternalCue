use std::sync::{Arc, atomic::AtomicBool};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Shared,
    Exclusive,
}

#[derive(Debug, Clone)]
pub struct DeviceEntry {
    pub name: String,
    /// Optional platform-specific device identifier (MMDevice ID on Windows).
    pub device_id: Option<String>,
    pub mode: Mode,
    /// True if device supports capture (input)
    pub is_input: bool,
    /// True if device supports render (output)
    pub is_output: bool,
}

#[derive(Debug)]
pub enum BackendError {
    InitError(String),
    StartError(String),
}

pub trait AudioBackend {
    /// Enumerate available devices as `DeviceEntry` (name + mode).
    fn enumerate_devices(&self) -> Result<Vec<DeviceEntry>, BackendError>;

    /// Start audio processing using selected device indices (from enumerate_devices list).
    /// This is a non-blocking call; actual audio runs on backend-managed threads/callbacks.
    fn start(&mut self, input_a: Option<usize>, input_b: Option<usize>, output: Option<usize>, listen_a: Arc<AtomicBool>, listen_b: Arc<AtomicBool>) -> Result<(), BackendError>;

    /// Stop audio processing and release resources.
    fn stop(&mut self) -> Result<(), BackendError>;
}
