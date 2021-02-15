// Glimbot - A Discord anti-spam and administration bot.
// Copyright (C) 2020-2021 Nick Samson

// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Main entry point for Glimbot. Additionally controls DB migration.

#![feature(const_panic)]
#![feature(try_blocks)]
#![forbid(unsafe_code)]
#![deny(unused_must_use)]

#[macro_use] extern crate tracing;
#[macro_use] extern crate serde;

#[macro_use]
mod error;
#[macro_use]
mod dispatch;
mod about;
mod run;
mod module;
mod util;
mod db;

use tracing_subscriber::{FmtSubscriber, EnvFilter};
use clap::{SubCommand, AppSettings, ArgMatches};

#[cfg(not(target_env = "msvc"))]
use jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[doc(hidden)] // it's a main function
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    better_panic::install();
    let _ = dotenv::dotenv()?;
    let sub = FmtSubscriber::builder()
        .with_env_filter(
            EnvFilter::from_env("GLIMBOT_LOG")
        )
        .finish();

    tracing::subscriber::set_global_default(sub)?;
    let matches = clap::App::new(about::BIN_NAME)
        .version(about::VERSION)
        .about(about::LICENSE_HEADER)
        .author(about::AUTHOR_NAME)
        .subcommand(
            SubCommand::with_name("run")
                .help("Starts Glimbot.")
        )
        .setting(AppSettings::SubcommandRequired)
        .get_matches()
        ;

    match matches.subcommand() {
        ("run", _) => {
            info!("Starting Glimbot.");
            run::start_bot().await?;
        }
        _ => unreachable!("Unrecognized command; we should have errored out already.")
    }
    Ok(())
}