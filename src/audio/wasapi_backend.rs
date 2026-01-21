use crate::audio::backend::{AudioBackend, BackendError, DeviceEntry, Mode};
use ringbuf::HeapRb;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::thread::{self, JoinHandle};

use winapi::Interface;
use winapi::shared::ksmedia::{
    KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, KSDATAFORMAT_SUBTYPE_PCM,
    SPEAKER_FRONT_CENTER, SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT,
};
use winapi::shared::mmreg::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_EXTENSIBLE, WAVE_FORMAT_IEEE_FLOAT, WAVE_FORMAT_PCM};
use winapi::shared::ntdef::{HANDLE, LPWSTR};
use winapi::shared::guiddef::IsEqualGUID;
use winapi::shared::winerror::{FAILED, SUCCEEDED, RPC_E_CHANGED_MODE, S_OK, S_FALSE};
use winapi::um::audioclient::{
    AUDCLNT_BUFFERFLAGS_SILENT, IAudioCaptureClient, IAudioClient, IAudioRenderClient,
};
use winapi::um::avrt::{AvSetMmThreadCharacteristicsW, AvRevertMmThreadCharacteristics};
use winapi::um::combaseapi::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL};
use winapi::um::handleapi::CloseHandle;
use winapi::um::mmdeviceapi::{
    eCapture, eRender, IMMDevice, IMMDeviceCollection, IMMDeviceEnumerator, CLSID_MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use winapi::um::objbase::COINIT_MULTITHREADED;
use winapi::um::propsys::IPropertyStore;
use winapi::um::propidl::PROPVARIANT;
use winapi::um::functiondiscoverykeys_devpkey::PKEY_Device_FriendlyName;
use winapi::shared::wtypes::VT_LPWSTR;

const STGM_READ: u32 = 0x00000000;
use winapi::um::synchapi::{CreateEventW, SetEvent, WaitForSingleObject};
use winapi::um::winbase::WAIT_OBJECT_0;

const BUFFER_FRAMES: usize = 16384;

pub struct WasapiBackend {
    stop_flag: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    event_handles: Vec<HANDLE>,
}

#[derive(Clone, Copy)]
struct FormatInfo {
    channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
    is_float: bool,
}

const AUDCLNT_SHAREMODE_SHARED: u32 = 0;
const AUDCLNT_SHAREMODE_EXCLUSIVE: u32 = 1;
const AUDCLNT_STREAMFLAGS_EVENTCALLBACK: u32 = 0x00040000;
const AUDCLNT_STREAMFLAGS_NOPERSIST: u32 = 0x00080000;


struct ClientBundle {
    audio_client: *mut IAudioClient,
    event: HANDLE,
    format: FormatInfo,
    buffer_frames: u32,
}

impl WasapiBackend {
    pub fn new() -> Result<Self, BackendError> {
        Ok(Self {
            stop_flag: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
            event_handles: Vec::new(),
        })
    }

    fn com_init() -> Result<bool, BackendError> {
        let hr = unsafe { CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            // COM already initialized in a different mode (likely STA). Continue without uninit.
            return Ok(false);
        }
        if FAILED(hr) {
            return Err(BackendError::InitError(format!("CoInitializeEx failed: 0x{:08X}", hr as u32)));
        }
        Ok(true)
    }

    fn com_uninit(should_uninit: bool) {
        if should_uninit {
            unsafe { CoUninitialize(); }
        }
    }

    fn pwstr_to_string(p: LPWSTR) -> String {
        if p.is_null() {
            return String::new();
        }
        unsafe {
            let mut len = 0usize;
            while *p.add(len) != 0 { len += 1; }
            let slice = std::slice::from_raw_parts(p, len);
            String::from_utf16_lossy(slice)
        }
    }

    fn to_wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }

    unsafe fn create_enumerator() -> Result<*mut IMMDeviceEnumerator, BackendError> {
        let mut enumerator: *mut IMMDeviceEnumerator = ptr::null_mut();
        let hr = CoCreateInstance(
            &CLSID_MMDeviceEnumerator,
            ptr::null_mut(),
            CLSCTX_ALL,
            &IMMDeviceEnumerator::uuidof(),
            &mut enumerator as *mut _ as *mut _,
        );
        if FAILED(hr) {
            return Err(BackendError::InitError(format!("CoCreateInstance(MMDeviceEnumerator) failed: 0x{:08X}", hr as u32)));
        }
        Ok(enumerator)
    }

    unsafe fn enum_devices(enumerator: *mut IMMDeviceEnumerator, flow: u32, is_input: bool, is_output: bool) -> Result<Vec<DeviceEntry>, BackendError> {
        let mut out = Vec::new();
        let mut collection: *mut IMMDeviceCollection = ptr::null_mut();
        let hr = (*enumerator).EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE, &mut collection);
        if FAILED(hr) {
            return Err(BackendError::InitError(format!("EnumAudioEndpoints failed: 0x{:08X}", hr as u32)));
        }

        let mut count: u32 = 0;
        let hr = (*collection).GetCount(&mut count);
        if FAILED(hr) {
            (*collection).Release();
            return Err(BackendError::InitError(format!("GetCount failed: 0x{:08X}", hr as u32)));
        }

        for i in 0..count {
            let mut device: *mut IMMDevice = ptr::null_mut();
            let hr = (*collection).Item(i, &mut device);
            if FAILED(hr) {
                continue;
            }

            let mut id_ptr: LPWSTR = ptr::null_mut();
            let hr = (*device).GetId(&mut id_ptr);
            if SUCCEEDED(hr) {
                let id = WasapiBackend::pwstr_to_string(id_ptr);
                let display = WasapiBackend::get_friendly_name(device).unwrap_or_else(|| id.clone());
                out.push(DeviceEntry { name: format!("{} (SHARED)", display), device_id: Some(id.clone()), mode: Mode::Shared, is_input, is_output });
                out.push(DeviceEntry { name: format!("{} (EXCLUSIVE)", display), device_id: Some(id.clone()), mode: Mode::Exclusive, is_input, is_output });
            }

            if !id_ptr.is_null() {
                CoTaskMemFree(id_ptr as *mut _);
            }

            (*device).Release();
        }

        (*collection).Release();

        Ok(out)
    }

    unsafe fn get_friendly_name(device: *mut IMMDevice) -> Option<String> {
        let mut store: *mut IPropertyStore = ptr::null_mut();
        let hr = (*device).OpenPropertyStore(STGM_READ, &mut store);
        if FAILED(hr) || store.is_null() {
            return None;
        }

        let mut pv: PROPVARIANT = std::mem::zeroed();
        let hr = (*store).GetValue(&PKEY_Device_FriendlyName, &mut pv);
        (*store).Release();
        if FAILED(hr) {
            return None;
        }

        let mut name = None;
        if pv.vt as u32 == VT_LPWSTR {
            let ptr = unsafe { *pv.data.pwszVal() };
            if !ptr.is_null() {
                name = Some(WasapiBackend::pwstr_to_string(ptr));
                CoTaskMemFree(ptr as *mut _);
            }
        }

        name
    }

    unsafe fn parse_format(pwfx: *const WAVEFORMATEX) -> Result<FormatInfo, BackendError> {
        if pwfx.is_null() {
            return Err(BackendError::StartError("Null WAVEFORMATEX".into()));
        }

        let fmt = &*pwfx;
        let mut is_float = false;

        if fmt.wFormatTag == WAVE_FORMAT_IEEE_FLOAT {
            is_float = true;
        } else if fmt.wFormatTag == WAVE_FORMAT_PCM {
            is_float = false;
        } else if fmt.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
            let ext = &*(pwfx as *const WAVEFORMATEXTENSIBLE);
            let subformat = std::ptr::read_unaligned(std::ptr::addr_of!(ext.SubFormat));
            if IsEqualGUID(&subformat, &KSDATAFORMAT_SUBTYPE_IEEE_FLOAT) {
                is_float = true;
            } else if IsEqualGUID(&subformat, &KSDATAFORMAT_SUBTYPE_PCM) {
                is_float = false;
            } else {
                return Err(BackendError::StartError("Unsupported extensible format".into()));
            }
        } else {
            return Err(BackendError::StartError("Unsupported format tag".into()));
        }

        Ok(FormatInfo {
            channels: fmt.nChannels,
            sample_rate: fmt.nSamplesPerSec,
            bits_per_sample: fmt.wBitsPerSample,
            is_float,
        })
    }

    unsafe fn open_device_exclusive(enumerator: *mut IMMDeviceEnumerator, device_id: &str) -> Result<ClientBundle, BackendError> {
        let wide = WasapiBackend::to_wide(device_id);
        let mut device: *mut IMMDevice = ptr::null_mut();
        let hr = (*enumerator).GetDevice(wide.as_ptr(), &mut device);
        if FAILED(hr) {
            return Err(BackendError::StartError(format!("GetDevice failed: 0x{:08X}", hr as u32)));
        }

        let mut audio_client: *mut IAudioClient = ptr::null_mut();
        let hr = (*device).Activate(&IAudioClient::uuidof(), CLSCTX_ALL, ptr::null_mut(), &mut audio_client as *mut _ as *mut _);
        (*device).Release();
        if FAILED(hr) {
            return Err(BackendError::StartError(format!("Activate(IAudioClient) failed: 0x{:08X}", hr as u32)));
        }

        let mut pwfx: *mut WAVEFORMATEX = ptr::null_mut();
        let hr = (*audio_client).GetMixFormat(&mut pwfx);
        if FAILED(hr) {
            (*audio_client).Release();
            return Err(BackendError::StartError(format!("GetMixFormat failed: 0x{:08X}", hr as u32)));
        }

        // Helper to initialize using a given format
        let mut try_init = |fmt_ptr: *const WAVEFORMATEX| -> Result<ClientBundle, BackendError> {
            let format = WasapiBackend::parse_format(fmt_ptr)?;

            let mut default_period: i64 = 0;
            let mut min_period: i64 = 0;
            let hr = (*audio_client).GetDevicePeriod(&mut default_period, &mut min_period);
            if FAILED(hr) {
                return Err(BackendError::StartError(format!("GetDevicePeriod failed: 0x{:08X}", hr as u32)));
            }

            let hns = if default_period > 0 { default_period } else { min_period };
            let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_NOPERSIST;
            let hr = (*audio_client).Initialize(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                flags,
                hns,
                hns,
                fmt_ptr,
                ptr::null(),
            );
            if FAILED(hr) {
                return Err(BackendError::StartError(format!("IAudioClient::Initialize failed: 0x{:08X}", hr as u32)));
            }

            let event = CreateEventW(ptr::null_mut(), 0, 0, ptr::null());
            if event.is_null() {
                return Err(BackendError::StartError("CreateEventW failed".into()));
            }

            let hr = (*audio_client).SetEventHandle(event);
            if FAILED(hr) {
                CloseHandle(event);
                return Err(BackendError::StartError(format!("SetEventHandle failed: 0x{:08X}", hr as u32)));
            }

            let mut buffer_frames: u32 = 0;
            let hr = (*audio_client).GetBufferSize(&mut buffer_frames);
            if FAILED(hr) {
                CloseHandle(event);
                return Err(BackendError::StartError(format!("GetBufferSize failed: 0x{:08X}", hr as u32)));
            }

            Ok(ClientBundle { audio_client, event, format, buffer_frames })
        };

        // 1) Try mix format (or closest)
        let mut closest: *mut WAVEFORMATEX = ptr::null_mut();
        let hr = (*audio_client).IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, pwfx as *const _, &mut closest);
        if hr == S_OK {
            let result = try_init(pwfx as *const _);
            CoTaskMemFree(pwfx as *mut _);
            return result;
        } else if hr == S_FALSE && !closest.is_null() {
            let result = try_init(closest as *const _);
            CoTaskMemFree(closest as *mut _);
            CoTaskMemFree(pwfx as *mut _);
            return result;
        }

        // 2) Try common exclusive formats
        let mix = WasapiBackend::parse_format(pwfx as *const _).ok();
        let base_channels = mix.map(|m| m.channels).unwrap_or(2);
        let base_rates = [
            mix.map(|m| m.sample_rate).unwrap_or(48000),
            48000,
            44100,
        ];

        for &rate in base_rates.iter() {
            for &(bits, is_float) in [(32, true), (24, false), (16, false)].iter() {
                let mut wfxe: WAVEFORMATEXTENSIBLE = std::mem::zeroed();
                let channels = base_channels;
                let block_align = (bits / 8) * channels;
                if block_align == 0 { continue; }
                wfxe.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE;
                wfxe.Format.nChannels = channels;
                wfxe.Format.nSamplesPerSec = rate;
                wfxe.Format.wBitsPerSample = bits;
                wfxe.Format.nBlockAlign = block_align;
                wfxe.Format.nAvgBytesPerSec = rate * block_align as u32;
                wfxe.Format.cbSize = (std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>()) as u16;
                wfxe.Samples = bits;
                wfxe.dwChannelMask = if channels == 1 {
                    SPEAKER_FRONT_CENTER
                } else if channels == 2 {
                    SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
                } else {
                    0
                };
                wfxe.SubFormat = if is_float { KSDATAFORMAT_SUBTYPE_IEEE_FLOAT } else { KSDATAFORMAT_SUBTYPE_PCM };

                let mut closest: *mut WAVEFORMATEX = ptr::null_mut();
                let hr = (*audio_client).IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, &wfxe.Format as *const _, &mut closest);
                if hr == S_OK {
                    CoTaskMemFree(pwfx as *mut _);
                    return try_init(&wfxe.Format as *const _);
                } else if hr == S_FALSE && !closest.is_null() {
                    let result = try_init(closest as *const _);
                    CoTaskMemFree(closest as *mut _);
                    CoTaskMemFree(pwfx as *mut _);
                    return result;
                }
            }
        }

        CoTaskMemFree(pwfx as *mut _);
        (*audio_client).Release();
        Err(BackendError::StartError("IsFormatSupported failed: 0x88890008".into()))
    }

    unsafe fn open_device_shared(enumerator: *mut IMMDeviceEnumerator, device_id: &str) -> Result<ClientBundle, BackendError> {
        let wide = WasapiBackend::to_wide(device_id);
        let mut device: *mut IMMDevice = ptr::null_mut();
        let hr = (*enumerator).GetDevice(wide.as_ptr(), &mut device);
        if FAILED(hr) {
            return Err(BackendError::StartError(format!("GetDevice failed: 0x{:08X}", hr as u32)));
        }

        let mut audio_client: *mut IAudioClient = ptr::null_mut();
        let hr = (*device).Activate(&IAudioClient::uuidof(), CLSCTX_ALL, ptr::null_mut(), &mut audio_client as *mut _ as *mut _);
        (*device).Release();
        if FAILED(hr) {
            return Err(BackendError::StartError(format!("Activate(IAudioClient) failed: 0x{:08X}", hr as u32)));
        }

        let mut pwfx: *mut WAVEFORMATEX = ptr::null_mut();
        let hr = (*audio_client).GetMixFormat(&mut pwfx);
        if FAILED(hr) {
            (*audio_client).Release();
            return Err(BackendError::StartError(format!("GetMixFormat failed: 0x{:08X}", hr as u32)));
        }

        let mut closest: *mut WAVEFORMATEX = ptr::null_mut();
        let hr = (*audio_client).IsFormatSupported(AUDCLNT_SHAREMODE_SHARED, pwfx as *const _, &mut closest);
        let fmt_ptr: *const WAVEFORMATEX = if hr == S_OK {
            pwfx as *const _
        } else if hr == S_FALSE && !closest.is_null() {
            closest as *const _
        } else {
            CoTaskMemFree(pwfx as *mut _);
            (*audio_client).Release();
            return Err(BackendError::StartError("IsFormatSupported (shared) failed".into()));
        };

        let format = match WasapiBackend::parse_format(fmt_ptr) {
            Ok(v) => v,
            Err(e) => {
                if !closest.is_null() { CoTaskMemFree(closest as *mut _); }
                CoTaskMemFree(pwfx as *mut _);
                (*audio_client).Release();
                return Err(e);
            }
        };

        let mut default_period: i64 = 0;
        let mut min_period: i64 = 0;
        let hr = (*audio_client).GetDevicePeriod(&mut default_period, &mut min_period);
        if FAILED(hr) {
            if !closest.is_null() { CoTaskMemFree(closest as *mut _); }
            CoTaskMemFree(pwfx as *mut _);
            (*audio_client).Release();
            return Err(BackendError::StartError(format!("GetDevicePeriod failed: 0x{:08X}", hr as u32)));
        }

        let hns_buffer = if default_period > 0 { default_period } else { min_period };
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_NOPERSIST;
        let hr = (*audio_client).Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            hns_buffer,
            0,
            fmt_ptr,
            ptr::null(),
        );
        if FAILED(hr) {
            if !closest.is_null() { CoTaskMemFree(closest as *mut _); }
            CoTaskMemFree(pwfx as *mut _);
            (*audio_client).Release();
            return Err(BackendError::StartError(format!("IAudioClient::Initialize (shared) failed: 0x{:08X}", hr as u32)));
        }

        let event = CreateEventW(ptr::null_mut(), 0, 0, ptr::null());
        if event.is_null() {
            if !closest.is_null() { CoTaskMemFree(closest as *mut _); }
            CoTaskMemFree(pwfx as *mut _);
            (*audio_client).Release();
            return Err(BackendError::StartError("CreateEventW failed".into()));
        }

        let hr = (*audio_client).SetEventHandle(event);
        if FAILED(hr) {
            CloseHandle(event);
            if !closest.is_null() { CoTaskMemFree(closest as *mut _); }
            CoTaskMemFree(pwfx as *mut _);
            (*audio_client).Release();
            return Err(BackendError::StartError(format!("SetEventHandle failed: 0x{:08X}", hr as u32)));
        }

        let mut buffer_frames: u32 = 0;
        let hr = (*audio_client).GetBufferSize(&mut buffer_frames);
        if FAILED(hr) {
            CloseHandle(event);
            if !closest.is_null() { CoTaskMemFree(closest as *mut _); }
            CoTaskMemFree(pwfx as *mut _);
            (*audio_client).Release();
            return Err(BackendError::StartError(format!("GetBufferSize failed: 0x{:08X}", hr as u32)));
        }

        if !closest.is_null() { CoTaskMemFree(closest as *mut _); }
        CoTaskMemFree(pwfx as *mut _);

        Ok(ClientBundle { audio_client, event, format, buffer_frames })
    }
}

impl AudioBackend for WasapiBackend {
    fn enumerate_devices(&self) -> Result<Vec<DeviceEntry>, BackendError> {
        let should_uninit = WasapiBackend::com_init()?;
        let mut out = Vec::new();
        unsafe {
            let enumerator = WasapiBackend::create_enumerator()?;
            let mut render = WasapiBackend::enum_devices(enumerator, eRender, false, true)?;
            let mut capture = WasapiBackend::enum_devices(enumerator, eCapture, true, false)?;
            (*enumerator).Release();
            out.append(&mut render);
            out.append(&mut capture);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        WasapiBackend::com_uninit(should_uninit);
        Ok(out)
    }

    fn start(&mut self, input_a: Option<usize>, input_b: Option<usize>, output: Option<usize>, listen_a: Arc<AtomicBool>, listen_b: Arc<AtomicBool>) -> Result<(), BackendError> {
        // Stop any existing threads
        let _ = self.stop();
        self.stop_flag.store(false, Ordering::Relaxed);

        // Build device map from current enumeration
        let entries = self.enumerate_devices()?;

        let get_entry = |idx: Option<usize>| -> Result<Option<DeviceEntry>, BackendError> {
            if let Some(i) = idx {
                entries.get(i).cloned().map(Some).ok_or_else(|| BackendError::StartError("Device index out of range".into()))
            } else {
                Ok(None)
            }
        };

        let in_a = get_entry(input_a)?;
        let in_b = get_entry(input_b)?;
        let out = get_entry(output)?;

        let out = match out {
            Some(d) => d,
            None => return Err(BackendError::StartError("Output device must be selected".into())),
        };

        let out_mode = out.mode;
        let in_a_mode = in_a.as_ref().map(|d| d.mode);
        let in_b_mode = in_b.as_ref().map(|d| d.mode);

        let out_id = out.device_id.clone().ok_or_else(|| BackendError::StartError("Output device has no ID".into()))?;
        let in_a_id = in_a.as_ref().and_then(|d| d.device_id.clone());
        let in_b_id = in_b.as_ref().and_then(|d| d.device_id.clone());

        let should_uninit = WasapiBackend::com_init()?;
        let mut threads = Vec::new();

        unsafe {
            let enumerator = WasapiBackend::create_enumerator()?;

            // Open output device
            let out_bundle = match out_mode {
                Mode::Exclusive => WasapiBackend::open_device_exclusive(enumerator, &out_id)?,
                Mode::Shared => WasapiBackend::open_device_shared(enumerator, &out_id)?,
            };
            let out_format = out_bundle.format.clone();

            // Open inputs if provided
            let in_a_bundle = if let Some(id) = in_a_id.as_ref() {
                let mode = in_a_mode.unwrap_or(Mode::Exclusive);
                Some(match mode {
                    Mode::Exclusive => WasapiBackend::open_device_exclusive(enumerator, id)?,
                    Mode::Shared => WasapiBackend::open_device_shared(enumerator, id)?,
                })
            } else { None };
            let in_b_bundle = if let Some(id) = in_b_id.as_ref() {
                let mode = in_b_mode.unwrap_or(Mode::Exclusive);
                Some(match mode {
                    Mode::Exclusive => WasapiBackend::open_device_exclusive(enumerator, id)?,
                    Mode::Shared => WasapiBackend::open_device_shared(enumerator, id)?,
                })
            } else { None };

            (*enumerator).Release();

            // Validate format compatibility
            if let Some(ref b) = in_a_bundle {
                if b.format.sample_rate != out_format.sample_rate || b.format.channels == 0 {
                    (*out_bundle.audio_client).Release();
                    return Err(BackendError::StartError(format!(
                        "Input A sample rate mismatch ({} Hz vs output {} Hz)",
                        b.format.sample_rate,
                        out_format.sample_rate
                    )));
                }
            }
            if let Some(ref b) = in_b_bundle {
                if b.format.sample_rate != out_format.sample_rate || b.format.channels == 0 {
                    (*out_bundle.audio_client).Release();
                    return Err(BackendError::StartError(format!(
                        "Input B sample rate mismatch ({} Hz vs output {} Hz)",
                        b.format.sample_rate,
                        out_format.sample_rate
                    )));
                }
            }

            // Create ringbuffers
            let in_a_channels = in_a_bundle.as_ref().map(|b| b.format.channels as usize).unwrap_or(0).max(1);
            let in_b_channels = in_b_bundle.as_ref().map(|b| b.format.channels as usize).unwrap_or(0).max(1);

            let rb_a = HeapRb::<f32>::new(BUFFER_FRAMES * in_a_channels);
            let (mut prod_a, mut cons_a) = rb_a.split();
            let rb_b = HeapRb::<f32>::new(BUFFER_FRAMES * in_b_channels);
            let (mut prod_b, mut cons_b) = rb_b.split();

            // Spawn capture threads
            if let Some(bundle) = in_a_bundle {
                let stop_flag = self.stop_flag.clone();
                let event = bundle.event;
                self.event_handles.push(event);

                let audio_client = bundle.audio_client as usize;
                let format = bundle.format;
                let task_name = WasapiBackend::to_wide("Pro Audio");
                let event = event as usize;

                let handle = thread::spawn(move || {
                    unsafe { CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED); }
                    let mut task_index: u32 = 0;
                    let mmcss = unsafe { AvSetMmThreadCharacteristicsW(task_name.as_ptr(), &mut task_index) };

                    let audio_client = audio_client as *mut IAudioClient;
                    let event = event as HANDLE;
                    let mut capture_client: *mut IAudioCaptureClient = ptr::null_mut();
                    let hr = unsafe { (*audio_client).GetService(&IAudioCaptureClient::uuidof(), &mut capture_client as *mut _ as *mut _) };
                    if FAILED(hr) {
                        unsafe { (*audio_client).Release(); }
                        if !mmcss.is_null() { unsafe { AvRevertMmThreadCharacteristics(mmcss); } }
                        unsafe { CoUninitialize(); }
                        return;
                    }

                    unsafe { (*audio_client).Start(); }

                    while !stop_flag.load(Ordering::Relaxed) {
                        let wait = unsafe { WaitForSingleObject(event, 2000) };
                        if wait != WAIT_OBJECT_0 { continue; }

                        let mut packet: u32 = 0;
                        unsafe { (*capture_client).GetNextPacketSize(&mut packet); }
                        while packet > 0 {
                            let mut data: *mut u8 = ptr::null_mut();
                            let mut frames: u32 = 0;
                            let mut flags: u32 = 0;
                            let hr = unsafe { (*capture_client).GetBuffer(&mut data, &mut frames, &mut flags, ptr::null_mut(), ptr::null_mut()) };
                            if FAILED(hr) { break; }

                            let channels = format.channels as usize;
                            if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 {
                                for _ in 0..(frames as usize * channels) { let _ = prod_a.push(0.0); }
                            } else {
                                let total = frames as usize * channels;
                                if format.is_float && format.bits_per_sample == 32 {
                                    let samples = unsafe { std::slice::from_raw_parts(data as *const f32, total) };
                                    for s in samples.iter().take(total) { let _ = prod_a.push(*s); }
                                } else if !format.is_float && format.bits_per_sample == 16 {
                                    let samples = unsafe { std::slice::from_raw_parts(data as *const i16, total) };
                                    for s in samples.iter().take(total) { let _ = prod_a.push(*s as f32 / 32768.0); }
                                } else if !format.is_float && format.bits_per_sample == 32 {
                                    let samples = unsafe { std::slice::from_raw_parts(data as *const i32, total) };
                                    for s in samples.iter().take(total) { let _ = prod_a.push(*s as f32 / 2147483648.0); }
                                }
                            }

                            unsafe { (*capture_client).ReleaseBuffer(frames); }
                            unsafe { (*capture_client).GetNextPacketSize(&mut packet); }
                        }
                    }

                    unsafe { (*audio_client).Stop(); }
                    unsafe { (*capture_client).Release(); }
                    unsafe { (*audio_client).Release(); }
                    if !mmcss.is_null() { unsafe { AvRevertMmThreadCharacteristics(mmcss); } }
                    unsafe { CoUninitialize(); }
                });

                threads.push(handle);
            }

            if let Some(bundle) = in_b_bundle {
                let stop_flag = self.stop_flag.clone();
                let event = bundle.event;
                self.event_handles.push(event);

                let audio_client = bundle.audio_client as usize;
                let format = bundle.format;
                let task_name = WasapiBackend::to_wide("Pro Audio");
                let event = event as usize;

                let handle = thread::spawn(move || {
                    unsafe { CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED); }
                    let mut task_index: u32 = 0;
                    let mmcss = unsafe { AvSetMmThreadCharacteristicsW(task_name.as_ptr(), &mut task_index) };

                    let audio_client = audio_client as *mut IAudioClient;
                    let event = event as HANDLE;
                    let mut capture_client: *mut IAudioCaptureClient = ptr::null_mut();
                    let hr = unsafe { (*audio_client).GetService(&IAudioCaptureClient::uuidof(), &mut capture_client as *mut _ as *mut _) };
                    if FAILED(hr) {
                        unsafe { (*audio_client).Release(); }
                        if !mmcss.is_null() { unsafe { AvRevertMmThreadCharacteristics(mmcss); } }
                        unsafe { CoUninitialize(); }
                        return;
                    }

                    unsafe { (*audio_client).Start(); }

                    while !stop_flag.load(Ordering::Relaxed) {
                        let wait = unsafe { WaitForSingleObject(event, 2000) };
                        if wait != WAIT_OBJECT_0 { continue; }

                        let mut packet: u32 = 0;
                        unsafe { (*capture_client).GetNextPacketSize(&mut packet); }
                        while packet > 0 {
                            let mut data: *mut u8 = ptr::null_mut();
                            let mut frames: u32 = 0;
                            let mut flags: u32 = 0;
                            let hr = unsafe { (*capture_client).GetBuffer(&mut data, &mut frames, &mut flags, ptr::null_mut(), ptr::null_mut()) };
                            if FAILED(hr) { break; }

                            let channels = format.channels as usize;
                            if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 {
                                for _ in 0..(frames as usize * channels) { let _ = prod_b.push(0.0); }
                            } else {
                                let total = frames as usize * channels;
                                if format.is_float && format.bits_per_sample == 32 {
                                    let samples = unsafe { std::slice::from_raw_parts(data as *const f32, total) };
                                    for s in samples.iter().take(total) { let _ = prod_b.push(*s); }
                                } else if !format.is_float && format.bits_per_sample == 16 {
                                    let samples = unsafe { std::slice::from_raw_parts(data as *const i16, total) };
                                    for s in samples.iter().take(total) { let _ = prod_b.push(*s as f32 / 32768.0); }
                                } else if !format.is_float && format.bits_per_sample == 32 {
                                    let samples = unsafe { std::slice::from_raw_parts(data as *const i32, total) };
                                    for s in samples.iter().take(total) { let _ = prod_b.push(*s as f32 / 2147483648.0); }
                                }
                            }

                            unsafe { (*capture_client).ReleaseBuffer(frames); }
                            unsafe { (*capture_client).GetNextPacketSize(&mut packet); }
                        }
                    }

                    unsafe { (*audio_client).Stop(); }
                    unsafe { (*capture_client).Release(); }
                    unsafe { (*audio_client).Release(); }
                    if !mmcss.is_null() { unsafe { AvRevertMmThreadCharacteristics(mmcss); } }
                    unsafe { CoUninitialize(); }
                });

                threads.push(handle);
            }

            // Output thread
            {
                let stop_flag = self.stop_flag.clone();
                let event = out_bundle.event;
                self.event_handles.push(event);

                let audio_client = out_bundle.audio_client as usize;
                let format = out_bundle.format;
                let buffer_frames = out_bundle.buffer_frames;
                let task_name = WasapiBackend::to_wide("Pro Audio");
                let event = event as usize;
                let in_a_channels = in_a_channels;
                let in_b_channels = in_b_channels;

                let handle = thread::spawn(move || {
                    unsafe { CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED); }
                    let mut task_index: u32 = 0;
                    let mmcss = unsafe { AvSetMmThreadCharacteristicsW(task_name.as_ptr(), &mut task_index) };

                    let audio_client = audio_client as *mut IAudioClient;
                    let event = event as HANDLE;
                    let mut render_client: *mut IAudioRenderClient = ptr::null_mut();
                    let hr = unsafe { (*audio_client).GetService(&IAudioRenderClient::uuidof(), &mut render_client as *mut _ as *mut _) };
                    if FAILED(hr) {
                        unsafe { (*audio_client).Release(); }
                        if !mmcss.is_null() { unsafe { AvRevertMmThreadCharacteristics(mmcss); } }
                        unsafe { CoUninitialize(); }
                        return;
                    }

                    unsafe { (*audio_client).Start(); }

                    while !stop_flag.load(Ordering::Relaxed) {
                        let wait = unsafe { WaitForSingleObject(event, 2000) };
                        if wait != WAIT_OBJECT_0 { continue; }

                        let mut padding: u32 = 0;
                        let hr = unsafe { (*audio_client).GetCurrentPadding(&mut padding) };
                        if FAILED(hr) { continue; }

                        let frames_avail = buffer_frames.saturating_sub(padding);
                        if frames_avail == 0 { continue; }

                        let mut data: *mut u8 = ptr::null_mut();
                        let hr = unsafe { (*render_client).GetBuffer(frames_avail, &mut data) };
                        if FAILED(hr) { continue; }

                        let channels = format.channels as usize;
                        let total = frames_avail as usize * channels;

                        let use_a = listen_a.load(Ordering::Relaxed);
                        let use_b = listen_b.load(Ordering::Relaxed);

                        let mut frame_a = vec![0.0f32; in_a_channels];
                        let mut frame_b = vec![0.0f32; in_b_channels];

                        if format.is_float && format.bits_per_sample == 32 {
                            let samples = unsafe { std::slice::from_raw_parts_mut(data as *mut f32, total) };
                            for f in 0..frames_avail as usize {
                                for i in 0..in_a_channels { frame_a[i] = if use_a { cons_a.pop().unwrap_or(0.0) } else { let _ = cons_a.pop(); 0.0 }; }
                                for i in 0..in_b_channels { frame_b[i] = if use_b { cons_b.pop().unwrap_or(0.0) } else { let _ = cons_b.pop(); 0.0 }; }
                                for ch in 0..channels {
                                    let a = if in_a_channels == 0 { 0.0 } else if ch < in_a_channels { frame_a[ch] } else { frame_a[0] };
                                    let b = if in_b_channels == 0 { 0.0 } else if ch < in_b_channels { frame_b[ch] } else { frame_b[0] };
                                    samples[f * channels + ch] = a + b;
                                }
                            }
                        } else if !format.is_float && format.bits_per_sample == 16 {
                            let samples = unsafe { std::slice::from_raw_parts_mut(data as *mut i16, total) };
                            for f in 0..frames_avail as usize {
                                for i in 0..in_a_channels { frame_a[i] = if use_a { cons_a.pop().unwrap_or(0.0) } else { let _ = cons_a.pop(); 0.0 }; }
                                for i in 0..in_b_channels { frame_b[i] = if use_b { cons_b.pop().unwrap_or(0.0) } else { let _ = cons_b.pop(); 0.0 }; }
                                for ch in 0..channels {
                                    let a = if in_a_channels == 0 { 0.0 } else if ch < in_a_channels { frame_a[ch] } else { frame_a[0] };
                                    let b = if in_b_channels == 0 { 0.0 } else if ch < in_b_channels { frame_b[ch] } else { frame_b[0] };
                                    let mixed = (a + b).clamp(-1.0, 1.0);
                                    samples[f * channels + ch] = (mixed * 32767.0) as i16;
                                }
                            }
                        } else if !format.is_float && format.bits_per_sample == 32 {
                            let samples = unsafe { std::slice::from_raw_parts_mut(data as *mut i32, total) };
                            for f in 0..frames_avail as usize {
                                for i in 0..in_a_channels { frame_a[i] = if use_a { cons_a.pop().unwrap_or(0.0) } else { let _ = cons_a.pop(); 0.0 }; }
                                for i in 0..in_b_channels { frame_b[i] = if use_b { cons_b.pop().unwrap_or(0.0) } else { let _ = cons_b.pop(); 0.0 }; }
                                for ch in 0..channels {
                                    let a = if in_a_channels == 0 { 0.0 } else if ch < in_a_channels { frame_a[ch] } else { frame_a[0] };
                                    let b = if in_b_channels == 0 { 0.0 } else if ch < in_b_channels { frame_b[ch] } else { frame_b[0] };
                                    let mixed = (a + b).clamp(-1.0, 1.0);
                                    samples[f * channels + ch] = (mixed * 2147483647.0) as i32;
                                }
                            }
                        }

                        unsafe { (*render_client).ReleaseBuffer(frames_avail, 0); }
                    }

                    unsafe { (*audio_client).Stop(); }
                    unsafe { (*render_client).Release(); }
                    unsafe { (*audio_client).Release(); }
                    if !mmcss.is_null() { unsafe { AvRevertMmThreadCharacteristics(mmcss); } }
                    unsafe { CoUninitialize(); }
                });

                threads.push(handle);
            }

        }

        WasapiBackend::com_uninit(should_uninit);

        self.threads = threads;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), BackendError> {
        self.stop_flag.store(true, Ordering::Relaxed);
        for h in self.event_handles.drain(..) {
            unsafe { SetEvent(h); }
            unsafe { CloseHandle(h); }
        }

        for t in self.threads.drain(..) {
            let _ = t.join();
        }

        Ok(())
    }
}
