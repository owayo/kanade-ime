use kanade_ipc::{MOD_ALT, MOD_CONTROL, MOD_META, MOD_SHIFT, frontend::function_key};
use std::{collections::BTreeMap, path::Path};

#[test]
fn function_shortcuts_match_the_pinned_keymap() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../third_party/mozc/src/data/keymap/ms-ime.tsv");
    let table = std::fs::read_to_string(path).unwrap();
    let bindings: BTreeMap<_, _> = table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut columns = line.split('\t');
            Some(((columns.next()?, columns.next()?), columns.next()?))
        })
        .collect();
    for number in 6..=10 {
        let normalized = function_key(number, 0).unwrap();
        let function = format!("F{number}");
        let control = format!("Ctrl {}", normalized.text);
        for state in ["Composition", "Conversion"] {
            let command = bindings.get(&(state, function.as_str()));
            assert!(command.is_some());
            assert_eq!(command, bindings.get(&(state, control.as_str())));
        }
        for state in ["Precomposition", "Suggestion", "Prediction"] {
            assert!(!bindings.contains_key(&(state, function.as_str())));
            assert!(!bindings.contains_key(&(state, control.as_str())));
        }
        for prefix in ["Shift", "Ctrl", "Alt", "Meta"] {
            let modified = format!("{prefix} {function}");
            assert!(!bindings.keys().any(|(_, key)| *key == modified));
        }
        for modifier in [MOD_SHIFT, MOD_CONTROL, MOD_ALT, MOD_META] {
            assert!(function_key(number, modifier).is_none());
        }
    }
}
