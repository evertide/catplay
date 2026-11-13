use std::collections::HashMap;

/// Live night mode as reported by the car: one global value (the main display's) plus overrides for displays
/// the car switched individually with a scoped setNightMode.
///
/// Displays without an override deliberately follow the global value, and the values from /info are not
/// seeded as overrides: the Volvo XC90 lists `nightMode` on its cluster but only sends setNightMode scoped to
/// the main display, so the cluster must follow the main display or it would stay dark (or light) forever.
#[derive(Debug, Default)]
pub struct NightModes {
    global: bool,
    per_display: HashMap<String, bool>,
}

impl NightModes {
    pub fn new(global: bool) -> Self {
        Self {
            global,
            per_display: HashMap::new(),
        }
    }

    /// Applies a setNightMode. Without a uuid it applies to every display; with the main display's uuid it also
    /// sets the global value.
    pub fn apply(&mut self, night_mode: bool, uuid: Option<&str>, main_uuid: Option<&str>) {
        match uuid {
            None => {
                self.global = night_mode;
                self.per_display.clear();
            }
            Some(uuid) => {
                if Some(uuid) == main_uuid {
                    self.global = night_mode;
                }
                self.per_display.insert(uuid.to_owned(), night_mode);
            }
        }
    }

    pub fn global(&self) -> bool {
        self.global
    }

    pub fn for_display(&self, uuid: &str) -> bool {
        self.per_display.get(uuid).copied().unwrap_or(self.global)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: &str = "main";
    const CLUSTER: &str = "cluster";

    #[test]
    fn scoped_updates_stay_on_their_display() {
        let mut modes = NightModes::new(false);
        modes.apply(true, Some(CLUSTER), Some(MAIN));
        assert!(!modes.global());
        assert!(!modes.for_display(MAIN));
        assert!(modes.for_display(CLUSTER));

        modes.apply(true, Some(MAIN), Some(MAIN));
        assert!(modes.global());

        // An unscoped update applies to every display again.
        modes.apply(false, None, Some(MAIN));
        assert!(!modes.for_display(CLUSTER));
        assert!(!modes.global());
    }
}
