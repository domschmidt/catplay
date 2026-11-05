use std::{
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use catplay_carplay::{
    audio::{AudioPlayer, AudioRecorder, AudioSinkBox, AudioSourceBox, RingBuffer, RingConsumer, RingProducer},
    carplay_tx::TeardownGuard,
    msg::StreamType,
    rtsp_frame::RtspResult,
};
use log::{debug, info};
use tokio::{task::JoinHandle, time::sleep};

#[path = "mic_diagnostics.rs"]
mod mic_diagnostics;

#[inline]
pub fn duration_to_frames_u32_saturating(d: Duration, freq: usize) -> u32 {
    let ns = d.as_nanos(); // u128
    let freq = freq as u128;

    let samples = (ns * freq) / 1_000_000_000u128;

    samples.min(u32::MAX as u128) as u32
}

/// Represents along with [PcmProxyRecorder] a loopback pair of AudioPlayer and AudioRecorder that exchange PCM samples using
/// ALSA-like high accuracy timing thread and [RingBuffer].
pub struct PcmProxyPlayer {
    ring: Option<(RingProducer<i16>, RingConsumer<i16>)>,
    source: Option<AudioSourceBox<i16>>,
    task: Option<JoinHandle<()>>,
    pub guard: Arc<Mutex<Option<TeardownGuard<StreamType>>>>,

    sample_rate: u32,
    channels_per_frame: usize,
    interval: Duration,
    receiver: Option<mpsc::Receiver<AudioSinkBox<i16>>>,
    limit: usize,
    mic_diagnostics: bool,
}

impl PcmProxyPlayer {
    pub fn new(
        ring: (RingProducer<i16>, RingConsumer<i16>),
        sample_rate: u32,
        interval: Duration,
        channels_per_frame: usize,
        receiver: mpsc::Receiver<AudioSinkBox<i16>>,
        limit: usize,
    ) -> Self {
        Self {
            ring: Some(ring),
            source: None,
            task: None,
            guard: Default::default(),

            sample_rate,
            channels_per_frame,
            interval,
            receiver: Some(receiver),
            limit,
            mic_diagnostics: false,
        }
    }

    pub fn pair(sample_rate: u32, channels_per_frame: usize) -> (PcmProxyPlayer, PcmProxyRecorder) {
        let _latency = Duration::from_millis(32);

        const INTERVAL: Duration = Duration::from_millis(10);
        const RING_INTERVAL_MULTIPLIER: usize = 50; // 10ms interval, 500ms ring

        let channel = mpsc::channel();

        let ring_size_samples =
            duration_to_frames_u32_saturating(INTERVAL, sample_rate as _) as usize * channels_per_frame * RING_INTERVAL_MULTIPLIER;

        let ring = RingBuffer::spsc(ring_size_samples);
        let player = PcmProxyPlayer::new(ring, sample_rate, INTERVAL, channels_per_frame, channel.1, ring_size_samples);
        let recorder = PcmProxyRecorder::new(channel.0);
        (player, recorder)
    }

    pub fn microphone_pair(sample_rate: u32, channels_per_frame: usize) -> (PcmProxyPlayer, PcmProxyRecorder) {
        let (mut player, recorder) = Self::pair(sample_rate, channels_per_frame);
        player.mic_diagnostics = true;
        (player, recorder)
    }
}

impl AudioPlayer for PcmProxyPlayer {
    type Sample = i16;

    fn init(&mut self, source: AudioSourceBox<Self::Sample>) -> RtspResult<()> {
        let _ring = self.ring.take().unwrap();
        self.source.replace(source);

        Ok(())
    }

    fn start(&mut self) {
        debug!("PcmProxyPlayer: start");
        if self.task.is_some() {
            return;
        }

        let Some(source) = self.source.take() else {
            debug!("PcmProxyPlayer: start without source");
            return;
        };
        let receiver = self.receiver.take().unwrap();
        let mut recorder = None;
        let _limit = self.limit;
        let mut start = Instant::now();

        // We have a lot of buffered audio, but don't stream too much ahead or receiver will start dropping data
        // (hard limit is ~20*latency_ms, depends on version of CarPlay SDK)
        let read_ahead = Duration::from_millis(150);
        let sample_rate = self.sample_rate;
        let channels_per_frame = self.channels_per_frame;

        let interval = self.interval;
        let diagnose_mic = self.mic_diagnostics;
        let task = tokio::spawn(async move {
            let mut player = source;
            let mut mic_window = mic_diagnostics::MicWindow::default();
            let mut mic_report_at = Instant::now() + Duration::from_secs(1);
            if diagnose_mic {
                info!("Mic diagnostics started: rate={sample_rate} channels={channels_per_frame}");
            }

            loop {
                let v = match recorder.as_mut() {
                    None => {
                        if let Ok(r) = receiver.try_recv() {
                            debug!("Received recorder!");
                            recorder.replace(r);
                            start = Instant::now();
                        }
                        None
                    }
                    Some(v) => Some(v),
                };

                if let Some(v) = v {
                    let stat = player.stat_raw();

                    let now = Instant::now();

                    let last_allowed_sample =
                        duration_to_frames_u32_saturating(now - start + read_ahead, sample_rate as _) * channels_per_frame as u32;
                    let allowed_samples = last_allowed_sample.min(stat.written as _);

                    let readable = allowed_samples.saturating_sub(stat.consumed as _);
                    debug!("??? readable = {readable} stat={:?}", player.stat());

                    let mut buf = vec![0i16; readable as _];

                    let complete = player.read(&mut buf);
                    if diagnose_mic {
                        mic_window.observe(&buf, complete);
                    }
                    let last_allowed = last_allowed_sample;
                    let min_src = if last_allowed <= stat.written as u32 { "wall" } else { "written" };

                    debug!(
                        "gate src={min_src} now-start={:?} last_allowed={} written={} consumed={} readable={}",
                        now - start,
                        last_allowed,
                        stat.written,
                        stat.consumed,
                        readable
                    );

                    v.write(&buf);
                }

                if diagnose_mic && Instant::now() >= mic_report_at {
                    let stat = player.stat_raw();
                    let window = mic_window.take();
                    let attached = recorder.is_some();
                    info!(
                        "Mic diagnostics: source_written={} source_consumed={} forwarded={} nonzero={} peak={} incomplete_reads={} recorder_attached={attached}",
                        stat.written, stat.consumed, window.samples, window.nonzero, window.peak, window.incomplete_reads
                    );
                    mic_report_at = Instant::now() + Duration::from_secs(2);
                }

                sleep(interval).await;
            }
        });
        self.task.replace(task);
    }

    fn stop(&mut self, _drain: bool) {
        debug!("PcmProxyPlayer: stop");
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for PcmProxyPlayer {
    fn drop(&mut self) {
        debug!("PcmProxyPlayer: drop");
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub struct PcmProxyRecorder {
    sender: mpsc::Sender<AudioSinkBox<i16>>,
    pub guard: Arc<Mutex<Option<TeardownGuard<StreamType>>>>,
}

impl PcmProxyRecorder {
    pub fn new(sender: mpsc::Sender<AudioSinkBox<i16>>) -> Self {
        Self {
            guard: Default::default(),
            sender,
        }
    }
}

impl AudioRecorder for PcmProxyRecorder {
    type Sample = i16;

    fn init(&mut self, source: AudioSinkBox<Self::Sample>) -> RtspResult<()> {
        if let Err(err) = self.sender.send(source) {
            debug!("PcmProxyRecorder failed to send source: {err:?}");
        }

        Ok(())
    }

    fn start(&mut self) {
        debug!("PcmProxyRecorder: start (ignored)");
    }

    fn stop(&mut self, drain: bool) {
        debug!("PcmProxyRecorder: stop {drain} (ignored)");
    }
}

impl Drop for PcmProxyRecorder {
    fn drop(&mut self) {
        debug!("PcmProxyRecorder: drop");
    }
}
