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

#[test]
fn download_label_pads_short_names_and_keeps_long_ones() {
    assert_eq!(download_label("jq"), format!("jq{}", " ".repeat(22)));
    let long = "a".repeat(30);
    assert_eq!(download_label(&long), long);
}

#[test]
fn templates_put_the_spinner_first_and_colour_only_when_asked() {
    assert_eq!(
        bar_template(false),
        "  {spinner} {msg}  {bytes}/{total_bytes}  [{wide_bar}]  {percent:>3}%  {binary_bytes_per_sec}  {eta}"
    );
    assert!(bar_template(true).contains("{spinner:.cyan} {msg}"));
    assert!(bar_template(true).contains("[{wide_bar:.cyan}]"));
    assert_eq!(
        unsized_template(false),
        "  {spinner} {msg}  {bytes}  {binary_bytes_per_sec}"
    );
    assert!(unsized_template(true).starts_with("  {spinner:.cyan} {msg}"));
}

#[test]
fn download_bar_is_inert_when_not_animated() {
    let bar = DownloadBar {
        bar: None,
        label: String::new(),
        colored: false,
    };
    bar.start(Some(10));
    bar.advance(5);
    bar.finish();
}
