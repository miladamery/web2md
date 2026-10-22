//! Entry point: parse arguments, resolve settings, run the crawl, report.

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use web2md::cli::Cli;
use web2md::config::Settings;
use web2md::pipeline::{self, Summary};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match run(&cli).await {
        Ok(code) => code,
        Err(e) => {
            // The outer message is what the user needs; the causes are only
            // interesting when they ask for more.
            eprintln!("web2md: {e}");
            let mut source = std::error::Error::source(&e);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: &Cli) -> web2md::Result<ExitCode> {
    let file = Settings::load_file(cli.config.as_deref())?;
    let settings = Settings::resolve(cli, file)?;

    let summary = pipeline::run(settings).await?;
    report(&summary);

    // Per-page failures are expected on a real crawl — a dead link, a page
    // that isn't HTML — so they are reported but do not fail the run. Only a
    // run that produced nothing at all is a failure.
    Ok(if summary.written == 0 && !summary.failed.is_empty() && !summary.dry_run {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn report(s: &Summary) {
    let dir = s.output_dir.display();

    if s.dry_run {
        println!("{} URL(s) in scope:", s.urls.len());
        for u in &s.urls {
            println!("  {u}");
        }
        println!("\n(dry run — nothing was fetched or written)");
        return;
    }

    println!("wrote {} page(s) to {dir}", s.written);
    if s.unchanged > 0 {
        println!("  {} unchanged (server reported no modification)", s.unchanged);
    }
    if s.skipped > 0 {
        println!("  {} skipped (already on disk)", s.skipped);
    }
    if let Some(index) = &s.index {
        println!("  index: {}", index.display());
    }
    if let Some(merged) = &s.merged {
        println!("  merged: {}", merged.display());
    }

    // Warnings are deduplicated with a count: one site-wide pattern (say, a
    // profile that no longer matches) otherwise prints once per page.
    if !s.warnings.is_empty() {
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for w in &s.warnings {
            match counts.iter_mut().find(|(m, _)| *m == w.as_str()) {
                Some((_, n)) => *n += 1,
                None => counts.push((w.as_str(), 1)),
            }
        }
        eprintln!("\n{} warning(s):", s.warnings.len());
        for (msg, n) in counts {
            if n > 1 {
                eprintln!("  {msg} (x{n})");
            } else {
                eprintln!("  {msg}");
            }
        }
    }

    if !s.failed.is_empty() {
        eprintln!("\n{} page(s) failed:", s.failed.len());
        // A crawl of a few thousand pages can fail on a few dozen; listing all
        // of them buries the summary, and the rest are in the log.
        for (url, reason) in s.failed.iter().take(20) {
            eprintln!("  {url}: {reason}");
        }
        if s.failed.len() > 20 {
            eprintln!("  ... and {} more (run with -v for detail)", s.failed.len() - 20);
        }
    }
}

/// `RUST_LOG` wins when set; otherwise `-v` sets the level.
fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    // `html5ever` reports "foster parenting not implemented" once per misnested
    // table cell. Asciidoctor pages trip it constantly — one Spring page emits
    // hundreds of lines — and none of them are actionable here: it is a note
    // about an unimplemented corner of the HTML5 parser, not about this program.
    let html5ever = if verbose >= 3 { "html5ever=trace" } else { "html5ever=off" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!("web2md={default},warn,{html5ever}"))
    });

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
