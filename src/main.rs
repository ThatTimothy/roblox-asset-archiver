use std::process::exit;

use console::style;
use log::debug;

mod api;
mod cli;
mod download;
mod progress;
mod state;

#[tokio::main]
async fn main() {
    let result = cli::run().await;

    if let Err(error) = result {
        eprintln!("{}", style(&error).for_stderr().red().bold());
        for cause in error.chain().skip(1) {
            eprintln!(
                "{}",
                style(format!("  caused by: {cause}")).for_stderr().red()
            );
        }
        debug!("{error:#?}");
        exit(1);
    }
}
