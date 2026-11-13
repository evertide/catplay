use std::{io::ErrorKind, path::Path};

use log::{info, warn};
use serde::Deserialize;

/// Experimental AirPlay iAP2 is deliberately opt-in. Read once at gadget startup.
#[derive(Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GpsConfig {
    pub airplay_iap2: bool,
}

impl GpsConfig {
    pub fn load(persist_dir: Option<&Path>) -> Self {
        let Some(dir) = persist_dir else {
            return Self::default();
        };
        let path = dir.join("gps.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                if err.kind() != ErrorKind::NotFound {
                    warn!("Could not read {}: {err}; AirPlay iAP2 stays disabled", path.display());
                }
                return Self::default();
            }
        };
        match toml::from_str::<Self>(&text) {
            Ok(config) => {
                if config.airplay_iap2 {
                    info!("Experimental AirPlay iAP2 GPS endpoint enabled by {}", path.display());
                }
                config
            }
            Err(err) => {
                warn!("Invalid {}: {err}; AirPlay iAP2 stays disabled", path.display());
                Self::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn airplay_requires_an_explicit_true_value() {
        assert!(!GpsConfig::load(None).airplay_iap2);
        assert!(!toml::from_str::<GpsConfig>("").unwrap().airplay_iap2);
        assert!(!toml::from_str::<GpsConfig>("airplay_iap2 = false").unwrap().airplay_iap2);
        assert!(toml::from_str::<GpsConfig>("airplay_iap2 = true").unwrap().airplay_iap2);
        assert!(toml::from_str::<GpsConfig>("airplay_iap2 = 'true'").is_err());
        assert!(toml::from_str::<GpsConfig>("airplay_iap2 = true\nairplay_iap = true").is_err());
    }
}
