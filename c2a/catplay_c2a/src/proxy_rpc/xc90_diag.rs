//! XC90 diagnostics (diag branch only, pruned once the instrument cluster works): runtime switches read from
//! `/persist/config.txt`, so one build can test several hypotheses for the black cluster without rebuilding.

use catplay_carplay::video::AvccConfigExtended;
use std::sync::OnceLock;

const CONFIG_FILE: &str = "/persist/config.txt";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Xc90Diag {
    /// `xc90_alt_config` (default 1): which VideoConfig header params of the iPhone's alt screen stream go to the car, see
    /// [Self::apply_config].
    pub alt_config: u8,
    /// `xc90_main_config` (default 1): the same for the main screen stream.
    pub main_config: u8,
    /// `xc90_suggest_ui` (default 1): forward the iPhone's suggestUI command to the car (the stock firmware does).
    pub suggest_ui: bool,
    /// `xc90_config_flags` (default 1): VideoConfig flag bytes like the stock firmware (see `XC90_STOCK_CONFIG_FLAGS`).
    pub config_flags: bool,
    /// `xc90_alt_lazy_setup` (default 1): SETUP the car's alt screen stream at the iPhone's first alt screen keyframe instead of
    /// at the iPhone's SETUP, so the car gets VideoConfig and frames right after its SETUP (the stock firmware does).
    pub alt_lazy_setup: bool,
    /// `xc90_alt_resetup` (default 0): re-SETUP the car's alt screen stream whenever the iPhone's alt screen config changes.
    pub alt_resetup: bool,
    /// `xc90_alt_latency_ms` (default 25): latency announced in the car's alt screen SETUP (the iPhone uses 75).
    pub alt_latency_ms: u64,
}

impl Default for Xc90Diag {
    fn default() -> Self {
        Self {
            alt_config: 1,
            main_config: 1,
            suggest_ui: true,
            config_flags: true,
            alt_lazy_setup: true,
            alt_resetup: false,
            alt_latency_ms: 25,
        }
    }
}

impl Xc90Diag {
    /// The switches from `/persist/config.txt`, read once (the car manager logs them at the start of every car session).
    pub fn get() -> Xc90Diag {
        static DIAG: OnceLock<Xc90Diag> = OnceLock::new();
        *DIAG.get_or_init(|| {
            let diag = Self::parse(&std::fs::read_to_string(CONFIG_FILE).unwrap_or_default());
            catplay_carplay::screen::XC90_STOCK_CONFIG_FLAGS.store(diag.config_flags, std::sync::atomic::Ordering::Relaxed);
            diag
        })
    }

    /// VideoConfig header params passed on to the car: 0 = size only (build 13), 1 = plus view origin and size, params 3 and 4
    /// (build 14; what Apple's receiver code reads), 2 = plus param 6 (everything the iPhone sends except its timestamp).
    pub fn apply_config(mode: u8, config: &mut AvccConfigExtended) {
        match mode {
            0 => config.view = Default::default(),
            1 => config.view[2] = Default::default(),
            _ => {}
        }
    }

    fn parse(text: &str) -> Xc90Diag {
        let mut diag = Xc90Diag::default();
        for (key, value) in text.lines().filter_map(|line| line.split_once('=')) {
            let (key, value) = (key.trim(), value.trim());
            match key {
                "xc90_alt_config" => diag.alt_config = value.parse().unwrap_or(diag.alt_config),
                "xc90_main_config" => diag.main_config = value.parse().unwrap_or(diag.main_config),
                "xc90_suggest_ui" => diag.suggest_ui = value == "1",
                "xc90_config_flags" => diag.config_flags = value == "1",
                "xc90_alt_lazy_setup" => diag.alt_lazy_setup = value == "1",
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
            "bluetooth_name=Volvo XC90\nxc90_alt_resetup=1\nxc90_alt_config = 0\nxc90_main_config=2\nxc90_suggest_ui=0\nxc90_config_flags=0\nxc90_alt_lazy_setup=0\nxc90_alt_latency_ms=75\n# xc90_alt_resetup=0\n",
        );
        assert_eq!(
            diag,
            Xc90Diag {
                alt_config: 0,
                main_config: 2,
                suggest_ui: false,
                config_flags: false,
                alt_lazy_setup: false,
                alt_resetup: true,
                alt_latency_ms: 75
            }
        );
        assert_eq!(Xc90Diag::parse("xc90_alt_latency_ms=abc").alt_latency_ms, 25);
    }

    #[test]
    fn config_modes_select_the_header_params_sent_to_the_car() {
        let config = |mode: u8| {
            let mut config = AvccConfigExtended {
                hevc: false,
                avcc: catplay_carplay::video::AvccConfig {
                    nal_size_len: 4,
                    sps_pps: vec![1, 2],
                },
                video_latency: std::time::Duration::ZERO,
                width: 1920,
                height: 720,
                respect_timestamps: true,
                view: [[3; 8], [4; 8], [6; 8]],
            };
            Xc90Diag::apply_config(mode, &mut config);
            config.view
        };
        assert_eq!(config(0), [[0; 8]; 3], "build 13: size only");
        assert_eq!(config(1), [[3; 8], [4; 8], [0; 8]], "build 14: view origin and size");
        assert_eq!(config(2), [[3; 8], [4; 8], [6; 8]], "everything the iPhone sends");
    }
}
