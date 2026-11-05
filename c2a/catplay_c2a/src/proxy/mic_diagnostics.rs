//! Aggregate PCM diagnostics only. Never retain or log speech samples.
#[derive(Default, Debug)]
pub struct MicWindow {
    pub samples: u64,
    pub nonzero: u64,
    pub peak: u16,
    pub incomplete_reads: u64,
}

impl MicWindow {
    pub fn observe(&mut self, samples: &[i16], complete: bool) {
        self.samples += samples.len() as u64;
        self.nonzero += samples.iter().filter(|&&sample| sample != 0).count() as u64;
        self.peak = samples
            .iter()
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap_or(0)
            .max(self.peak);
        self.incomplete_reads += u64::from(!complete);
    }

    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_no_samples_silence_and_signal() {
        let mut window = MicWindow::default();
        assert_eq!(window.samples, 0);
        window.observe(&[0; 240], true);
        assert_eq!((window.samples, window.nonzero, window.peak), (240, 0, 0));
        window.observe(&[0, -200, 100], true);
        assert_eq!((window.samples, window.nonzero, window.peak), (243, 2, 200));
    }

    #[test]
    fn full_negative_range_and_window_reset() {
        let mut window = MicWindow::default();
        window.observe(&[i16::MIN, i16::MAX], false);
        let report = window.take();
        assert_eq!((report.peak, report.incomplete_reads), (32768, 1));
        assert_eq!((window.samples, window.nonzero, window.peak, window.incomplete_reads), (0, 0, 0, 0));
    }
}
