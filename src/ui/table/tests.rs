use console::measure_text_width as cells;
use ratatui::style::{Color, Modifier};
use ratatui::{Terminal, backend::TestBackend};

use super::format::summary_block;
use super::mode::ModeInputs;
use super::*;

const BOXED: Mode = Mode {
    colored: false,
    boxed: true,
};
const STYLED: Mode = Mode {
    colored: true,
    boxed: true,
};

fn strip_ansi(text: &str) -> String {
    console::strip_ansi_codes(text).into_owned()
}

fn row(cells: &[&str]) -> Vec<String> {
    cells.iter().map(ToString::to_string).collect()
}

fn plan_row(action: &'static str, note: &str, cells: &[&str]) -> Row {
    Row::plan(action, note.to_string(), row(cells))
}

fn plan_panel() -> Panel<'static> {
    Panel {
        title: "Install plan",
        headers: &["Package", "Version", "Source"],
        sections: vec![
            Section::new(
                "Installing",
                vec![plan_row("install", "", &["ripgrep", "14.1.1", "homebrew"])],
            ),
            Section::new("Promoting", Vec::new()),
            Section::new(
                "Installing dependencies",
                vec![plan_row("install", "", &["pcre2", "10.44", "homebrew"]).requested(false)],
            ),
        ],
        summary: Vec::new(),
    }
}

fn summary_rows() -> [(&'static str, String); 3] {
    [
        ("Install", "2 packages (1 requested, 1 dependency)".into()),
        ("Target", "arm64_sequoia".into()),
        ("Prefix", "~/p".into()),
    ]
}

#[test]
fn mode_decision_covers_every_input() {
    let base = ModeInputs {
        is_tty: true,
        term_dumb: false,
        no_color: false,
        config_colored: true,
    };
    let decide = |change: fn(&mut ModeInputs)| {
        let mut inputs = base;
        change(&mut inputs);
        Mode::decide(inputs)
    };
    assert_eq!(Mode::decide(base), STYLED);
    assert_eq!(decide(|i| i.is_tty = false), Mode::PLAIN);
    assert_eq!(decide(|i| i.term_dumb = true), Mode::PLAIN);
    assert_eq!(decide(|i| i.no_color = true), BOXED);
    assert_eq!(decide(|i| i.config_colored = false), BOXED);
    // Redirected output is plain even when colour is requested.
    assert_eq!(
        decide(|i| {
            i.is_tty = false;
            i.config_colored = true;
        }),
        Mode::PLAIN
    );
}

#[test]
fn plain_flat_table_is_space_aligned_with_header() {
    let panel = Panel::flat(
        "Installed (2)",
        &["Package", "Version", "Source", "Reason"],
        &[
            row(&["猫", "1\n2", "homebrew", "requested"]),
            row(&["ripgrep", "14.1.1", "upstream", "dependency"]),
        ],
    );
    assert_eq!(
        panel.render(Mode::PLAIN, Some(10)),
        "Package  Version  Source    Reason\n\
             猫       1 2      homebrew  requested\n\
             ripgrep  14.1.1   upstream  dependency"
    );
}

#[test]
fn plain_plan_table_leads_with_action_and_ends_with_reason_and_note() {
    assert_eq!(
        plan_panel().render(Mode::PLAIN, None),
        "Action   Package  Version  Source    Reason      Note\n\
             install  ripgrep  14.1.1   homebrew  requested   -\n\
             install  pcre2    10.44    homebrew  dependency  -"
    );
}

#[test]
fn plain_plan_replaces_spaces_in_a_note_with_dashes() {
    let panel = Panel {
        title: "Install plan",
        headers: &["Package", "Version", "Source"],
        sections: vec![Section::new(
            "Installing",
            vec![plan_row(
                "install",
                "needs a long note",
                &["hello", "1.2", "homebrew"],
            )],
        )],
        summary: Vec::new(),
    };
    let rendered = panel.render(Mode::PLAIN, None);
    assert!(rendered.contains("needs-a-long-note"), "{rendered}");
    assert!(!rendered.contains("needs a long note"), "{rendered}");
}

#[test]
fn plain_plan_uses_single_action_tokens_and_prints_dash_for_an_empty_note() {
    let panel = Panel {
        title: "Install plan",
        headers: &["Package", "Version", "Source"],
        sections: vec![Section::new(
            "Already installed",
            vec![plan_row(
                "already-installed",
                "",
                &["hello", "1.2", "homebrew"],
            )],
        )],
        summary: Vec::new(),
    };
    let rendered = panel.render(Mode::PLAIN, None);
    assert_eq!(
        rendered,
        "Action             Package  Version  Source    Reason     Note\n\
             already-installed  hello    1.2      homebrew  requested  -"
    );
}

#[test]
fn boxed_plan_without_exceptions_has_neither_action_nor_note_column() {
    let text = plan_panel().render(BOXED, None);
    assert!(!text.contains("Action"), "{text}");
    assert!(!text.contains("Note"), "{text}");
    assert!(text.contains("Package") && text.contains("Version") && text.contains("Source"));
}

#[test]
fn boxed_plan_shows_a_note_column_for_a_keg_only_row() {
    let panel = Panel {
        title: "Install plan",
        headers: &["Package", "Version", "Source"],
        sections: vec![Section::new(
            "Installing",
            vec![
                plan_row("install", "", &["ripgrep", "14.1.1", "homebrew"]),
                plan_row("install", "keg-only", &["openssl@3", "3.0", "homebrew"]),
            ],
        )],
        summary: Vec::new(),
    };
    let text = panel.render(BOXED, None);
    assert!(text.contains("Note"), "{text}");
    assert!(text.contains("keg-only"), "{text}");
    let openssl = text
        .lines()
        .find(|line| line.contains("openssl@3"))
        .expect("openssl row");
    assert!(openssl.contains("keg-only"), "{openssl:?}");
}

#[test]
fn boxed_panel_has_rounded_corners_title_header_and_sections() {
    let text = plan_panel().render(BOXED, None);
    assert!(!text.contains('\x1b'));
    assert_eq!(
        text,
        "╭─ Install plan ───────────────╮\n\
             │ Package    Version  Source   │\n\
             │ ──────────────────────────── │\n\
             │ Installing                   │\n\
             │   ripgrep  14.1.1   homebrew │\n\
             │ Installing dependencies      │\n\
             │   pcre2    10.44    homebrew │\n\
             ╰──────────────────────────────╯"
    );
}

fn assert_full_width(text: &str, width: usize) {
    assert!(
        text.lines().all(|line| cells(line) == width),
        "not {width} wide:\n{text}"
    );
}

#[test]
fn panels_fill_the_width_and_keep_columns_compact() {
    let narrow = plan_panel().render(BOXED, Some(80));
    assert_full_width(&narrow, 80);
    let wide = plan_panel().render(BOXED, Some(120));
    assert_full_width(&wide, 120);
    // Spare width goes to the last column, so every earlier column keeps
    // its natural width and starts at the same place; Source, being last,
    // only grows to the right.
    let header = |text: &str| text.lines().nth(1).unwrap().to_string();
    let start = |line: &str, word: &str| cells(&line[..line.find(word).unwrap()]);
    let (natural, narrow_header, wide_header) = (
        header(&plan_panel().render(BOXED, None)),
        header(&narrow),
        header(&wide),
    );
    for word in ["Version", "Source"] {
        assert_eq!(start(&narrow_header, word), start(&natural, word), "{word}");
        assert_eq!(start(&wide_header, word), start(&natural, word), "{word}");
    }
}

#[test]
fn panel_width_fills_the_terminal_and_defaults_to_100() {
    assert_eq!(panel_width(Some(80)), 80);
    assert_eq!(panel_width(Some(120)), 120);
    assert_eq!(panel_width(Some(200)), 200);
    assert_eq!(panel_width(None), 100);
    let full = plan_panel().render(BOXED, Some(panel_width(Some(200))));
    assert_full_width(&full, 200);
}

#[test]
fn summary_rows_sit_inside_the_borders() {
    let mut panel = plan_panel();
    panel.summary = summary_rows()
        .iter()
        .map(|(label, value)| (label.to_string(), value.clone()))
        .collect();
    let text = panel.render(BOXED, Some(60));
    assert_eq!(
        text,
        "╭─ Install plan ───────────────────────────────────────────╮\n\
             │ Package    Version  Source                               │\n\
             │ ──────────────────────────────────────────────────────── │\n\
             │ Installing                                               │\n\
             │   ripgrep  14.1.1   homebrew                             │\n\
             │ Installing dependencies                                  │\n\
             │   pcre2    10.44    homebrew                             │\n\
             │ ──────────────────────────────────────────────────────── │\n\
             │ Summary                                                  │\n\
             │   Install     2 packages (1 requested, 1 dependency)     │\n\
             │   Target      arm64_sequoia                              │\n\
             │   Prefix      ~/p                                        │\n\
             ╰──────────────────────────────────────────────────────────╯"
    );
    let styled = panel.render(STYLED, Some(60));
    assert_eq!(strip_ansi(&styled), text);
    assert!(!styled.contains("\x1b[2mPrefix"), "{styled:?}");
    assert!(styled.contains("\x1b[1;36mSummary\x1b[0m"), "{styled:?}");
}

#[test]
fn long_summary_values_wrap_inside_the_panel() {
    let mut panel = plan_panel();
    panel.summary = vec![(
        "Prefix".into(),
        "~/Library/Application Support/brew-rs/native/with/a/very/long/path".into(),
    )];
    let text = panel.render(BOXED, Some(50));
    assert_full_width(&text, 50);
    assert!(text.contains("Prefix"), "{text}");
}

#[test]
fn plain_render_excludes_the_summary_which_is_rendered_separately() {
    let summary = summary_rows();
    let panel =
        Panel::flat("T", &["Package", "Version"], &[row(&["a", "1"])]).with_summary(&summary);
    let plain = panel.render(Mode::PLAIN, None);
    assert_eq!(plain, "Package  Version\na        1");
    assert!(!plain.contains("Summary"));
    assert!(!plain.contains('│'));
    assert_eq!(
        panel.plain_summary(Mode::PLAIN),
        "Summary\n\
             \x20 Install     2 packages (1 requested, 1 dependency)\n\
             \x20 Target      arm64_sequoia\n\
             \x20 Prefix      ~/p"
    );
    let stacked = Panel::flat(
        "T",
        &["Package", "Available"],
        &[row(&["a-very-long-name", "2"])],
    )
    .with_summary(&summary)
    .render(BOXED, Some(12));
    assert!(stacked.ends_with("  Prefix      ~/p"), "{stacked}");
}

#[test]
fn plain_upgrade_table_splits_versions_and_adds_reason_and_note() {
    let panel = Panel {
        title: "Upgrade plan",
        headers: &["Package", "Version", "Source"],
        sections: vec![Section::new(
            "Upgrading",
            vec![plan_row("upgrade", "", &["hello", "1.2 → 1.3", "homebrew"])],
        )],
        summary: Vec::new(),
    };
    assert_eq!(
        panel.render(Mode::PLAIN, None),
        "Action   Package  From  To   Source    Reason     Note\n\
             upgrade  hello    1.2   1.3  homebrew  requested  -"
    );
}

#[test]
fn styled_panel_colours_semantically_and_strips_to_the_boxed_text() {
    let styled = plan_panel().render(STYLED, Some(80));
    assert_eq!(strip_ansi(&styled), plan_panel().render(BOXED, Some(80)));
    assert_eq!(strip_ansi(&styled).lines().next().map(cells), Some(80));
    assert!(styled.contains("\x1b[1;36mInstalling\x1b[0m"));
    assert!(styled.contains("\x1b[1mPackage\x1b[0m"));
    assert!(!styled.contains("\x1b[2mpcre2"), "{styled:?}");
}

#[test]
fn version_change_dims_old_and_arrow_and_bolds_new() {
    let panel = Panel::flat(
        "Upgrade plan",
        &["Package", "Version", "Source"],
        &[row(&["hello", "1.2 → 1.3", "homebrew"])],
    );
    let styled = panel.render(STYLED, Some(80));
    assert!(
        styled.contains("\x1b[2m1.2 → \x1b[0m\x1b[1;32m1.3\x1b[0m"),
        "{styled:?}"
    );
}

#[test]
fn dependency_rows_are_not_dimmed() {
    let styled = plan_panel().render(STYLED, Some(80));
    let dependency = styled
        .lines()
        .find(|line| line.contains("pcre2"))
        .expect("dependency row");
    assert!(!dependency.contains("\x1b[2m"), "{dependency:?}");
}

#[test]
fn divider_rules_stay_dim() {
    let styled = plan_panel().render(STYLED, Some(80));
    assert!(styled.contains("\x1b[2m─"), "{styled:?}");
}

#[test]
fn long_cells_wrap_to_the_terminal_width() {
    let path = "/Users/example/Library/Application Support/brew-rs/native/Cellar/pkg/1.0";
    let panel = Panel::flat(
        "Paths",
        &["Package", "Path"],
        &[row(&["pkg", path]), row(&["other", "short"])],
    );
    let text = panel.render(BOXED, Some(40));
    assert!(text.lines().all(|line| cells(line) <= 40), "{text}");
    assert!(text.lines().count() > 6, "{text}");
    assert!(text.contains("other"));
    let flattened: String = text
        .lines()
        .filter_map(|line| line.strip_prefix("│ pkg "))
        .chain(text.lines().filter(|line| line.starts_with("│       ")))
        .map(|line| line.trim_matches(|c| c == '│' || c == ' '))
        .collect();
    assert!(flattened.contains("/Users/example/Library"), "{text}");
}

#[test]
fn too_narrow_for_a_panel_stacks_fields() {
    let panel = Panel::flat(
        "Packages",
        &["Package", "Available"],
        &[row(&["example/tap/very-long-package-name", "2.0"])],
    );
    assert_eq!(
        panel.render(BOXED, Some(24)),
        "example/tap/very-long-package-name\n  Available       2.0"
    );
}

#[test]
fn panel_draws_into_a_ratatui_test_backend() {
    let layout = plan_panel().layout(Some(80)).unwrap();
    let mut terminal = Terminal::new(TestBackend::new(layout.width, layout.height)).unwrap();
    terminal
        .draw(|frame| layout.paint(frame.buffer_mut()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let (right, bottom) = (layout.width - 1, layout.height - 1);
    assert_eq!(buffer[(0, 0)].symbol(), "╭");
    assert_eq!(buffer[(right, 0)].symbol(), "╮");
    assert_eq!(buffer[(0, bottom)].symbol(), "╰");
    assert_eq!(buffer[(right, bottom)].symbol(), "╯");
    let header = buffer[(2, 1)].clone();
    assert_eq!(header.symbol(), "P");
    assert!(header.modifier.contains(Modifier::BOLD));
    let first = buffer[(4, 4)].clone();
    assert_eq!(first.symbol(), "r");
    assert_eq!(first.fg, Color::Reset);
}

#[test]
fn errors_are_red_bold_with_indented_hints() {
    let message = "No Homebrew formula named 'x'.\n  Did you mean: y?\nthird\n    brewrs install z";
    assert_eq!(
        format_error(Mode::PLAIN, message),
        "Error: No Homebrew formula named 'x'.\n  Did you mean: y?\n  third\n    brewrs install z"
    );
    assert_eq!(
        format_error(STYLED, message),
        "\x1b[1;31mError:\x1b[0m No Homebrew formula named 'x'.\n  Did you mean: y?\n  third\n    \x1b[1mbrewrs install z\x1b[0m"
    );
}

#[test]
fn prose_mentioning_brewrs_is_not_bold() {
    let message = "Blocked.\n  Homebrew built it for /opt/homebrew only, and brewrs installs into its own prefix.\n  brewrs can't relocate it.";
    let styled = format_error(STYLED, message);
    assert!(!styled.contains("\x1b[1mbrewrs"), "{styled:?}");
    assert!(styled.contains("and brewrs installs into"), "{styled:?}");
}

#[test]
fn command_after_prose_is_bold_to_end_of_line() {
    let message = "Blocked.\n  brewrs installs here; run brewrs install --source upstream jq now";
    assert_eq!(
        format_error(STYLED, message),
        "\x1b[1;31mError:\x1b[0m Blocked.\n  brewrs installs here; run \x1b[1mbrewrs install --source upstream jq now\x1b[0m"
    );
}

#[test]
fn runnable_hints_are_bold_from_the_command_onwards() {
    let message = "Something failed.\n  Try: brewrs install neovim --verbose";
    let styled = format_error(STYLED, message);
    assert!(
        styled.contains("\n  Try: \x1b[1mbrewrs install neovim --verbose\x1b[0m"),
        "{styled:?}"
    );
    assert_eq!(
        format_error(BOXED, message),
        "Error: Something failed.\n  Try: brewrs install neovim --verbose"
    );
}

#[test]
fn summary_block_aligns_plain_labels_under_a_bold_cyan_heading() {
    let rows = [
        ("Install", "2 packages".to_string()),
        ("Prefix", "~/p".into()),
    ];
    assert_eq!(
        summary_block(Mode::PLAIN, &rows),
        "Summary\n  Install     2 packages\n  Prefix      ~/p"
    );
    assert_eq!(
        summary_block(STYLED, &rows),
        "\x1b[1;36mSummary\x1b[0m\n  Install     2 packages\n  Prefix      ~/p"
    );
}

#[test]
fn fields_are_indented_with_plain_labels_and_no_colon() {
    let rows = [("Bin", "~/bin".to_string()), ("Opt", "~/opt".into())];
    let expected = "  Bin         ~/bin\n  Opt         ~/opt";
    assert_eq!(fields(Mode::PLAIN, &rows), expected);
    assert_eq!(fields(STYLED, &rows), expected);
    assert_eq!(
        field(Mode::PLAIN, "Scope", "runtime\nsecond"),
        "  Scope           runtime\n                  second"
    );
}

#[test]
fn field_labels_share_one_width_and_longer_labels_push_their_value() {
    let short = fields(Mode::PLAIN, &[("Bin", "a".to_string())]);
    let other = fields(Mode::PLAIN, &[("Service", "a".to_string())]);
    assert_eq!(short.find('a'), Some(14));
    assert_eq!(other.find('a'), Some(14));
    assert_eq!(
        fields(Mode::PLAIN, &[("Destination", "a".to_string())]),
        "  Destination    a"
    );
}

#[test]
fn summary_labels_share_one_width_across_panels() {
    let value_column = |label: &str| {
        let panel =
            Panel::flat("T", &["A"], &[row(&["a"])]).with_summary(&[(label, "value".to_string())]);
        let text = panel.render(BOXED, Some(40));
        let line = text.lines().find(|line| line.contains("value")).unwrap();
        line.chars().take_while(|ch| *ch != 'v').count()
    };
    assert_eq!(value_column("Prefix"), value_column("Install"));
    assert_eq!(value_column("Appdir"), value_column("Mark"));
}

#[test]
fn result_lines_always_carry_the_check_and_colour_it_only_with_colour() {
    assert_eq!(
        result(STYLED, "Installed 1 formula"),
        "\x1b[1;32m✔ \x1b[0mInstalled 1 formula"
    );
    assert_eq!(
        result(BOXED, "Installed 2 formulae"),
        "✔ Installed 2 formulae"
    );
    assert_eq!(
        result(Mode::PLAIN, "Removed 1 formula"),
        "✔ Removed 1 formula"
    );
}

#[test]
fn display_path_abbreviates_the_home_directory() {
    let home = dirs::home_dir().unwrap();
    assert_eq!(display_path(&home.join("x/y")), "~/x/y");
    assert_eq!(display_path(&home), "~");
    assert_eq!(display_path(std::path::Path::new("/opt/x")), "/opt/x");
}
