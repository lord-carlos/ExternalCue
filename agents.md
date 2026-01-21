# agents.md — Contributor Notes

Quick overview for working in this repository.

## Workflow
- Keep changes small and focused.
- After any significant code change, always run:
  - `cargo build`
- If you want the user to test something run:
  - `cargo run`

## Project Shape
- UI: `eframe`/`egui`
- Audio backend: WASAPI in `src/audio/wasapi_backend.rs`
- CPAL prototype: `src/audio/cpal_backend.rs`

## Conventions
- No resampling: inputs must match output sample rate.
- Shared vs. Exclusive: both are supported by WASAPI; selection is in the device list.
- Errors: bubble up to UI as user-visible warnings.

## Safety & Cleanup
- Prefer clear error messages for device mismatch.
- Avoid changing public APIs without a reason.
