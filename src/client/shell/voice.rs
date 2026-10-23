//! Shell hooks for client-side voice input: where dictated text would land, and the level meter.

use super::*;

impl ClientShellState {
    /// Identity of the pane typed text would reach right now, or `None` while input is bound for
    /// Herdr itself (prefix, navigation, copy mode, overlays).
    pub(crate) fn voice_input_target(&self) -> Option<String> {
        if self.popup_pending {
            return None;
        }
        let endpoint = self.active_endpoint_id.storage_key();
        if let Some(terminal_id) = self.popup_terminal_id.as_ref() {
            return Some(format!("{endpoint}/popup/{terminal_id}"));
        }
        if self.overlay.is_some() || self.mode != ClientShellMode::Terminal {
            return None;
        }
        self.focused_pane_id()
            .map(|pane_id| format!("{endpoint}/{pane_id}"))
    }

    /// Replace the microphone level history to draw. Returns whether a repaint is needed.
    pub(crate) fn set_voice_meter(&mut self, meter: Option<Vec<f32>>) -> bool {
        if self.voice_meter == meter {
            return false;
        }
        self.voice_meter = meter;
        true
    }

    /// Draw the meter at the right end of `area`'s last row: a red dot, then one cell per level
    /// sample, where height and colour both carry loudness.
    pub(super) fn render_voice_meter(&self, buffer: &mut Buffer, area: Rect) -> Option<Rect> {
        let levels = self.voice_meter.as_ref()?;
        let width = u16::try_from(levels.len() + 3).ok()?;
        if area.is_empty() || area.width <= width {
            return None;
        }
        let bar = Rect::new(
            area.x + area.width - width,
            area.y + area.height - 1,
            width,
            1,
        );
        let palette = &self.config.palette;
        let base = Style::default().bg(palette.panel_bg);
        let mut x = bar.x;
        let mut put = |symbol: &str, style: Style| {
            buffer[(x, bar.y)].set_symbol(symbol).set_style(style);
            x += 1;
        };
        put(" ", base);
        put("●", base.fg(palette.red));
        put(" ", base);
        for level in levels {
            let (glyph, heat) = crate::client::voice::meter_cell(*level);
            let colour = match heat {
                0 => palette.overlay0,
                1 => palette.yellow,
                2 => palette.peach,
                _ => palette.red,
            };
            put(glyph.encode_utf8(&mut [0; 4]), base.fg(colour));
        }
        Some(bar)
    }
}
