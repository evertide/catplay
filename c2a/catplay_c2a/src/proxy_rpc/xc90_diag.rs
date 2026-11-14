//! XC90 diagnostics (diag branch only, pruned once the instrument cluster works): runtime switches read from
//! `/persist/config.txt`, so one build can test several hypotheses for the black cluster without rebuilding.

use log::info;
use std::sync::OnceLock;

const CONFIG_FILE: &str = "/persist/config.txt";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Xc90Diag {
    /// `xc90_alt_passthrough_config` (default 1): pass the iPhone's view origin and size on to the car.
    pub alt_passthrough_config: bool,
    /// `xc90_alt_resetup` (default 0): re-SETUP the car's alt screen stream whenever the iPhone's alt screen config changes.
    pub alt_resetup: bool,
    /// `xc90_alt_latency_ms` (default 25): latency announced in the car's alt screen SETUP (the iPhone uses 75).
    pub alt_latency_ms: u64,
}

impl Default for Xc90Diag {
    fn default() -> Self {
        Self {
            alt_passthrough_config: true,
            alt_resetup: false,
            alt_latency_ms: 25,
        }
    }
}

impl Xc90Diag {
    /// The switches from `/persist/config.txt`, read and logged once.
    pub fn get() -> Xc90Diag {
        static DIAG: OnceLock<Xc90Diag> = OnceLock::new();
        *DIAG.get_or_init(|| {
            let diag = Self::parse(&std::fs::read_to_string(CONFIG_FILE).unwrap_or_default());
            info!("XC90 diagnostic switches: {diag:?}");
            diag
        })
    }

    fn parse(text: &str) -> Xc90Diag {
        let mut diag = Xc90Diag::default();
        for (key, value) in text.lines().filter_map(|line| line.split_once('=')) {
            let (key, value) = (key.trim(), value.trim());
            match key {
                "xc90_alt_passthrough_config" => diag.alt_passthrough_config = value == "1",
                "xc90_alt_resetup" => diag.alt_resetup = value == "1",
                "xc90_alt_latency_ms" => diag.alt_latency_ms = value.parse().unwrap_or(diag.alt_latency_ms),
                _ => {}
            }
        }
        diag
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_switches_and_keeps_defaults() {
        assert_eq!(Xc90Diag::parse(""), Xc90Diag::default());
        let diag = Xc90Diag::parse(
            "bluetooth_name=Volvo XC90\nxc90_alt_resetup=1\nxc90_alt_passthrough_config = 0\nxc90_alt_latency_ms=75\n# xc90_alt_resetup=0\n",
        );
        assert_eq!(
            diag,
            Xc90Diag {
                alt_passthrough_config: false,
                alt_resetup: true,
                alt_latency_ms: 75
            }
        );
        assert_eq!(Xc90Diag::parse("xc90_alt_latency_ms=abc").alt_latency_ms, 25);
    }
}
