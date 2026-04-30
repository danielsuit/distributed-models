// src/main.rs
use clap::{App, Arg};
use config::{Config, Config as OrchestratorConfig};
use orchestrator::run_orchestrator;
use worker::run_worker;

fn main() {
    let matches = App::new("Distributed AI Platform")
        .version("1.0")
        .author("Your Name <you@example.com>")
        .about("Builds a distributed multi-agent AI platform in Rust.")
        .arg(
            Arg::with_name("mode")
                .short('m')
                .long("mode")
                .takes_value(true)
                .possible_values(&["orchestrator", "worker"])
                .required(true),
        )
        .get_matches();

    let mode = matches.value_of("mode").unwrap();
    match mode {
        "orchestrator" => run_orchestrator().await,
        "worker" => run_worker().await,
        _ => unreachable!(),
    }
}