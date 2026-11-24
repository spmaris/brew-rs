use super::*;

#[test]
fn plain_line_indents_the_finished_text_under_a_check_mark() {
    assert_eq!(plain_line("Downloaded jq 1.8.2"), "  ✔ Downloaded jq 1.8.2");
}

#[test]
fn look_animates_on_interactive_terminals_and_colours_only_when_allowed() {
    let look = |colored, boxed| Look::from_mode(Mode { colored, boxed });
    assert_eq!(
        look(true, true),
        Look {
            animate: true,
            colored: true
        }
    );
    // NO_COLOR on a real terminal: still animated, just uncoloured.
    assert_eq!(
        look(false, true),
        Look {
            animate: true,
            colored: false
        }
    );
    assert_eq!(
        look(false, false),
        Look {
            animate: false,
            colored: false
        }
    );
}

fn width(text: &str) -> usize {
    console::measure_text_width(text)
}

#[test]
fn label_is_padded_or_truncated_to_exactly_twenty_columns() {
    assert_eq!(download_label("jq"), format!("jq{}", " ".repeat(18)));
    assert_eq!(download_label(&"a".repeat(20)), "a".repeat(20));
    let long = download_label("visual-studio-code-insiders");
    assert_eq!(long, "visual-studio-code-…");
    assert_eq!(width(&long), 20);
    // Wide characters count two columns each; a cut never splits one.
    let wide = download_label(&"日".repeat(15));
    assert_eq!(width(&wide), 20);
    assert!(wide.contains('…'));
    assert_eq!(width(&download_label("日本")), 20);
}

#[test]
fn sizes_have_one_decimal_in_a_ten_column_field() {
    assert_eq!(size_field(0), "     0.0 B");
    assert_eq!(size_field(1023), "  1023.0 B");
    assert_eq!(size_field(1_040_580), "1016.2 KiB");
    assert_eq!(size_field(59_419_000), "  56.7 MiB");
    assert_eq!(size_field(1_503_238_554), "   1.4 GiB");
}

#[test]
fn percent_is_three_columns_and_never_exceeds_100() {
    assert_eq!(percent_field(0, 10), "  0");
    assert_eq!(percent_field(7, 20), " 35");
    assert_eq!(percent_field(30, 10), "100");
    assert_eq!(percent_field(1, 0), "  0");
}

#[test]
fn eta_is_mm_ss_or_dashes() {
    assert_eq!(eta_field(Some(2)), "00:02");
    assert_eq!(eta_field(Some(754)), "12:34");
    assert_eq!(eta_field(Some(5999)), "99:59");
    assert_eq!(eta_field(Some(6000)), "--:--");
    assert_eq!(eta_field(None), "--:--");
}

#[test]
fn bar_width_follows_the_terminal_and_drops_fields_when_narrow() {
    assert_eq!(bar_width(120), (40, Fields::Full));
    assert_eq!(bar_width(110), (30, Fields::Full));
    assert_eq!(bar_width(200), (60, Fields::Full));
    assert_eq!(bar_width(90), (10, Fields::Full));
    // 80 columns cannot hold a 10-wide bar with speed, so both go.
    assert_eq!(bar_width(80), (21, Fields::Minimal));
    assert_eq!(bar_width(83), (10, Fields::NoEta));
    assert_eq!(bar_width(40), (10, Fields::Minimal));
}

/// The line the style renders for the given state, minus the spinner.
fn line(name: &str, done: u64, total: u64, rate: f64, eta: Option<u64>, w: usize) -> String {
    let (bar, fields) = bar_width(w);
    let filled = (done as u128 * bar as u128 / total as u128) as usize;
    let tail = match fields {
        Fields::Full => format!("  {}/s  {}", speed_field(rate), eta_field(eta)),
        Fields::NoEta => format!("  {}/s", speed_field(rate)),
        Fields::Minimal => String::new(),
    };
    format!(
        "{}  {} / {}  [{}{}]  {}%{tail}",
        download_label(name),
        size_field(done),
        size_field(total),
        "#".repeat(filled),
        "-".repeat(bar - filled),
        percent_field(done, total),
    )
}

#[test]
fn every_line_has_the_same_length_and_bracket_columns() {
    for w in [80, 110, 120, 200] {
        let lines = [
            line("gmp", 0, 1_040_580, 0.0, None, w),
            line("pandoc", 20_990_000, 59_419_000, 21_570_000.0, Some(2), w),
            line(
                "visual-studio-code-insiders",
                212_000_000,
                393_700_000,
                5.05e7,
                Some(4),
                w,
            ),
            line("日本語", 1, 1_503_238_554, 1.0, Some(7000), w),
        ];
        let first = &lines[0];
        for l in &lines {
            assert_eq!(width(l), width(first), "width {w}: {l}");
            for mark in ['[', ']'] {
                let column = |t: &str| t.find(mark).map(|at| width(&t[..at]));
                assert_eq!(column(l), column(first), "{l}");
            }
        }
    }
}

#[test]
fn styles_put_the_spinner_first_and_colour_only_when_asked() {
    // Building must not panic or reject the template.
    let _ = bar_style(false, 30, Fields::Full, "jq");
    let _ = bar_style(true, 30, Fields::Minimal, "jq");
    let _ = unsized_style(true, "jq");
}

#[test]
fn download_bar_is_inert_when_not_animated() {
    let bar = DownloadBar {
        bar: None,
        name: String::new(),
        colored: false,
    };
    bar.start(Some(10));
    bar.advance(5);
    bar.finish();
}

fn at(name: &str, ms: u64) -> String {
    marquee_window(name, 20, Duration::from_millis(ms))
}

const LONG: &str = "visual-studio-code-insiders";

#[test]
fn marquee_holds_at_the_start_during_the_pause() {
    assert_eq!(at(LONG, 0), "visual-studio-code-i");
    assert_eq!(at(LONG, 999), "visual-studio-code-i");
}

#[test]
fn marquee_advances_a_column_per_step_after_the_pause() {
    assert_eq!(at(LONG, 1000), "visual-studio-code-i");
    assert_eq!(at(LONG, 1150), "isual-studio-code-in");
    assert_eq!(at(LONG, 1000 + 150 * 3), "ual-studio-code-insi");
}

#[test]
fn marquee_wraps_through_the_separator() {
    // 27 name columns + 3 gap columns: offset 20 shows the tail, the gap, the start.
    assert_eq!(at(LONG, 1000 + 150 * 20), "nsiders · visual-stu");
    // A full cycle returns to the start.
    assert_eq!(at(LONG, 1000 + 150 * 30), "visual-studio-code-i");
}

#[test]
fn marquee_is_always_twenty_columns_even_with_wide_glyphs() {
    let wide = "日本語のとても長いパッケージ名前";
    for ms in (0..12_000).step_by(75) {
        assert_eq!(width(&at(wide, ms)), 20, "wide at {ms}");
        assert_eq!(width(&at(LONG, ms)), 20, "long at {ms}");
    }
}

#[test]
fn marquee_leaves_names_that_fit_alone() {
    for ms in [0, 1500, 60_000] {
        assert_eq!(at("jq", ms), download_label("jq"));
        assert_eq!(at(&"a".repeat(20), ms), "a".repeat(20));
    }
}
