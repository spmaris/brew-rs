#![cfg_attr(not(debug_assertions), deny(warnings))]

mod api;
mod cache;
mod cli;
mod config;
mod native;
mod ui;

use std::process::ExitCode;

use clap::{CommandFactory, Parser, error::ErrorKind};

use cli::Cli;

#[tokio::main]
async fn main() -> ExitCode {
    let disable_color = std::env::var_os("NO_COLOR").is_some()
        || std::env::args_os()
            .skip(1)
            .take_while(|arg| arg != "--")
            .any(|arg| arg == "--no-color");
    if disable_color {
        console::set_colors_enabled(false);
        console::set_colors_enabled_stderr(false);
    }
    let cli = if disable_color {
        use clap::FromArgMatches;
        Cli::command()
            .color(clap::ColorChoice::Never)
            .try_get_matches()
            .and_then(|matches| Cli::from_arg_matches(&matches))
    } else {
        Cli::try_parse()
    };
    let cli = match cli {
        Ok(cli) => cli,
        Err(error) => {
            let code = if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            };
            let _ = error.print();
            return code;
        }
    };

    let verbose = cli.verbose || std::env::var_os("BREW_RS_VERBOSE").is_some();
    let env = env_logger::Env::default()
        .filter_or("BREW_RS_LOG", if verbose { "info" } else { "off" })
        .write_style_or("BREW_RS_LOG_STYLE", "auto");
    let mut logger = env_logger::Builder::from_env(env);
    if disable_color {
        logger.write_style(env_logger::WriteStyle::Never);
    }
    logger.format_module_path(false).format_target(false).init();

    let result = match cli.load_config() {
        Ok(config) => cli.execute(config).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            let mode = ui::table::Mode::detect_stderr(true);
            eprintln!("{}", ui::table::format_error(mode, &format!("{error:#}")));
            ExitCode::FAILURE
        }
    }
}
