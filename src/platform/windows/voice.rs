//! Microphone capture and physical key state for client-side voice input.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// An open microphone. Capture stops when this is dropped.
///
/// Must be dropped on the thread that opened it: cpal streams are not `Send` on every backend.
pub(crate) struct VoiceCapture {
    _stream: cpal::Stream,
}

impl VoiceCapture {
    /// Open the default input device and start delivering mono `f32` frames to `on_audio`.
    ///
    /// Returns the device sample rate alongside the stream, because the callback receives audio at
    /// whatever rate the device runs at and the caller owns resampling.
    pub(crate) fn open(
        mut on_audio: impl FnMut(&[f32]) + Send + 'static,
    ) -> Result<(Self, u32), String> {
        let device = cpal::default_host()
            .default_input_device()
            .ok_or_else(|| "no microphone".to_owned())?;
        let supported = device
            .default_input_config()
            .map_err(|err| format!("no usable microphone config: {err}"))?;
        let rate = supported.sample_rate();
        let channels = usize::from(supported.channels()).max(1);
        let config = supported.config();
        let on_error = |err| tracing::warn!(%err, "voice capture stream error");
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device.build_input_stream(
                config,
                move |data: &[f32], _: &_| on_audio(&downmix(data, channels, |s| s)),
                on_error,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_input_stream(
                config,
                move |data: &[i16], _: &_| {
                    on_audio(&downmix(data, channels, |s| {
                        f32::from(s) / f32::from(i16::MAX)
                    }))
                },
                on_error,
                None,
            ),
            cpal::SampleFormat::I32 => device.build_input_stream(
                config,
                move |data: &[i32], _: &_| {
                    on_audio(&downmix(data, channels, |s| s as f32 / i32::MAX as f32))
                },
                on_error,
                None,
            ),
            other => return Err(format!("unsupported microphone sample format: {other:?}")),
        }
        .map_err(|err| format!("could not open the microphone: {err}"))?;
        stream.play().map_err(|err| err.to_string())?;
        Ok((Self { _stream: stream }, rate))
    }
}

fn downmix<T: Copy>(data: &[T], channels: usize, to_f32: impl Fn(T) -> f32) -> Vec<f32> {
    data.chunks(channels)
        .map(|frame| frame.iter().map(|&s| to_f32(s)).sum::<f32>() / frame.len() as f32)
        .collect()
}

/// Whether the physical key is held right now, regardless of which window has focus.
///
/// Console input never reports a release that happens while another window is focused, so this is
/// the backstop that closes the microphone in that case.
pub(crate) fn voice_key_physically_down(code: &crate::protocol::ClientKeyCode) -> Option<bool> {
    use crate::protocol::ClientKeyCode;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    let virtual_key: i32 = match code {
        ClientKeyCode::Char(' ') => 0x20,
        ClientKeyCode::Insert => 0x2d,
        ClientKeyCode::F(n @ 1..=12) => 0x6f + i32::from(*n),
        _ => return None,
    };
    // SAFETY: GetAsyncKeyState only reads global keyboard state for a valid virtual-key code.
    Some(unsafe { GetAsyncKeyState(virtual_key) } < 0)
}
