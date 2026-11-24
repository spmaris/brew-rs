use super::*;

#[test]
fn plain_line_indents_the_finished_text_under_a_check_mark() {
    assert_eq!(plain_line("Downloaded jq 1.8.2"), "  ✔ Downloaded jq 1.8.2");
}
