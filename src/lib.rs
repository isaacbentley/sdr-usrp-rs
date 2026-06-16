//! USRP-family SDR source for SDR applications.
//!
//! Implements [`sdr_source_rs::SdrSource`] for Ettus USRP devices
//! (tested on B210; B205mini should also work). Owns the device
//! handle, the channel-hop loop, and the IQ buffer allocation. The
//! orchestrator consumes [`IqPacket`]s through the receiver returned
//! in [`SdrHandle`].

use crossbeam::channel;
use num_complex::Complex32;
use sdr_source_rs::{
    DwellAdvice, DwellController, IqPacket, SdrError, SdrHandle, SdrSource, SourceConfig,
    freq_key_khz,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tracing::info;
use uhd::{StreamCommand, StreamCommandType, StreamTime, TuneRequest};

/// Builder for a USRP source. Wrap in `Box::new(...)` and call
/// [`SdrSource::start`] from the orchestrator.
pub struct UsrpSource {
    /// UHD device args. Empty string lets UHD auto-discover. Common
    /// values: `"type=b200"`, `"serial=320XXXX"`.
    pub args: String,
    /// RX gain in dB. The B210 supports 0–76 dB; we default to a mid
    /// value (40 dB) that doesn't saturate on strong ambient ISM
    /// traffic.
    pub gain_db: f64,
    /// RX antenna port. B210 exposes `RX1` and `RX2`; B205mini has
    /// `RX2` only. Default `RX2`.
    pub antenna: String,
}

impl Default for UsrpSource {
    fn default() -> Self {
        Self {
            args: String::new(),
            gain_db: 40.0,
            antenna: "RX2".to_string(),
        }
    }
}

impl SdrSource for UsrpSource {
    fn start(
        self: Box<Self>,
        config: SourceConfig,
        advice: Arc<dyn DwellAdvice>,
    ) -> Result<SdrHandle, SdrError> {
        let sample_rate = config.sample_rate_hz;

        // Optimal Master Clock Selection: highest integer decimation
        // (up to 4×) within the 61.44 MHz limit. Each 4× oversampling
        // step yields ~1 additional bit of ENOB at the cost of more
        // FPGA work, which the B210 can deliver up to its ceiling.
        let (master_clock, decimation) = if sample_rate * 4.0 <= 61.44e6 {
            (sample_rate * 4.0, 4)
        } else if sample_rate * 2.0 <= 61.44e6 {
            (sample_rate * 2.0, 2)
        } else {
            (sample_rate, 1)
        };

        let bit_gain = (decimation as f32).log2() * 0.5;
        let total_bits = 12.0 + bit_gain;

        info!(
            "[usrp] Configuring hardware: Rate={:.2} MSPS | Clock={:.2} MHz | Decimation={}x",
            sample_rate / 1e6,
            master_clock / 1e6,
            decimation
        );
        info!(
            "[usrp] Signal Quality: Effective ADC Resolution = {:.2} bits (+{:.2} bits gain)",
            total_bits, bit_gain
        );

        let dev_args = if self.args.is_empty() {
            format!("master_clock_rate={}", master_clock)
        } else {
            format!("{},master_clock_rate={}", self.args, master_clock)
        };

        let devices =
            uhd::Usrp::find("").map_err(|e| SdrError::Io(format!("USRP find failed: {e}")))?;
        if devices.is_empty() {
            return Err(SdrError::NotFound(
                "No USRP devices found. Ensure the USRP is connected and powered on.".into(),
            ));
        }

        info!("[usrp] Opening device with args: \"{}\"", dev_args);
        let mut usrp = uhd::Usrp::open(&dev_args)
            .map_err(|e| SdrError::Io(format!("USRP open failed: {e}")))?;

        usrp.set_rx_sample_rate(sample_rate, 0)
            .map_err(|e| SdrError::BadConfig(format!("set_rx_sample_rate({sample_rate}): {e}")))?;
        usrp.set_rx_gain(self.gain_db, 0, "")
            .map_err(|e| SdrError::BadConfig(format!("set_rx_gain({}): {e}", self.gain_db)))?;
        usrp.set_rx_antenna(&self.antenna, 0)
            .map_err(|e| SdrError::BadConfig(format!("set_rx_antenna({}): {e}", self.antenna)))?;

        let dwell_controller = DwellController {
            min: config.dwell_min,
            max: config.dwell_max,
            extension: config.dwell_extension,
        };

        let channels_hz = config.channels_hz.clone();
        let num_channels = channels_hz.len();
        if num_channels == 0 {
            return Err(SdrError::BadConfig(
                "SourceConfig.channels_hz must not be empty".into(),
            ));
        }
        if dwell_controller.is_adaptive() {
            info!(
                "[usrp] Starting scan: {} channels, adaptive dwell {}–{}ms (+{}ms per detection)",
                num_channels,
                config.dwell_min.as_millis(),
                config.dwell_max.as_millis(),
                config.dwell_extension.as_millis()
            );
        } else {
            info!(
                "[usrp] Starting scan: {} channels, fixed {}ms dwell per channel",
                num_channels,
                config.dwell_min.as_millis()
            );
        }

        let (tx, receiver) = channel::bounded::<IqPacket>(1024);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_thread = stop_flag.clone();
        let advice_thread = advice;
        let sample_rate_f32 = sample_rate as f32;

        let capture_thread = thread::spawn(move || {
            if let Err(e) = (move || -> Result<(), anyhow::Error> {
                // uhd 0.3.0's `Usrp::get_rx_stream` and `Usrp::set_rx_frequency` both take
                // `&mut self`, and `ReceiveStreamer<'_>` holds the mutable borrow for its
                // lifetime. We therefore have to recreate the streamer per hop. Eliminating
                // that overhead requires either upgrading/forking the uhd binding or dropping
                // to uhd-sys and managing the C handle ourselves. For now we lift everything
                // that *can* live outside the loop and accept the per-hop streamer teardown.
                let stream_args = uhd::StreamArgs::builder()
                    .wire_format("sc16".to_string())
                    .args("num_recv_frames=1024".to_string())
                    .build();

                // Pre-allocate the vector recycling pool
                let (pool_tx, pool_rx) = channel::bounded::<Vec<Complex32>>(256);
                for _ in 0..256 {
                    let _ = pool_tx.send(vec![Complex32::new(0.0, 0.0); 65536]);
                }

                let mut last_report = Instant::now();
                let mut channel_switches = 0;
                let mut channel_idx = 0;

                'outer: loop {
                    if stop_flag_thread.load(Ordering::SeqCst) {
                        break;
                    }
                    let current_freq_hz = channels_hz[channel_idx];
                    let freq_key = freq_key_khz(current_freq_hz);
                    usrp.set_rx_frequency(&TuneRequest::with_frequency(current_freq_hz), 0)?;

                    let mut rx_stream = usrp.get_rx_stream(&stream_args)?;
                    rx_stream.send_command(&StreamCommand {
                        command_type: StreamCommandType::StartContinuous,
                        time: StreamTime::Now,
                    })?;

                    let dwell_start = Instant::now();
                    loop {
                        if stop_flag_thread.load(Ordering::SeqCst) {
                            rx_stream.send_command(&StreamCommand {
                                command_type: StreamCommandType::StopContinuous,
                                time: StreamTime::Now,
                            })?;
                            drop(rx_stream);
                            break 'outer;
                        }
                        let now_loop = Instant::now();
                        let latest_signal = advice_thread.latest_signal_at(freq_key);
                        let deadline = dwell_controller.deadline(dwell_start, latest_signal);
                        if now_loop >= deadline {
                            break;
                        }

                        // Borrow an empty buffer from the pool (or allocate if heavily backed up)
                        let mut raw_buffer = Some(
                            pool_rx
                                .try_recv()
                                .unwrap_or_else(|_| vec![Complex32::new(0.0, 0.0); 65536]),
                        );
                        {
                            // Present a full-length 65536-element buffer to
                            // `receive` without paying for a 512 KB zero-fill
                            // every iteration: UHD overwrites `[0..n]` and we
                            // discard the tail, so the memset was pure waste.
                            let buf = raw_buffer.as_mut().unwrap();
                            buf.reserve(65536_usize.saturating_sub(buf.len()));
                            // SAFETY: We pre-initialized the vector capacity
                            // via `vec![Complex32::new(0.0, 0.0); 65536]`. Thus,
                            // `.set_len(65536)` exposes fully initialized (though
                            // stale) elements, which is perfectly sound. 
                            // WARNING: Do not swap the allocation to `Vec::with_capacity` 
                            // or this will trigger UB.
                            #[allow(clippy::uninit_vec)]
                            unsafe {
                                buf.set_len(65536);
                            }
                        }

                        let mut put_back = true;
                        let mut buffers = [&mut raw_buffer.as_mut().unwrap()[..]];
                        if let Ok(meta) = rx_stream.receive(&mut buffers, 0.05, false) {
                            let n = meta.samples().min(raw_buffer.as_ref().unwrap().len());
                            if n > 0 {
                                let is_overrun = if let Some(err) = meta.last_error() {
                                    err.to_string().contains("Overflow")
                                } else {
                                    false
                                };

                                let mut buf = raw_buffer.take().unwrap();
                                // Truncate the vector to exactly n samples (capacity is maintained)
                                buf.truncate(n);

                                let pkt = IqPacket {
                                    samples: sdr_source_rs::PooledIqBuffer::new_pooled(
                                        buf,
                                        pool_tx.clone(),
                                    ),
                                    center_frequency_hz: current_freq_hz,
                                    sample_rate_hz: sample_rate_f32,
                                    overrun: is_overrun,
                                };
                                if tx.send(pkt).is_err() {
                                    // Receiver dropped — wind down.
                                    rx_stream.send_command(&StreamCommand {
                                        command_type: StreamCommandType::StopContinuous,
                                        time: StreamTime::Now,
                                    })?;
                                    drop(rx_stream);
                                    break 'outer;
                                }
                                put_back = false;
                            }
                        }

                        if put_back {
                            if let Some(buf) = raw_buffer {
                                let _ = pool_tx.send(buf);
                            }
                        }

                        let elapsed = now_loop.duration_since(last_report);
                        if elapsed >= Duration::from_secs(60) {
                            let rate =
                                channel_switches as f32 / elapsed.as_secs_f32();
                            info!(
                                "[usrp] Scanning speed: {:.1} ch/s | Pool size: {} channels",
                                rate, num_channels
                            );
                            channel_switches = 0;
                            last_report = now_loop;
                        }
                    }

                    rx_stream.send_command(&StreamCommand {
                        command_type: StreamCommandType::StopContinuous,
                        time: StreamTime::Now,
                    })?;
                    drop(rx_stream);

                    channel_idx = (channel_idx + 1) % num_channels;
                    channel_switches += 1;
                }
                Ok(())
            })() {
                tracing::error!("[usrp] Capture thread failed: {:?}", e);
            }
        });

        let stop_flag_for_stop = stop_flag.clone();
        let stop = Box::new(move || {
            stop_flag_for_stop.store(true, Ordering::SeqCst);
        });
        let wait = Box::new(move || {
            if let Err(e) = capture_thread.join() {
                tracing::error!("[usrp] capture thread join failed: {:?}", e);
            }
        });

        Ok(SdrHandle {
            receiver,
            stop,
            wait,
        })
    }
}

/// Probe the connected USRP for its maximum supported RX sample rate.
///
/// Opens the device (with optional `usrp_args`), queries
/// `get_rx_sample_rates(0)` → `MetaRange`, and returns the highest
/// `stop()` value across all sub-ranges. The device handle is dropped
/// immediately — no streaming is started.
///
/// For B210 this returns 61.44 MSPS, B205mini 56 MSPS, N310
/// 122.88 MSPS, etc.
pub fn query_max_rx_rate(usrp_args: &str) -> Result<f64, SdrError> {
    let usrp =
        uhd::Usrp::open(usrp_args).map_err(|e| SdrError::Io(format!("USRP open failed: {e}")))?;
    let meta_range = usrp
        .get_rx_sample_rates(0)
        .map_err(|e| SdrError::Io(format!("get_rx_sample_rates failed: {e}")))?;
    let max_rate = meta_range
        .stop()
        .map_err(|e| SdrError::Io(format!("MetaRange::stop() failed: {e}")))?;
    if max_rate <= 0.0 {
        return Err(SdrError::BadConfig(
            "USRP returned no valid RX sample rates".into(),
        ));
    }
    Ok(max_rate)
}
