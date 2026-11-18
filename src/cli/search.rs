use anyhow::{Context, Result};
use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};

use crate::api::catalog_cache::{self, CachePolicy};
use crate::api::{BrewApiClient, Cask, Formula};
use crate::config::Config;
use crate::ui::{DnfStyleFormatter, ProgressManager};

/// Package families a search is allowed to return.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchScope {
    All,
    Formulae,
    Casks,
}

impl SearchScope {
    fn from_kind(kind: &super::PackageKind) -> Self {
        if kind.formula {
            Self::Formulae
        } else if kind.cask {
            Self::Casks
        } else {
            Self::All
        }
    }

    fn includes_formulae(self) -> bool {
        matches!(self, Self::All | Self::Formulae)
    }

    fn includes_casks(self) -> bool {
        matches!(self, Self::All | Self::Casks)
    }

    fn empty_message(self, query: &str) -> String {
        match self {
            Self::All => format!("No packages found matching '{query}'"),
            Self::Formulae => format!("No formulae found matching '{query}'"),
            Self::Casks => format!("No casks found matching '{query}'"),
        }
    }
}

/// Which Homebrew namespace a hit came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PackageType {
    Formula,
    Cask,
}

impl PackageType {
    fn label(self) -> &'static str {
        match self {
            Self::Formula => "formula",
            Self::Cask => "cask",
        }
    }

    fn order(self) -> u8 {
        match self {
            Self::Formula => 0,
            Self::Cask => 1,
        }
    }
}

/// One ranked search result, normalized across formulae and casks.
#[derive(Debug)]
struct SearchHit {
    /// The installable identifier: a formula name or a cask token.
    display_name: String,
    /// A cask's human-facing name, when it differs from the token.
    human_name: Option<String>,
    sort_name: String,
    version: String,
    description: String,
    homepage: String,
    kind: PackageType,
    score: i64,
    exact: bool,
}

/// Handle search command
pub async fn handle_search(
    config: Config,
    query: String,
    limit: usize,
    verbose: bool,
    kind: super::PackageKind,
    policy: CachePolicy,
) -> Result<()> {
    let formatter = DnfStyleFormatter::new(&config);
    let query = query.trim().to_string();
    let scope = SearchScope::from_kind(&kind);

    if limit == 0 || query.is_empty() {
        println!("{}", scope.empty_message(&query));
        return Ok(());
    }

    let progress = ProgressManager::new(config.ui.progress_bars);
    let client = BrewApiClient::new(&config)?;
    let spinner = progress.create_spinner(&format!("Searching for '{query}'..."));

    let fetched = fetch_packages(
        &client,
        &config,
        scope,
        policy,
        config.parallel_downloads() > 1,
    )
    .await;
    progress.finish_with_message(
        spinner,
        if fetched.is_ok() {
            "Search completed"
        } else {
            "Search failed"
        },
    );
    let (formulae, casks, warnings) = fetched?;

    for warning in warnings {
        eprintln!("{} {warning}", formatter.warning("Warning:"));
    }

    let hits = collect_search_hits(formulae, casks, &query, limit, scope);

    if hits.is_empty() {
        println!("{}", scope.empty_message(&query));
        return Ok(());
    }

    println!(
        "{}",
        formatter.header(&format!("Search results for '{query}'"))
    );

    let headers = table_headers(verbose);
    let rows = table_rows(&hits, verbose);
    println!("{}", formatter.table(&headers, &rows));

    Ok(())
}

/// Fetch the metadata needed for a search. Both lists are requested
/// concurrently when the search spans formulae and casks. The catalog cache is
/// consulted according to `policy`, and any non-fatal cache problems are
/// returned so the caller can warn about them.
async fn fetch_packages(
    client: &BrewApiClient,
    config: &Config,
    scope: SearchScope,
    policy: CachePolicy,
    parallel: bool,
) -> Result<(Vec<Formula>, Vec<Cask>, Vec<String>)> {
    match scope {
        SearchScope::All if parallel => {
            let (formulae, casks) = tokio::join!(
                catalog_cache::load_formulae(client, config, policy),
                catalog_cache::load_casks(client, config, policy),
            );
            let formulae = formulae.context("Failed to load the formula catalog")?;
            let casks = casks.context("Failed to load the cask catalog")?;
            let mut warnings = formulae.warnings;
            warnings.extend(casks.warnings);
            Ok((formulae.items, casks.items, warnings))
        }
        SearchScope::All => {
            let formulae = catalog_cache::load_formulae(client, config, policy)
                .await
                .context("Failed to load the formula catalog")?;
            let casks = catalog_cache::load_casks(client, config, policy)
                .await
                .context("Failed to load the cask catalog")?;
            let mut warnings = formulae.warnings;
            warnings.extend(casks.warnings);
            Ok((formulae.items, casks.items, warnings))
        }
        SearchScope::Formulae => {
            let formulae = catalog_cache::load_formulae(client, config, policy)
                .await
                .context("Failed to load the formula catalog")?;
            let warnings = formulae.warnings;
            Ok((formulae.items, Vec::new(), warnings))
        }
        SearchScope::Casks => {
            let casks = catalog_cache::load_casks(client, config, policy)
                .await
                .context("Failed to load the cask catalog")?;
            let warnings = casks.warnings;
            Ok((Vec::new(), casks.items, warnings))
        }
    }
}

/// Match, merge, and rank results. The limit applies to the merged
/// formula/cask list, so one namespace cannot consume the whole budget.
fn collect_search_hits(
    formulae: Vec<Formula>,
    casks: Vec<Cask>,
    query: &str,
    limit: usize,
    scope: SearchScope,
) -> Vec<SearchHit> {
    let mut hits = Vec::new();

    if scope.includes_formulae() {
        hits.extend(formula_hits(formulae, query));
    }
    if scope.includes_casks() {
        hits.extend(cask_hits(casks, query));
    }

    rank_hits(hits, limit)
}

fn rank_hits(mut hits: Vec<SearchHit>, limit: usize) -> Vec<SearchHit> {
    hits.sort_by(|a, b| {
        b.exact
            .cmp(&a.exact)
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| a.sort_name.cmp(&b.sort_name))
            .then_with(|| a.kind.order().cmp(&b.kind.order()))
    });
    hits.truncate(limit);
    hits
}

fn formula_hits(formulae: Vec<Formula>, query: &str) -> Vec<SearchHit> {
    if query.is_empty() {
        return Vec::new();
    }

    let matcher = SkimMatcherV2::default();
    let query_lower = query.to_lowercase();

    formulae
        .into_iter()
        .filter_map(|formula| {
            let exact = formula.name.to_lowercase() == query_lower
                || (!formula.full_name.is_empty()
                    && formula.full_name.to_lowercase() == query_lower);

            let name_score = matcher.fuzzy_match(&formula.name, query).unwrap_or(0);
            let desc_score = formula
                .desc
                .as_deref()
                .and_then(|desc| matcher.fuzzy_match(desc, query))
                .unwrap_or(0)
                / 2;
            let score = name_score + desc_score;

            if !exact && score <= 0 {
                return None;
            }

            Some(SearchHit {
                sort_name: formula.name.to_lowercase(),
                display_name: formula.name,
                human_name: None,
                version: formula.versions.stable.unwrap_or_else(|| "N/A".to_string()),
                description: formula.desc.unwrap_or_default(),
                homepage: formula.homepage.unwrap_or_default(),
                kind: PackageType::Formula,
                score,
                exact,
            })
        })
        .collect()
}

fn cask_hits(casks: Vec<Cask>, query: &str) -> Vec<SearchHit> {
    if query.is_empty() {
        return Vec::new();
    }

    let matcher = SkimMatcherV2::default();
    let query_lower = query.to_lowercase();

    casks
        .into_iter()
        .filter_map(|cask| {
            let exact = cask.token.to_lowercase() == query_lower
                || cask
                    .name
                    .iter()
                    .any(|name| name.to_lowercase() == query_lower);

            let token_score = matcher.fuzzy_match(&cask.token, query).unwrap_or(0);
            let display_name_score = cask
                .name
                .iter()
                .filter_map(|name| matcher.fuzzy_match(name, query))
                .max()
                .unwrap_or(0);
            let desc_score = cask
                .desc
                .as_deref()
                .and_then(|desc| matcher.fuzzy_match(desc, query))
                .unwrap_or(0)
                / 2;
            let score = token_score.max(display_name_score) + desc_score;

            if !exact && score <= 0 {
                return None;
            }

            // The token is what `brew install --cask` accepts, so it is the
            // package identifier shown in the table. The human name is kept
            // separately for search and for the description summary.
            let token = cask.token.clone();
            let display_name = if token.is_empty() {
                cask.name.first().cloned().unwrap_or_default()
            } else {
                token
            };
            let human_name = cask
                .name
                .iter()
                .find(|name| !name.is_empty() && **name != display_name)
                .cloned();
            let version = if cask.version.is_empty() {
                "N/A".to_string()
            } else {
                cask.version
            };

            Some(SearchHit {
                sort_name: cask.token.to_lowercase(),
                display_name,
                human_name,
                version,
                description: cask.desc.unwrap_or_default(),
                homepage: cask.homepage.unwrap_or_default(),
                kind: PackageType::Cask,
                score,
                exact,
            })
        })
        .collect()
}

fn table_headers(verbose: bool) -> Vec<&'static str> {
    let mut headers = vec!["Package", "Version", "Type", "Description"];
    if verbose {
        headers.push("Homepage");
    }
    headers
}

/// Describe a hit without hiding a cask's human-facing name behind its token.
fn hit_description(hit: &SearchHit) -> String {
    let description = hit.description.trim();
    match hit.human_name.as_deref() {
        Some(human_name) if description.is_empty() => human_name.to_string(),
        Some(human_name) => format!("{human_name} — {description}"),
        None if description.is_empty() => "No description.".to_string(),
        None => description.to_string(),
    }
}

fn table_rows(hits: &[SearchHit], verbose: bool) -> Vec<Vec<String>> {
    hits.iter()
        .map(|hit| {
            let mut row = vec![
                hit.display_name.clone(),
                hit.version.clone(),
                hit.kind.label().to_string(),
                hit_description(hit),
            ];
            if verbose {
                row.push(if hit.homepage.is_empty() {
                    "-".to_string()
                } else {
                    hit.homepage.clone()
                });
            }
            row
        })
        .collect()
}

#[cfg(test)]
mod tests {
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
}
