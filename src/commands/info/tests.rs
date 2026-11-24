use std::cell::Cell;

use super::*;

const FORMULA_JSON: &str = r#"{
        "name": "jq",
        "full_name": "jq",
        "tap": "homebrew/core",
        "desc": "Lightweight and flexible command-line JSON processor",
        "license": "MIT",
        "homepage": "https://jqlang.github.io/jq/",
        "versions": { "stable": "1.8.2", "head": "HEAD", "bottle": true },
        "revision": 0,
        "keg_only": false,
        "dependencies": ["oniguruma"],
        "build_dependencies": ["pkgconf"],
        "recommended_dependencies": [],
        "optional_dependencies": ["extra"],
        "uses_from_macos": ["krb5", { "unzip": "build" }],
        "conflicts_with": ["other"],
        "aliases": ["jq5"],
        "oldnames": ["jq4"],
        "deprecated": false,
        "disabled": false,
        "analytics": {
            "install": {
                "30d": { "jq": 62097, "jq --HEAD": 39 },
                "90d": { "jq": 171818, "jq --HEAD": 312 },
                "365d": { "jq": 822414, "jq --HEAD": 3530 }
            }
        }
    }"#;

const CASK_JSON: &str = r#"{
        "token": "firefox",
        "full_token": "firefox",
        "old_tokens": [],
        "tap": "homebrew/cask",
        "name": ["Mozilla Firefox"],
        "desc": "Web browser",
        "homepage": "https://www.mozilla.org/firefox/",
        "version": "141.0",
        "auto_updates": true,
        "deprecated": false,
        "disabled": false,
        "depends_on": { "macos": { ">=": ["13"] } },
        "conflicts_with": { "cask": ["firefox@beta"] },
        "analytics": { "install": { "30d": { "firefox": 40282 } } }
    }"#;

const ANALYTICS_JSON: &str = r#"{
        "category": "formula_install",
        "total_items": 8113,
        "start_date": "2026-10-20",
        "end_date": "2026-11-19",
        "total_count": 24541690,
        "formulae": {
            "jq": [
                { "formula": "jq", "count": "62,097" },
                { "formula": "jq --HEAD", "count": "39" }
            ],
            "other": [{ "formula": "other", "count": "1" }]
        }
    }"#;

#[derive(Clone, Copy)]
enum Failure {
    NotFound,
    Unavailable,
}

struct FixtureSource {
    formula: Option<&'static str>,
    cask: Option<&'static str>,
    analytics: &'static str,
    formula_failure: Option<Failure>,
    cask_failure: Option<Failure>,
    formula_calls: Cell<usize>,
    cask_calls: Cell<usize>,
    analytics_calls: Cell<usize>,
}

impl FixtureSource {
    fn new() -> Self {
        Self {
            formula: None,
            cask: None,
            analytics: "{}",
            formula_failure: None,
            cask_failure: None,
            formula_calls: Cell::new(0),
            cask_calls: Cell::new(0),
            analytics_calls: Cell::new(0),
        }
    }

    fn formula(mut self, document: &'static str) -> Self {
        self.formula = Some(document);
        self
    }

    fn cask(mut self, document: &'static str) -> Self {
        self.cask = Some(document);
        self
    }

    fn analytics_document(mut self, document: &'static str) -> Self {
        self.analytics = document;
        self
    }

    fn formula_failure(mut self, failure: Failure) -> Self {
        self.formula_failure = Some(failure);
        self
    }

    fn cask_failure(mut self, failure: Failure) -> Self {
        self.cask_failure = Some(failure);
        self
    }
}

impl MetadataSource for FixtureSource {
    async fn formula(&self, _name: &str) -> std::result::Result<FormulaResponse, LookupError> {
        self.formula_calls.set(self.formula_calls.get() + 1);
        match self.formula_failure {
            Some(Failure::NotFound) => Err(LookupError::NotFound),
            Some(Failure::Unavailable) => Err(LookupError::Failed(anyhow::anyhow!(
                "API request failed: HTTP 503 Service Unavailable"
            ))),
            None => {
                let document = self.formula.expect("formula fixture must be set");
                serde_json::from_str(document).map_err(|error| LookupError::Failed(error.into()))
            }
        }
    }

    async fn cask(&self, _token: &str) -> std::result::Result<CaskResponse, LookupError> {
        self.cask_calls.set(self.cask_calls.get() + 1);
        match self.cask_failure {
            Some(Failure::NotFound) => Err(LookupError::NotFound),
            Some(Failure::Unavailable) => Err(LookupError::Failed(anyhow::anyhow!(
                "API request failed: HTTP 503 Service Unavailable"
            ))),
            None => {
                let document = self.cask.expect("cask fixture must be set");
                serde_json::from_str(document).map_err(|error| LookupError::Failed(error.into()))
            }
        }
    }

    async fn analytics(&self, _kind: Kind, _period: &str) -> Result<AnalyticsResponse> {
        self.analytics_calls.set(self.analytics_calls.get() + 1);
        serde_json::from_str(self.analytics).context("invalid analytics fixture")
    }
}

#[test]
fn parses_official_identifiers_and_taps() {
    let plain = parse_package("jq").expect("plain name");
    assert_eq!(plain.name, "jq");
    assert_eq!(plain.kind, None);

    let versioned = parse_package("python@3.13").expect("versioned name");
    assert_eq!(versioned.name, "python@3.13");

    let core = parse_package("homebrew/core/jq").expect("core tap");
    assert_eq!(core.name, "jq");
    assert_eq!(core.kind, Some(Kind::Formula));

    let cask = parse_package("homebrew/cask/firefox").expect("cask tap");
    assert_eq!(cask.name, "firefox");
    assert_eq!(cask.kind, Some(Kind::Cask));
}

#[test]
fn rejects_unsafe_identifiers_and_unsupported_taps() {
    for bad in [
        "",
        "   ",
        "../jq",
        "jq/",
        "homebrew/core/",
        "homebrew/core/a/b",
        "user/repo/pkg",
        "homebrew/other/jq",
        "JQ",
        "-jq",
        ".jq",
        "jq?x",
        "jq x",
        "caf\u{e9}",
    ] {
        assert!(
            parse_package(bad).is_err(),
            "'{bad}' must be rejected before any request"
        );
    }
}

#[test]
fn kind_flags_are_explicit_and_unambiguous() {
    let cask = parse_package("homebrew/cask/firefox").expect("cask");
    assert!(requested_kind("homebrew/cask/firefox", &cask, true, false).is_err());

    let plain = parse_package("firefox").expect("plain");
    assert!(requested_kind("firefox", &plain, true, true).is_err());
    assert_eq!(
        requested_kind("firefox", &plain, false, true).expect("cask flag"),
        Some(Kind::Cask)
    );

    let cask = parse_package("homebrew/cask/firefox").expect("cask");
    assert_eq!(
        requested_kind("homebrew/cask/firefox", &cask, false, false).expect("tap kind"),
        Some(Kind::Cask)
    );
}

#[test]
fn only_a_typed_http_404_counts_as_not_found() {
    // Error text alone never qualifies; the status must be typed. The
    // positive path is exercised end-to-end in the `HttpClient` tests.
    assert!(!is_not_found(&anyhow::anyhow!(
        "API request failed: HTTP 404 Not Found"
    )));
    assert!(!is_not_found(&anyhow::anyhow!(
        "API request failed: HTTP 503 Service Unavailable"
    )));
    assert!(!is_not_found(&anyhow::anyhow!(
        "Failed to send request: connection refused"
    )));
}

#[tokio::test]
async fn bare_name_prefers_formula_when_both_namespaces_exist() {
    let source = FixtureSource::new().formula(FORMULA_JSON).cask(CASK_JSON);
    let metadata = lookup(&source, "jq", None).await.expect("formula");
    assert_eq!(metadata.kind, Kind::Formula);
    assert_eq!(metadata.name, "jq");
    assert_eq!(source.cask_calls.get(), 0);
}

#[tokio::test]
async fn formula_404_falls_back_to_the_cask() {
    let source = FixtureSource::new()
        .formula_failure(Failure::NotFound)
        .cask(CASK_JSON);
    let metadata = lookup(&source, "firefox", None).await.expect("cask");
    assert_eq!(metadata.kind, Kind::Cask);
    assert_eq!(metadata.name, "firefox");
}

#[tokio::test]
async fn a_non_404_formula_failure_is_not_hidden_by_the_fallback() {
    let source = FixtureSource::new()
        .formula_failure(Failure::Unavailable)
        .cask(CASK_JSON);
    let error = lookup(&source, "jq", None)
        .await
        .expect_err("503 must fail");
    assert!(format!("{error:#}").contains("503"));
    assert_eq!(source.cask_calls.get(), 0);
}

#[tokio::test]
async fn two_404s_report_a_missing_package() {
    let source = FixtureSource::new()
        .formula_failure(Failure::NotFound)
        .cask_failure(Failure::NotFound);
    let error = lookup(&source, "missing", None)
        .await
        .expect_err("missing package");
    assert!(error.to_string().contains("No formula or cask named"));
}

#[tokio::test]
async fn an_explicit_cask_never_probes_the_formula_endpoint() {
    let source = FixtureSource::new().cask_failure(Failure::NotFound);
    let error = lookup(&source, "firefox", Some(Kind::Cask))
        .await
        .expect_err("missing cask");
    assert!(error.to_string().contains("No cask named"));
    assert_eq!(source.formula_calls.get(), 0);
}

#[test]
fn analytics_urls_are_tap_scoped() {
    assert_eq!(
        analytics_url(Kind::Formula, "30d"),
        "https://formulae.brew.sh/api/analytics/install/homebrew-core/30d.json"
    );
    assert_eq!(
        analytics_url(Kind::Cask, "365d"),
        "https://formulae.brew.sh/api/analytics/cask-install/homebrew-cask/365d.json"
    );
}

#[test]
fn period_validation_lists_supported_values() {
    assert_eq!(validate_period("30d").expect("30d"), "30d");
    assert_eq!(validate_period(" 90d ").expect("trimmed"), "90d");
    let error = validate_period("7d").expect_err("7d is unsupported");
    assert!(error.to_string().contains("30d, 90d, 365d"));
}

#[test]
fn analytics_breakdown_reads_variants_and_dates() {
    let response: AnalyticsResponse =
        serde_json::from_str(ANALYTICS_JSON).expect("analytics fixture");
    let entries = response.formulae.get("jq").expect("jq entries");
    let rows = analytics_breakdown("jq", entries).expect("breakdown");
    assert_eq!(
        rows,
        vec![("jq".to_string(), 62_097), ("jq --HEAD".to_string(), 39)]
    );
    assert_eq!(
        category_display(&response.category, Kind::Formula),
        "formula_install"
    );
    assert_eq!(
        period_description("30d", &response),
        "30d (2026-10-20 to 2026-11-19)"
    );
    assert_eq!(format_percent(62_097, 24_541_690).as_deref(), Some("0.25%"));
}

#[test]
fn analytics_breakdown_rejects_malformed_counts() {
    let entry = AnalyticsEntry {
        formula: Some("jq".to_string()),
        cask: None,
        count: "62,,097".to_string(),
    };
    assert!(analytics_breakdown("jq", &[entry]).is_err());
}

#[test]
fn missing_package_is_reported_as_no_data() {
    let response: AnalyticsResponse =
        serde_json::from_str(ANALYTICS_JSON).expect("analytics fixture");
    assert!(!response.formulae.contains_key("absent"));
    let metadata = Metadata {
        kind: Kind::Formula,
        name: "absent".to_string(),
        formula: None,
        cask: None,
    };
    let message = no_analytics_message(&metadata, "30d", &response);
    assert!(message.contains("No formula_install analytics recorded for 'absent'"));
    assert!(message.contains("2026-10-20 to 2026-11-19"));
}

#[test]
fn formats_and_parses_install_counts() {
    assert_eq!(format_count(0), "0");
    assert_eq!(format_count(62_097), "62,097");
    assert_eq!(format_count(24_541_690), "24,541,690");
    assert_eq!(parse_count("62,097").expect("grouped"), 62_097);
    assert_eq!(parse_count("0").expect("zero"), 0);
    for bad in ["", " ", "62,,097", "62097,", "0062,097", "abc", "1,2"] {
        assert!(parse_count(bad).is_err(), "'{bad}' must be rejected");
    }
}

#[test]
fn inline_analytics_prefers_exact_counts_and_orders_periods() {
    let analytics: InlineAnalytics = serde_json::from_str(
        r#"{"install":{"365d":{"jq":3,"jq --HEAD":1},"30d":{"jq":62,"jq --HEAD":2},"90d":{"jq":171}}}"#,
    )
    .expect("inline analytics");
    let rows = inline_analytics_rows("jq", &analytics);
    let periods: Vec<&str> = rows.iter().map(|row| row[0].as_str()).collect();
    assert_eq!(periods, ["30d", "90d", "365d"]);
    assert_eq!(rows[0], vec!["30d", "62", "2"]);
    assert_eq!(rows[2], vec!["365d", "3", "1"]);
}

#[test]
fn inline_analytics_ignores_unrelated_names() {
    let analytics: InlineAnalytics =
        serde_json::from_str(r#"{"install":{"30d":{"other":5}}}"#).expect("inline");
    assert!(inline_analytics_rows("jq", &analytics).is_empty());
}

#[test]
fn formula_versions_include_revision_and_head() {
    let mut formula: FormulaResponse = serde_json::from_str(FORMULA_JSON).expect("formula fixture");
    assert_eq!(formula_versions(&formula), "stable 1.8.2, head HEAD");
    formula.revision = Some(2);
    assert_eq!(formula_versions(&formula), "stable 1.8.2_2, head HEAD");
    formula.versions.head = None;
    formula.versions.stable = None;
    assert_eq!(formula_versions(&formula), "stable none");
}

#[test]
fn formula_dependencies_cover_optional_and_macos_scopes() {
    let formula: FormulaResponse = serde_json::from_str(FORMULA_JSON).expect("formula fixture");
    let rows = formula_dependency_rows(&formula);
    assert!(rows.contains(&vec!["Runtime".to_string(), "oniguruma".to_string()]));
    assert!(rows.contains(&vec!["Optional".to_string(), "extra".to_string()]));
    assert!(rows.contains(&vec!["Build".to_string(), "pkgconf".to_string()]));
    assert!(rows.contains(&vec!["macOS".to_string(), "krb5".to_string()]));
    assert!(rows.contains(&vec!["macOS".to_string(), "unzip (build)".to_string()]));
}

#[test]
fn cask_dependencies_render_versions_and_names() {
    let depends_on = serde_json::json!({
        "macos": { ">=": ["13"] },
        "arch": ["arm64"],
        "formula": ["lib"],
        "cask": ["helper"],
        "x11": true
    });
    assert_eq!(
        cask_dependency_rows(&depends_on),
        vec![
            vec!["macOS".to_string(), ">= 13".to_string()],
            vec!["Architecture".to_string(), "arm64".to_string()],
            vec!["Formula".to_string(), "lib".to_string()],
            vec!["Cask".to_string(), "helper".to_string()],
            vec!["X11".to_string(), "yes".to_string()],
        ]
    );
}

#[test]
fn cask_conflicts_are_grouped_by_namespace() {
    let conflicts = serde_json::json!({ "cask": ["firefox@beta", "firefox@esr"] });
    assert_eq!(
        cask_conflicts_value(&conflicts).as_deref(),
        Some("firefox@beta, firefox@esr (cask)")
    );
}

#[test]
fn installed_overlay_reports_version_and_update_state() {
    let installed = Installed::Present {
        version: "1.7.0".to_string(),
        requested: true,
    };
    let described = describe_installed(&installed, Some("1.8.2"), 0);
    assert!(described.contains("1.7.0"));
    assert!(described.contains("requested"));
    assert!(described.contains("update available"));

    let current = Installed::Present {
        version: "1.8.2".to_string(),
        requested: false,
    };
    let described = describe_installed(&current, Some("1.8.2"), 0);
    assert!(described.contains("dependency"));
    assert!(described.contains("up to date"));

    let unsupported = Installed::Present {
        version: "1.8.2-rc1".to_string(),
        requested: true,
    };
    let described = describe_installed(&unsupported, Some("1.8.2"), 0);
    assert!(!described.contains("available"));
}

#[test]
fn an_unowned_prefix_has_no_installed_overlay() {
    let prefix = tempfile::tempdir().expect("tempdir");
    match read_installed(Some(prefix.path()), "jq") {
        Installed::Absent => {}
        other => panic!("unowned prefix must be absent, got {other:?}"),
    }
    match read_installed_cask(Some(prefix.path()), "firefox") {
        InstalledCask::Absent => {}
        other => panic!("unowned prefix must be absent for casks, got {other:?}"),
    }
}

#[test]
fn cask_installed_overlay_reports_version_and_update_state() {
    assert_eq!(
        describe_installed_cask(
            &InstalledCask::Present {
                version: "141.0".into(),
            },
            "142.0",
        ),
        "141.0 (update available)"
    );
    assert_eq!(
        describe_installed_cask(
            &InstalledCask::Present {
                version: "142.0".into(),
            },
            "142.0",
        ),
        "142.0 (up to date)"
    );
    assert_eq!(
        describe_installed_cask(&InstalledCask::Absent, "142.0"),
        "not installed by brewrs"
    );
}

#[test]
fn status_messages_name_the_replacement() {
    let formula: FormulaResponse = serde_json::from_str(
        r#"{
                "name": "jq",
                "deprecated": true,
                "deprecation_date": "2024-01-01",
                "deprecation_reason": "unmaintained",
                "deprecation_replacement_formula": "jaq"
            }"#,
    )
    .expect("status fixture");
    let messages = formula_status_messages(&formula, "jq");
    assert_eq!(messages.len(), 1);
    assert!(messages[0].contains("deprecated since 2024-01-01"));
    assert!(messages[0].contains("replacement: jaq"));
}

#[test]
fn caveats_localize_homebrew_prefix_placeholders() {
    let prefix = Path::new("/Users/me/Brewrs Space");
    let text = "export PATH=${HOMEBREW_PREFIX}/bin:$HOMEBREW_PREFIX/sbin\n\
                    cargs=--cellar=$HOMEBREW_CELLAR\n\
                    cfg=${HOMEBREW_CELLAR}/pkg\n\
                    helper=$(brew --prefix)";
    assert_eq!(
        localize_caveats(text, prefix),
        "export PATH=/Users/me/Brewrs Space/bin:/Users/me/Brewrs Space/sbin\n\
             cargs=--cellar=/Users/me/Brewrs Space/Cellar\n\
             cfg=/Users/me/Brewrs Space/Cellar/pkg\n\
             helper=/Users/me/Brewrs Space"
    );
}

#[tokio::test]
async fn analytics_source_is_used_for_the_requested_period() {
    let source = FixtureSource::new().analytics_document(ANALYTICS_JSON);
    let response = source
        .analytics(Kind::Formula, "30d")
        .await
        .expect("analytics");
    assert_eq!(source.analytics_calls.get(), 1);
    assert_eq!(response.total_count, 24_541_690);
}
