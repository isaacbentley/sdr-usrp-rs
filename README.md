# 📡 sdr-usrp-rs: Ettus USRP Interface

[![CI](https://github.com/isaacbentley/sdr-usrp-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/isaacbentley/sdr-usrp-rs/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/badge/rustc-1.85+-ab6000.svg)](https://blog.rust-lang.org/2025/02/20/Rust-1.85.0.html)

## 🎯 **What it does**

Ettus USRP B2xx implementation of the
[`SdrSource`](https://github.com/isaacbentley/sdr-source-rs) trait. Tested on the B210;
the B205mini works the same way (note: it only exposes `RX2`). Talks
to the device through the `uhd` 0.3 crate over UHD 4.x.

## 🔧 **Usage**

```rust
use sdr_usrp_rs::UsrpSource;
use sdr_source_rs::{DwellAdvice, SdrSource, SourceConfig};
use std::sync::Arc;
use std::time::{Duration, Instant};

// Stub DwellAdvice — the orchestrator normally implements this on
// AppState so the capture thread can read the per-channel signal
// log. Returning None disables adaptive extension; the controller
// falls back to the minimum dwell on every hop.
struct NoSignalLog;
impl DwellAdvice for NoSignalLog {
    fn latest_signal_at(&self, _freq_key_khz: u64) -> Option<Instant> { None }
}
let advice: Arc<dyn DwellAdvice> = Arc::new(NoSignalLog);

let source = Box::new(UsrpSource {
    args:      String::new(),       // empty = auto-discover
    gain_db:   40.0,
    antenna:   "RX2".into(),
});

let config = SourceConfig {
    sample_rate_hz:    15_360_000.0,
    channels_hz:       vec![2_412e6, 2_437e6, 2_462e6],
    dwell_min:         Duration::from_millis(60),
    dwell_max:         Duration::from_millis(500),
    dwell_extension:   Duration::from_millis(80),
};

let handle = source.start(config, advice)?;
for packet in handle.receiver.iter() {
    // packet.samples is a PooledIqBuffer (use like &[Complex32])
    // packet.center_frequency_hz tells you which channel was tuned
    // packet.sample_rate_hz is the SDR sample rate
}
```

## ⚙️ **Builder Fields**

| Field | Default | Notes |
|---|---|---|
| `args` | `""` | UHD device args, e.g. `"type=b200"` or `"serial=320XXXX"`. Empty lets UHD auto-discover. `master_clock_rate=...` is appended automatically based on the requested sample rate. |
| `gain_db` | `40.0` | B210 supports 0–76 dB. 40 dB is a mid value that doesn't saturate on strong ambient ISM traffic. |
| `antenna` | `"RX2"` | B210 has `RX1` / `RX2`; B205mini has `RX2` only. |

## 📦 **Dependencies**

```toml
sdr-source-rs = { git = "https://github.com/isaacbentley/sdr-source-rs.git", branch = "main" }
uhd           = "0.3"
crossbeam     = "0.8"
num-complex   = "0.4"
anyhow        = "1.0"
tracing       = "0.1"
```

## 📚 **Documentation**

- [Architecture & Design](DESIGN.md) — internal architecture and execution flow.

## 📄 **License**

This project is licensed under the GNU General Public License v3.0 or later (GPL-3.0-or-later) - see the [LICENSE](../../LICENSE) file for details.

## 📞 **Support**

- 🐛 **Issues**: [GitHub Issues](https://github.com/isaacbentley/sdr-usrp-rs/issues)
