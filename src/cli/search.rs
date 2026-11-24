use anyhow::{Context, Result};
use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};

use crate::config::Config;
use crate::infra::api::catalog_cache::{self, CachePolicy};
use crate::infra::api::{BrewApiClient, Cask, Formula};
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

    let headers = table_headers(verbose);
    let rows = table_rows(&hits, verbose);
    println!(
        "{}",
        formatter.table(
            &format!("Search results for '{query}' ({})", rows.len()),
            &headers,
            &rows
        )
    );

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
mod tests;
