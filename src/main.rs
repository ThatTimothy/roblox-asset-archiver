use std::process::exit;

use log::debug;

mod api;
mod cli;
mod download;
mod progress;

#[tokio::main]
async fn main() {
    let result = cli::run().await;

    if let Err(error) = result {
        eprintln!("{error}");
        for cause in error.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
        debug!("{error:#?}");
        exit(1);
    }
}
