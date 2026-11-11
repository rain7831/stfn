//! GUI/CLI 启动意图解析测试（不启动 GTK）。
use clap::Parser;
use stfn::cli::{Cli, GuiIntent, Mode};

fn mode(args: &[&str]) -> Mode {
    Cli::parse_from(args).mode()
}

#[test]
fn no_arguments_opens_main_window() {
    assert_eq!(
        mode(&["stfn"]),
        Mode::Gui {
            intent: GuiIntent::Open
        }
    );
}

#[test]
fn settings_and_selection_are_gui_intents() {
    assert_eq!(
        mode(&["stfn", "settings"]),
        Mode::Gui {
            intent: GuiIntent::Settings
        }
    );
    assert_eq!(
        mode(&["stfn", "selection"]),
        Mode::Gui {
            intent: GuiIntent::Selection
        }
    );
}

#[test]
fn text_and_flags_stay_in_cli_mode() {
    assert_eq!(mode(&["stfn", "hello"]), Mode::Cli);
    assert_eq!(mode(&["stfn", "--json", "hello"]), Mode::Cli);
    assert_eq!(mode(&["stfn", "-"]), Mode::Cli);
}
