//! Generates `docs/keybindings.md` from the bundled key bindings and the `?`
//! help labels, and fails when the committed file is stale.
//!
//! ```text
//! UPDATE_DOCS=1 cargo test keybindings_reference
//! ```

use std::{fmt::Write as _, path::PathBuf};

use strum::IntoEnumIterator;

use super::render::{help_entries, GO_TO_HELP_ENTRIES, TEXT_INPUT_HELP};
use crate::{command::Command, config::Config, keybindings::KeyBindings, mode::Mode};

/// The screens, in the order the reference lists them.
const SCREENS: [(Mode, &str); 6] = [
    (Mode::Common, "Everywhere"),
    (Mode::KeySpace, "Key list and values"),
    (Mode::Dashboard, "Dashboard"),
    (Mode::PubSub, "Pub/Sub"),
    (Mode::Monitor, "MONITOR"),
    (Mode::Slowlog, "Slowlog"),
];

/// A table cell's text: a literal `|` would end the cell.
fn cell(text: &str) -> String {
    text.replace('|', "\\|")
}

/// `` `j`, `Down` ``.
fn keys_cell(keys: &[String]) -> String {
    keys.iter()
        .map(|key| format!("`{}`", key.replace('|', "\\|")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The commands bound in `mode` with their label: the help's order first,
/// then anything the help doesn't list, by name.
fn rows(keybindings: &KeyBindings, mode: Mode) -> Vec<(Vec<String>, String)> {
    let help: Vec<(Command, &str)> = if mode == Mode::Common {
        Vec::new()
    } else {
        help_entries(mode)
            .iter()
            .chain(GO_TO_HELP_ENTRIES)
            .copied()
            .collect()
    };
    let mut unlisted: Vec<Command> = Command::iter()
        .filter(|command| !help.iter().any(|(listed, _)| listed == command))
        .collect();
    unlisted.sort_by_key(ToString::to_string);

    help.iter()
        .map(|(command, label)| (*command, (*label).to_owned()))
        .chain(
            unlisted
                .into_iter()
                .map(|command| (command, command.to_string())),
        )
        .filter_map(|(command, label)| {
            let keys = keybindings.get_config_for_command(mode, command);
            (!keys.is_empty()).then_some((keys, label))
        })
        .collect()
}

/// The whole reference.
fn render(keybindings: &KeyBindings) -> String {
    let mut out = String::from(
        "# Keybindings\n\n\
         <!-- Generated from .config/config.json5. Do not edit: run\n\
         `UPDATE_DOCS=1 cargo test keybindings_reference`. -->\n\n\
         The default keys. Rebind any of them in your config file; the `?` help and\n\
         the hint bar always show the keys in force. `g o` is a chord: `g`, then `o`.\n",
    );
    for (mode, title) in SCREENS {
        let _ = write!(out, "\n## {title}\n\n| Keys | Action |\n| --- | --- |\n");
        for (keys, label) in rows(keybindings, mode) {
            let _ = writeln!(out, "| {} | {} |", keys_cell(&keys), cell(&label));
        }
    }
    out.push_str(
        "\n## Text inputs\n\nFixed, not rebindable.\n\n| Keys | Action |\n| --- | --- |\n",
    );
    for (label, keys) in TEXT_INPUT_HELP {
        let _ = writeln!(out, "| `{}` | {} |", cell(keys), cell(label));
    }
    out
}

fn bundled() -> KeyBindings {
    let config: Config = json5::from_str(include_str!("../../.config/config.json5"))
        .expect("the bundled config parses");
    config.keybindings
}

#[test]
fn keybindings_reference() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/keybindings.md");
    let actual = render(&bundled());
    if std::env::var_os("UPDATE_DOCS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == actual,
        "docs/keybindings.md is stale. Run: UPDATE_DOCS=1 cargo test keybindings_reference"
    );
}

#[test]
fn every_default_binding_is_in_the_reference() {
    let keybindings = bundled();
    for (mode, _) in SCREENS {
        let listed: usize = rows(&keybindings, mode)
            .iter()
            .map(|(keys, _)| keys.len())
            .sum();
        let bound = keybindings
            .0
            .get(&mode)
            .map_or(0, std::collections::HashMap::len);
        assert_eq!(listed, bound, "{mode:?}");
    }
}

#[test]
fn a_changed_binding_changes_the_reference() {
    let mut keybindings = bundled();
    let before = render(&keybindings);
    let keyspace = keybindings.0.get_mut(&Mode::KeySpace).unwrap();
    keyspace.retain(|_, command| *command != Command::ScrollDown);
    keyspace.insert(
        crate::keybindings::parse_key_sequence("<x>").unwrap(),
        Command::ScrollDown,
    );

    let after = render(&keybindings);

    assert_ne!(before, after);
    assert!(after.contains("| `x` | Scroll down |"), "{after}");
}

#[test]
fn a_pipe_key_cannot_break_a_table() {
    assert_eq!(keys_cell(&["|".to_owned()]), "`\\|`");
    assert_eq!(cell("a | b"), "a \\| b");
}
