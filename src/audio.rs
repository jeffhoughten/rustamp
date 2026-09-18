//! Audio output, one small interface with two implementations.
//!
//! Linux uses the ALSA C API directly. That isn't just preference: going
//! through cpal on 32-bit ARM (Raspberry Pi) segfaults inside
//! alsa::pcm::Status::get_htstamp, which cpal calls unconditionally when it
//! opens a stream. Talking to ALSA ourselves avoids that path entirely.
//!
//! Everything else (Windows x64/ARM64, macOS, and any platform cpal
//! supports) uses cpal, wrapped in a blocking ring buffer so the caller
//! sees the same push-samples-and-block behaviour ALSA gives us.

/// An opened output device accepting interleaved i16 frames.
///
/// Deliberately not `Send`: cpal's `Stream` is `!Send` on Windows (WASAPI
/// requires it stay on the thread that created it). Nothing here needs to
/// cross threads — the player thread opens its own output and drops it
/// before moving to the next track.
pub trait AudioOut {
    /// Writes interleaved samples, blocking until they're accepted.
    fn write(&mut self, samples: &[i16]) -> anyhow::Result<()>;
    /// Frames handed over but not yet heard. Used to rewind the decoder on
    /// pause so resuming doesn't skip audio.
    fn queued_frames(&self) -> u64;
    /// Throws away everything pending — for pause, seek, skip, stop.
    fn discard(&mut self);
    /// Blocks until pending audio has finished playing (end of track).
    fn drain(&mut self);
    /// Re-arms the device after discard().
    fn resume(&mut self);
}

/// Opens the default output, preferring the given format. Returns the
/// device alongside the format it *actually* opened with — callers must
/// resample to that if it differs. Windows (WASAPI shared mode) generally
/// only accepts the device's own mix format, so this is the common case
/// there; ALSA's `plug` layer converts for us, so Linux always gets what
/// it asked for.
pub fn open(sample_rate: u32, channels: u16) -> anyhow::Result<(Box<dyn AudioOut>, u32, u16)> {
    #[cfg(target_os = "linux")]
    {
        let out = alsa_out::AlsaOut::open(sample_rate, channels)?;
        Ok((Box::new(out), sample_rate, channels))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let (out, rate, ch) = cpal_out::CpalOut::open(sample_rate, channels)?;
        Ok((Box::new(out), rate, ch))
    }
}

// ---------------------------------------------------------------- Linux

#[cfg(target_os = "linux")]
mod alsa_out {
    use super::AudioOut;
    use alsa::pcm::PCM;

    pub struct AlsaOut {
        pcm: PCM,
    }

    impl AlsaOut {
        pub fn open(sample_rate: u32, channels: u16) -> anyhow::Result<Self> {
            use alsa::pcm::{Access, Format, HwParams};
            use alsa::{Direction, ValueOr};

            let pcm = PCM::new("default", Direction::Playback, false)?;
            {
                let hwp = HwParams::any(&pcm)?;
                hwp.set_channels(channels as u32)?;
                hwp.set_rate(sample_rate, ValueOr::Nearest)?;
                hwp.set_format(Format::S16LE)?;
                hwp.set_access(Access::RWInterleaved)?;
                // Short buffer (~250ms) so pause/seek/skip take effect
                // promptly; ALSA would otherwise queue seconds of audio.
                let _ = hwp.set_buffer_size_near((sample_rate / 4) as alsa::pcm::Frames);
                let _ = hwp.set_period_size_near(1024, ValueOr::Nearest);
                pcm.hw_params(&hwp)?;
            }
            pcm.prepare()?;
            Ok(Self { pcm })
        }
    }

    impl AudioOut for AlsaOut {
        fn write(&mut self, samples: &[i16]) -> anyhow::Result<()> {
            let io = self.pcm.io_i16()?;
            // On an xrun or the transient EIO the bcm2835 I2S driver throws
            // at stream start, recover and retry the same buffer so no
            // audio is lost.
            let mut attempts = 0;
            loop {
                match io.writei(samples) {
                    Ok(_) => return Ok(()),
                    Err(e) => {
                        attempts += 1;
                        let recovered = self.pcm.recover(e.errno() as i32, true).is_ok()
                            || self.pcm.prepare().is_ok();
                        if !recovered || attempts >= 3 {
                            return Err(e.into());
                        }
                    }
                }
            }
        }

        fn queued_frames(&self) -> u64 {
            self.pcm.delay().unwrap_or(0).max(0) as u64
        }

        fn discard(&mut self) {
            let _ = self.pcm.drop();
        }

        fn drain(&mut self) {
            let _ = self.pcm.drain();
        }

        fn resume(&mut self) {
            let _ = self.pcm.prepare();
        }
    }
}

// ------------------------------------------------------------ Everything else

#[cfg(not(target_os = "linux"))]
mod cpal_out {
    use super::AudioOut;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::collections::VecDeque;
    use std::sync::{Arc, Condvar, Mutex};

    struct Shared {
        buf: Mutex<VecDeque<i16>>,
        cv: Condvar,
    }

    pub struct CpalOut {
        _stream: cpal::Stream,
        shared: Arc<Shared>,
        channels: u16,
        capacity: usize, // samples, ~250ms
    }

    impl CpalOut {
        pub fn open(sample_rate: u32, channels: u16) -> anyhow::Result<(Self, u32, u16)> {
            let host = cpal::default_host();
            let device = host
                .default_output_device()
                .ok_or_else(|| anyhow::anyhow!("no default output device"))?;

            // Use the requested format only if the device actually supports
            // it; otherwise fall back to the device's default config and
            // report that back so the caller can resample.
            let wanted_supported = device
                .supported_output_configs()
                .map(|mut cfgs| {
                    cfgs.any(|c| {
                        c.channels() == channels
                            && c.sample_format() == cpal::SampleFormat::F32
                            && c.min_sample_rate().0 <= sample_rate
                            && sample_rate <= c.max_sample_rate().0
                    })
                })
                .unwrap_or(false);

            let config = if wanted_supported {
                cpal::StreamConfig {
                    channels,
                    sample_rate: cpal::SampleRate(sample_rate),
                    buffer_size: cpal::BufferSize::Default,
                }
            } else {
                let default = device.default_output_config()?;
                if default.sample_format() != cpal::SampleFormat::F32 {
                    anyhow::bail!(
                        "default output device uses {:?} samples, which isn't supported yet",
                        default.sample_format()
                    );
                }
                default.config()
            };
            let out_rate = config.sample_rate.0;
            let out_channels = config.channels;

            let shared = Arc::new(Shared {
                buf: Mutex::new(VecDeque::new()),
                cv: Condvar::new(),
            });
            let cb_shared = shared.clone();

            let stream = device.build_output_stream(
                &config,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut buf = cb_shared.buf.lock().unwrap();
                    for slot in out.iter_mut() {
                        *slot = match buf.pop_front() {
                            Some(s) => s as f32 / i16::MAX as f32,
                            None => 0.0, // underrun: silence rather than noise
                        };
                    }
                    drop(buf);
                    cb_shared.cv.notify_all();
                },
                |e| eprintln!("audio stream error: {e}"),
                None,
            )?;
            stream.play()?;

            Ok((
                Self {
                    _stream: stream,
                    shared,
                    channels: out_channels.max(1),
                    capacity: (out_rate as usize / 4) * out_channels.max(1) as usize,
                },
                out_rate,
                out_channels,
            ))
        }
    }

    impl AudioOut for CpalOut {
        fn write(&mut self, samples: &[i16]) -> anyhow::Result<()> {
            let mut buf = self.shared.buf.lock().unwrap();
            // Block while the buffer is full, mirroring ALSA's behaviour;
            // the timeout keeps us from hanging if the device stalls.
            let mut waited = 0u32;
            while buf.len() + samples.len() > self.capacity {
                let (b, timeout) = self
                    .shared
                    .cv
                    .wait_timeout(buf, std::time::Duration::from_millis(100))
                    .unwrap();
                buf = b;
                if timeout.timed_out() {
                    waited += 1;
                    if waited > 50 {
                        anyhow::bail!("audio device stopped consuming samples");
                    }
                }
            }
            buf.extend(samples.iter().copied());
            Ok(())
        }

        fn queued_frames(&self) -> u64 {
            let buf = self.shared.buf.lock().unwrap();
            (buf.len() / self.channels as usize) as u64
        }

        fn discard(&mut self) {
            self.shared.buf.lock().unwrap().clear();
        }

        fn drain(&mut self) {
            let mut buf = self.shared.buf.lock().unwrap();
            let mut waited = 0u32;
            while !buf.is_empty() {
                let (b, timeout) = self
                    .shared
                    .cv
                    .wait_timeout(buf, std::time::Duration::from_millis(100))
                    .unwrap();
                buf = b;
                if timeout.timed_out() {
                    waited += 1;
                    if waited > 50 {
                        break;
                    }
                }
            }
        }

        fn resume(&mut self) {}
    }
}
