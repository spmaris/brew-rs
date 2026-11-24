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
