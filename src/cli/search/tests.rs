use super::*;

fn formula(name: &str, desc: Option<&str>) -> Formula {
    Formula {
        name: name.to_string(),
        desc: desc.map(str::to_string),
        ..Default::default()
    }
}

fn cask(token: &str, names: &[&str], desc: Option<&str>) -> Cask {
    Cask {
        token: token.to_string(),
        name: names.iter().map(|name| name.to_string()).collect(),
        desc: desc.map(str::to_string),
        ..Default::default()
    }
}

fn package_kind(formula_flag: bool, cask_flag: bool) -> crate::cli::PackageKind {
    crate::cli::PackageKind {
        formula: formula_flag,
        cask: cask_flag,
    }
}

fn display_names(hits: &[SearchHit]) -> Vec<&str> {
    hits.iter().map(|hit| hit.display_name.as_str()).collect()
}

#[test]
fn exact_formula_ranks_before_fuzzy_formula() {
    let formulae = vec![
        formula("ripgrep-all", Some("search tool")),
        formula("ripgrep", Some("line-oriented search tool")),
    ];

    let hits = collect_search_hits(formulae, Vec::new(), "ripgrep", 10, SearchScope::All);

    assert_eq!(display_names(&hits)[0], "ripgrep");
    assert!(hits[0].exact);
    assert!(!hits[1].exact);
}

#[test]
fn exact_cask_ranks_before_fuzzy_formula_across_namespaces() {
    let formulae = vec![formula("docker-completion", Some("completion for docker"))];
    let casks = vec![cask("docker", &["Docker"], Some("Container runtime"))];

    let hits = collect_search_hits(formulae, casks, "docker", 10, SearchScope::All);

    assert_eq!(hits[0].display_name, "docker");
    assert_eq!(hits[0].human_name.as_deref(), Some("Docker"));
    assert_eq!(hits[0].kind, PackageType::Cask);
    assert!(hits[0].exact);
}

#[test]
fn limit_is_shared_across_formulae_and_casks() {
    let formulae = vec![
        formula("alpha-tool", Some("alpha utility")),
        formula("beta-tool", Some("beta utility")),
    ];
    let casks = vec![
        cask("alpha-app", &["Alpha App"], Some("alpha application")),
        cask("beta-app", &["Beta App"], Some("beta application")),
    ];

    let hits = collect_search_hits(formulae, casks, "alpha", 1, SearchScope::All);

    // Both namespaces contain matches; a per-namespace limit would yield
    // two or more results instead of one shared result.
    assert_eq!(hits.len(), 1);
    assert!(matches!(
        hits[0].display_name.as_str(),
        "alpha-tool" | "alpha-app"
    ));
}

#[test]
fn formula_scope_skips_casks() {
    let hits = collect_search_hits(
        vec![formula("docker", None)],
        vec![cask("docker", &["Docker"], None)],
        "docker",
        10,
        SearchScope::Formulae,
    );

    assert!(!hits.is_empty());
    assert!(hits.iter().all(|hit| hit.kind == PackageType::Formula));
}

#[test]
fn cask_scope_skips_formulae() {
    let hits = collect_search_hits(
        vec![formula("docker", None)],
        vec![cask("docker", &["Docker"], None)],
        "docker",
        10,
        SearchScope::Casks,
    );

    assert!(!hits.is_empty());
    assert!(hits.iter().all(|hit| hit.kind == PackageType::Cask));
}

#[test]
fn package_kind_flags_select_scope() {
    assert_eq!(
        SearchScope::from_kind(&package_kind(false, false)),
        SearchScope::All
    );
    assert_eq!(
        SearchScope::from_kind(&package_kind(true, false)),
        SearchScope::Formulae
    );
    assert_eq!(
        SearchScope::from_kind(&package_kind(false, true)),
        SearchScope::Casks
    );
}

#[test]
fn formula_description_matches_are_included() {
    let formulae = vec![formula("rg", Some("recursively search directories"))];

    let hits = collect_search_hits(formulae, Vec::new(), "recursively", 10, SearchScope::All);

    assert_eq!(hits.len(), 1);
    assert!(!hits[0].exact);
}

#[test]
fn cask_human_names_remain_searchable() {
    let casks = vec![cask(
        "visual-studio-code",
        &["Visual Studio Code"],
        Some("Editor"),
    )];

    let hits = collect_search_hits(Vec::new(), casks, "studio", 10, SearchScope::All);

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].display_name, "visual-studio-code");
    assert_eq!(hits[0].human_name.as_deref(), Some("Visual Studio Code"));
}

#[test]
fn cask_rows_show_the_installable_token_with_the_human_name() {
    let casks = vec![cask(
        "visual-studio-code",
        &["Visual Studio Code"],
        Some("Editor"),
    )];

    let hits = collect_search_hits(
        Vec::new(),
        casks,
        "visual-studio-code",
        10,
        SearchScope::All,
    );
    let rows = table_rows(&hits, false);

    // The Package column must be the token accepted by
    // `brew install --cask`; the human name stays visible in the summary.
    assert_eq!(rows[0][0], "visual-studio-code");
    assert_eq!(rows[0][3], "Visual Studio Code — Editor");
    assert!(hits[0].exact);
}

#[test]
fn cask_rows_without_a_description_still_show_the_human_name() {
    let casks = vec![cask("docker", &["Docker"], None)];

    let hits = collect_search_hits(Vec::new(), casks, "docker", 10, SearchScope::All);
    let rows = table_rows(&hits, false);

    assert_eq!(rows[0][0], "docker");
    assert_eq!(rows[0][3], "Docker");
}

#[test]
fn cask_without_display_name_uses_token() {
    let casks = vec![cask("firefox", &[], None)];

    let hits = collect_search_hits(Vec::new(), casks, "firefox", 10, SearchScope::All);

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].display_name, "firefox");
    assert_eq!(hits[0].human_name, None);
    assert!(hits[0].exact);
}

#[test]
fn empty_query_produces_no_hits() {
    let formulae = vec![formula("ripgrep", None)];
    let casks = vec![cask("firefox", &["Firefox"], None)];

    let hits = collect_search_hits(formulae, casks, "", 10, SearchScope::All);

    assert!(hits.is_empty());
}

#[test]
fn limit_caps_the_merged_result_set() {
    let formulae = vec![formula("alpha", None), formula("beta", None)];
    let casks = vec![cask("gamma", &[], None), cask("delta", &[], None)];

    let hits = collect_search_hits(formulae, casks, "a", 3, SearchScope::All);

    assert_eq!(hits.len(), 3);
}

#[test]
fn verbose_rows_include_homepage() {
    let mut ripgrep = formula("ripgrep", Some("search tool"));
    ripgrep.homepage = Some("https://github.com/BurntSushi/ripgrep".to_string());

    let hits = collect_search_hits(vec![ripgrep], Vec::new(), "ripgrep", 10, SearchScope::All);

    let compact = table_rows(&hits, false);
    assert_eq!(compact[0].len(), 4);

    let verbose = table_rows(&hits, true);
    assert_eq!(verbose[0].len(), 5);
    assert_eq!(verbose[0][0], "ripgrep");
    assert_eq!(verbose[0][2], "formula");
    assert_eq!(verbose[0][4], "https://github.com/BurntSushi/ripgrep");
}

#[test]
fn table_headers_match_row_width() {
    assert_eq!(
        table_headers(false),
        vec!["Package", "Version", "Type", "Description"]
    );
    assert_eq!(table_headers(true).len(), 5);
    assert_eq!(table_headers(true)[4], "Homepage");
}

#[test]
fn missing_version_and_description_have_fallbacks() {
    let hits = collect_search_hits(
        vec![formula("bare", None)],
        Vec::new(),
        "bare",
        10,
        SearchScope::All,
    );

    let rows = table_rows(&hits, false);
    assert_eq!(rows[0][1], "N/A");
    assert_eq!(rows[0][3], "No description.");
}

#[test]
fn empty_messages_respect_the_selected_scope() {
    assert_eq!(
        SearchScope::All.empty_message("nope"),
        "No packages found matching 'nope'"
    );
    assert_eq!(
        SearchScope::Formulae.empty_message("nope"),
        "No formulae found matching 'nope'"
    );
    assert_eq!(
        SearchScope::Casks.empty_message("nope"),
        "No casks found matching 'nope'"
    );
}
