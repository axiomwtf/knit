use anyhow::Result;
use clap::Parser;
use knit::cli::Cli;

fn main() -> Result<()> {
    // The spec spells free-text flags as single-dash longs (-knot, -yarn).
    // Clap only knows -k/--knot, so rewrite the spec spellings up front.
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let rewritten: Vec<String> = args
        .into_iter()
        .map(|a| match a.as_str() {
            "-knot" => "--knot".to_string(),
            "-yarn" => "--yarn".to_string(),
            other => other.to_string(),
        })
        .collect();
    knit::cli::run(Cli::parse_from(rewritten))
}
