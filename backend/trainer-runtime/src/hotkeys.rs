//! Global hotkeys for memory records — parsed, reported, and deliberately not
//! registered.
//!
//! Tables bind keys to records (`<Hotkeys>` in the CT). Actions follow
//! Hotkey action values:
//!
//! | value | action |
//! |-------|--------|
//! | 0,1,2 | toggle activation |
//! | 3     | activate |
//! | 4     | deactivate |
//! | 5     | set value |
//! | 6     | increase value |
//! | 7     | decrease value |
//!
//! **Nothing here calls `RegisterHotKey`, and that is a decision rather than a
//! gap.** A trainer runs under Proton, launched from a Decky panel, on a
//! device in Gaming Mode where gamescope and Steam own the input path. A
//! `RegisterHotKey` with a null window would very likely *succeed* and then
//! never receive a `WM_HOTKEY`, which is precisely the silent-success failure
//! class CLAUDE.md exists to prevent: the log would read "registered 12
//! hotkeys" and no key would ever do anything.
//!
//! So the bindings are still parsed — a table that declares them is not
//! malformed, and the count is worth reporting — and the log names the REST
//! endpoints that drive the same actions. The panel is the input device here.
//!
//! If global capture is ever wanted, the honest implementations are a Wine
//! window with a real message pump, or evdev on the Linux side plus a
//! `VK_*`→`KEY_*` table. Both are real work; neither is a two-line change to
//! this file.

use crate::memory_record::MemoryRecord;
use crate::rest_server::SharedState;
use tracing::warn;

/// One parsed binding: which record it drives and what it would do.
#[derive(Debug, Clone)]
struct Binding {
    #[allow(dead_code)]
    record_id: u32,
    #[allow(dead_code)]
    action: u32,
    description: String,
}

/// Accepts both the numeric form and the symbolic names.
fn parse_action(s: &str) -> u32 {
    if let Ok(n) = s.trim().parse::<u32>() {
        return n;
    }
    // CT files store the display string (marked
    // "DO NOT TRANSLATE"); the enum names appear in hand-written tables.
    match s.trim() {
        "Toggle Activation" | "mrhToggleActivation" => 0,
        "Toggle Activation Allow Increase" | "mrhToggleActivationAllowIncrease" => 1,
        "Toggle Activation Allow Decrease" | "mrhToggleActivationAllowDecrease" => 2,
        "Activate" | "mrhActivate" => 3,
        "Deactivate" | "mrhDeactivate" => 4,
        "Set Value" | "mrhSetValue" => 5,
        "Increase Value" | "mrhIncreaseValue" => 6,
        "Decrease Value" | "mrhDecreaseValue" => 7,
        _ => 0,
    }
}

fn hotkeys_of(records: &[MemoryRecord]) -> Vec<Binding> {
    fn walk(recs: &[MemoryRecord], out: &mut Vec<Binding>) {
        for rec in recs {
            for hk in &rec.hotkeys {
                out.push(Binding {
                    record_id: rec.id,
                    action: parse_action(&hk.action),
                    description: if hk.description.is_empty() {
                        rec.description.clone()
                    } else {
                        hk.description.clone()
                    },
                });
            }
            walk(&rec.children, out);
        }
    }
    let mut out = Vec::new();
    walk(records, &mut out);
    out
}

/// Reports the table's hotkey bindings and the REST calls that replace them.
///
/// Named `start_*` for symmetry with `start_freeze_loop`, but it starts
/// nothing: there is no listener to run. It takes the lock briefly and
/// returns.
pub fn start_hotkey_listener(state: SharedState) {
    let bindings = {
        let guard = match state.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        hotkeys_of(&guard.records)
    };

    if bindings.is_empty() {
        return;
    }

    // Name a few so this is recognisable as *this table's* bindings rather
    // than a generic notice, but do not print 200 of them.
    let sample: Vec<&str> = bindings
        .iter()
        .take(3)
        .map(|b| b.description.as_str())
        .collect();

    warn!(
        "{} hotkey binding(s) parsed from this table (e.g. {}) will not fire: a trainer \
         runs under Proton with Steam and gamescope owning the input path, so global key \
         capture is not implemented. Drive the same actions over REST instead: POST \
         /api/features/toggle, /api/features/set, /api/features/activate_all, \
         /api/features/deactivate_all.",
        bindings.len(),
        sample.join(", ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_names_and_numbers_both_parse() {
        assert_eq!(parse_action("0"), 0);
        assert_eq!(parse_action("5"), 5);
        assert_eq!(parse_action("mrhToggleActivation"), 0);
        assert_eq!(parse_action("mrhSetValue"), 5);
        assert_eq!(parse_action("mrhDecreaseValue"), 7);
        // CT files use the display strings.
        assert_eq!(parse_action("Toggle Activation"), 0);
        assert_eq!(parse_action("Set Value"), 5);
        assert_eq!(parse_action("Increase Value"), 6);
        assert_eq!(parse_action("Deactivate"), 4);
        // Unknown falls back to toggle, matching the default.
        assert_eq!(parse_action("nonsense"), 0);
    }
}
