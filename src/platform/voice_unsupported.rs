//! Voice input is Windows-only: other hosts' terminals do not report key release.

pub(crate) struct VoiceCapture;

impl VoiceCapture {
    pub(crate) fn open(
        _on_audio: impl FnMut(&[f32]) + Send + 'static,
    ) -> Result<(Self, u32), String> {
        Err("voice input is not supported on this platform".to_owned())
    }
}

pub(crate) fn voice_key_physically_down(_code: &crate::protocol::ClientKeyCode) -> Option<bool> {
    None
}
