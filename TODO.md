# TODO — Cross‑Platform (Windows + macOS)

## Goal
Make ExternalCue run on both Windows and macOS with equivalent functionality:
- Two live inputs (A/B), mixed and routed to a selectable output.
- Low latency, stable operation.
- Per‑channel listen toggles (local hotkeys for now; global + MIDI later).
- Clear device list and helpful warnings for format mismatches.

## Scope Notes
- No resampling: inputs must match output sample rate.
- Windows uses WASAPI (shared + exclusive).
- macOS uses CoreAudio (shared/exclusive concept differs).

## 1) Architecture Cleanup (Platform Abstraction)
- [ ] Split the backend into platform modules:
  - `src/audio/windows_wasapi_backend.rs`
  - `src/audio/macos_coreaudio_backend.rs`
- [ ] Add a unified `AudioBackend` selector in `src/audio/mod.rs` using `cfg(target_os)`.
- [ ] Ensure `DeviceEntry` holds platform‑agnostic info:
  - Name, device_id, mode (or mode-like), is_input, is_output.
- [ ] Decide how to map “Exclusive/Shared” on macOS (see section 3).

## 2) Windows Parity Tasks (Keep Existing Behavior)
- [ ] Validate shared mode in WASAPI against multiple devices.
- [ ] Ensure device selection indices remain stable across enumeration.
- [ ] Improve underrun diagnostics and UI warnings.
- [ ] Keep format mismatch checks and show UI warnings.

## 3) macOS Audio Backend (CoreAudio)
- [ ] Implement CoreAudio device enumeration (inputs/outputs):
  - Use `coreaudio-rs` or `coreaudio-sys`.
  - Capture device UID and friendly name.
- [ ] Implement audio I/O streams:
  - Input capture streams for A/B.
  - Output render stream for mixed audio.
- [ ] Sample format handling:
  - Identify device native format and confirm it matches output.
  - Abort with warning if sample rates differ.
- [ ] Latency strategy:
  - Use small buffer sizes where possible.
  - Validate stability under real device loads.

## 4) Mode Mapping (Shared/Exclusive on macOS)
- [ ] Define “mode” mapping in UI for macOS:
  - macOS doesn’t have WASAPI‑style exclusive mode.
  - Option: hide “Exclusive/Shared” suffix or map “Exclusive” to “hog mode” if possible.
- [ ] Update UI labeling by platform:
  - Windows: show SHARED/EXCLUSIVE.
  - macOS: show only device name or “DEFAULT”.

## 5) Cross‑Platform UI + Behavior
- [ ] Ensure UI device filtering works for each OS (input/output lists).
- [ ] Show warning banner for sample rate mismatch or unsupported formats.
- [ ] Keep local hotkeys consistent (F9/F10 or platform‑appropriate keys).

## 6) Testing & Validation
- [ ] Windows test matrix:
  - Shared + exclusive output.
  - One input, two inputs, no inputs.
  - Sample rate mismatch should warn and refuse.
- [ ] macOS test matrix:
  - Multiple input devices.
  - Output to built‑in + external devices.
  - Buffer stability under CPU load.

## 7) Build & CI
- [ ] Add GitHub Actions matrix build for Windows + macOS.
- [ ] Ensure cargo build passes on both platforms.

## 8) Deferred (Post‑MVP)
- [ ] Global hotkeys (platform‑specific).
- [ ] MIDI control support.
- [ ] Persistent settings and VU meters.
