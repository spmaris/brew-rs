use super::*;

#[test]
fn test_formula_default() {
    let formula = Formula::default();
    assert_eq!(formula.name, "");
    assert_eq!(formula.desc, None);
    assert!(!formula.deprecated);
}

#[test]
fn test_cask_default() {
    let cask = Cask::default();
    assert_eq!(cask.token, "");
    assert_eq!(cask.desc, None);
    assert!(!cask.deprecated);
}
