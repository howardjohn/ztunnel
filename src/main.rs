// Copyright Istio Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

extern crate core;
#[cfg(feature = "gperftools")]
extern crate gperftools;
use tracing::info;
use ztunnel::*;
use tikv_jemallocator::Jemalloc;

static GLOBAL: Jemalloc = Jemalloc;
// #[global_allocator]
// static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// #[global_allocator]
// static GLOBAL: tcmalloc::TCMalloc = tcmalloc::TCMalloc;

fn main() -> anyhow::Result<()> {
    telemetry::setup_logging();
    let config: config::Config = config::parse_config()?;

    // For now we don't need a complex CLI, so rather than pull in dependencies just use basic argv[1]
    match std::env::args().nth(1).as_deref() {
        None | Some("proxy") => (),
        Some("version") => return version(),
        Some("help") => return help(),
        Some(unknown) => {
            eprintln!("unknown command: {unknown}");
            help().unwrap();
            std::process::exit(1)
        }
    };

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move { proxy(config).await })
}

fn help() -> anyhow::Result<()> {
    let version = version::BuildInfo::new();
    println!(
        "
Istio Ztunnel ({version})

Commands:
proxy (default) - Start the ztunnel proxy
version         - Print the version of ztunnel
help            - Print commands and version of ztunnel"
    );
    Ok(())
}

fn version() -> anyhow::Result<()> {
    println!("{}", version::BuildInfo::new());
    Ok(())
}

async fn proxy(cfg: config::Config) -> anyhow::Result<()> {
    info!("version: {}", version::BuildInfo::new());
    info!("running with config: {}", serde_yaml::to_string(&cfg)?);
    app::build(cfg).await?.wait_termination().await
}
