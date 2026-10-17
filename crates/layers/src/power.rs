use std::fs;
use std::path::Path;

/// How eagerly vuoxi works, from most to least.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Mode {
    /// On mains: sync within seconds.
    Live,
    /// On battery: collect changes, send them in one burst per window.
    Batched,
    /// Low battery: only folders marked urgent; the rest waits for AC.
    Frugal,
    /// Paused: changes are journaled, nothing is hashed or sent.
    Hold,
}

impl Mode {
    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            0 => Mode::Live,
            1 => Mode::Batched,
            2 => Mode::Frugal,
            _ => Mode::Hold,
        }
    }
}

const FRUGAL_BELOW_PERCENT: u8 = 20;

/// What the machine's power supply looks like right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerState {
    pub on_ac: bool,
    /// Battery charge; `None` on machines without a battery.
    pub capacity: Option<u8>,
}

impl PowerState {
    /// Read `/sys/class/power_supply` (or a test tree shaped like it).
    /// A desktop with no supplies at all counts as mains.
    pub fn read(root: &Path) -> Self {
        let mut st = PowerState {
            on_ac: false,
            capacity: None,
        };
        let mut saw_mains = false;
        let Ok(dir) = fs::read_dir(root) else {
            return PowerState {
                on_ac: true,
                capacity: None,
            };
        };
        for e in dir.flatten() {
            let p = e.path();
            let read = |f: &str| {
                fs::read_to_string(p.join(f))
                    .ok()
                    .map(|s| s.trim().to_owned())
            };
            match read("type").as_deref() {
                Some("Mains") => {
                    saw_mains = true;
                    st.on_ac |= read("online").as_deref() == Some("1");
                }
                Some("Battery") => {
                    st.capacity = read("capacity").and_then(|c| c.parse().ok());
                }
                _ => {}
            }
        }
        if !saw_mains && st.capacity.is_none() {
            st.on_ac = true;
        }
        st
    }

    pub fn mode(self) -> Mode {
        match (self.on_ac, self.capacity) {
            (true, _) | (false, None) => Mode::Live,
            (false, Some(c)) if c < FRUGAL_BELOW_PERCENT => Mode::Frugal,
            (false, Some(_)) => Mode::Batched,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_policy() {
        let s = |on_ac, capacity| PowerState { on_ac, capacity }.mode();
        assert_eq!(s(true, None), Mode::Live);
        assert_eq!(s(true, Some(5)), Mode::Live);
        assert_eq!(s(false, Some(80)), Mode::Batched);
        assert_eq!(s(false, Some(19)), Mode::Frugal);
    }

    #[test]
    fn reads_a_sysfs_shaped_tree() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-power_supply");
        let _ = fs::remove_dir_all(&root);
        let supply = |name: &str, files: &[(&str, &str)]| {
            let d = root.join(name);
            fs::create_dir_all(&d).unwrap();
            for (f, v) in files {
                fs::write(d.join(f), format!("{v}\n")).unwrap();
            }
        };
        supply("ADP1", &[("type", "Mains"), ("online", "0")]);
        supply("BAT0", &[("type", "Battery"), ("capacity", "42")]);

        let st = PowerState::read(&root);
        assert_eq!(
            st,
            PowerState {
                on_ac: false,
                capacity: Some(42)
            }
        );
        assert_eq!(st.mode(), Mode::Batched);
    }

    #[test]
    fn no_supplies_means_desktop_on_mains() {
        let st = PowerState::read(Path::new("/nonexistent/power_supply"));
        assert_eq!(st.mode(), Mode::Live);
    }
}
