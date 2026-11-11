use llm_loadgen::cli::{self, Command, USAGE};
use llm_loadgen::mock::Server;
use llm_loadgen::runner;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args) {
        Ok(Command::Help) => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("llm-loadgen {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Run { config, json }) => match runner::run(&config) {
            Ok(report) => {
                print!("{}", report.text());
                if let Some(path) = json {
                    if let Err(e) = std::fs::write(&path, format!("{}\n", report.json())) {
                        eprintln!("llm-loadgen: cannot write {}: {e}", path.display());
                        return ExitCode::from(1);
                    }
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("llm-loadgen: {e}");
                ExitCode::from(2)
            }
        },
        Ok(Command::Mock { host, port, config }) => {
            match Server::start(&format!("{host}:{port}"), config) {
                Ok(server) => {
                    println!("mock endpoint listening on {}", server.url());
                    loop {
                        std::thread::park(); // until Ctrl-C
                    }
                }
                Err(e) => {
                    eprintln!("llm-loadgen: cannot listen on {host}:{port}: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Err(e) => {
            eprintln!("llm-loadgen: {e}\n\nrun llm-loadgen --help for the options");
            ExitCode::from(2)
        }
    }
}
