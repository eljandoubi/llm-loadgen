//! Command line parsing, with no dependencies.

use crate::arrival::Process;
use crate::client::{self, RequestSpec};
use crate::http::parse_url;
use crate::mock::{MockConfig, Stall};
use crate::runner::{Mode, RunConfig};
use std::path::PathBuf;
use std::time::Duration;

pub const USAGE: &str = "\
llm-loadgen: open-loop load generator for OpenAI-compatible streaming endpoints

USAGE
    llm-loadgen run --url URL [options]     send load and print latencies
    llm-loadgen mock [options]              serve a fake endpoint to try it against
    llm-loadgen --help | --version

RUN OPTIONS
    --url URL            endpoint, e.g. http://localhost:8000/v1/chat/completions (http only)
    --model NAME         model name in the request                       [default]
    --mode open|closed   open: requests start on a schedule whatever the server does;
                         closed: clients wait for each answer (hides stalls)     [open]
    --arrival KIND       poisson | constant (open mode)                      [poisson]
    --rate N             requests per second (open mode)                           [10]
    --duration T         measured time, e.g. 30, 30s, 500ms, 2m                    [30s]
    --warmup T           load sent before the measured window                       [2s]
    --connections N      open: most requests in flight; closed: number of clients  [64]
    --max-queue N        open: requests due and waiting before new ones are dropped [1000]
    --prompt-words N     words in the prompt                                       [64]
    --max-tokens N       tokens to ask for                                        [128]
    --timeout T          connect, and silence on a stream, before a request fails [120s]
    --seed N             seed of the schedule and the prompt                         [1]
    --api-key KEY        sent as a bearer token (default: the OPENAI_API_KEY variable)
    --json PATH          also write the report as JSON

MOCK OPTIONS
    --host H --port N    where to listen                                 [127.0.0.1:8000]
    --ttft-ms N          time to first token                                       [50]
    --itl-ms N           time between tokens                                       [10]
    --tokens N           tokens when the request does not say                      [32]
    --slots N            generations at once; the rest wait their turn              [8]
    --stall AFTER:LEN    freeze everything for LEN seconds, AFTER seconds in, e.g. 5:1
    --error-status N     answer every request with this status
    --die-after N        close the connection after N tokens
";

#[derive(Debug)]
pub enum Command {
    Help,
    Version,
    Run {
        config: Box<RunConfig>,
        json: Option<PathBuf>,
    },
    Mock {
        host: String,
        port: u16,
        config: MockConfig,
    },
}

/// `30`, `30s`, `1.5s`, `500ms`, `2m`, `1h`: a bare number is seconds.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let bad = || format!("{text:?} is not a duration: use 30, 30s, 500ms, 2m or 1h");
    let (number, unit) = match text.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => text.split_at(i),
        None => (text, "s"),
    };
    let value: f64 = number.parse().map_err(|_| bad())?;
    if !value.is_finite() || value < 0.0 {
        return Err(bad());
    }
    let seconds = match unit {
        "ms" => value / 1000.0,
        "s" => value,
        "m" => value * 60.0,
        "h" => value * 3600.0,
        _ => return Err(bad()),
    };
    Duration::try_from_secs_f64(seconds).map_err(|_| bad())
}

/// `--name value` and `--name=value` pairs.
fn pairs(args: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let Some(name) = arg.strip_prefix("--") else {
            return Err(format!("unexpected argument {arg:?}"));
        };
        if let Some((name, value)) = name.split_once('=') {
            out.push((name.to_string(), value.to_string()));
            i += 1;
        } else {
            let value = args
                .get(i + 1)
                .ok_or_else(|| format!("--{name} needs a value"))?;
            out.push((name.to_string(), value.clone()));
            i += 2;
        }
    }
    Ok(out)
}

fn number<T: std::str::FromStr>(name: &str, value: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("--{name}: {value:?} is not a valid number"))
}

fn duration_option(name: &str, value: &str) -> Result<Duration, String> {
    parse_duration(value).map_err(|e| format!("--{name}: {e}"))
}

fn parse_run(args: &[String]) -> Result<Command, String> {
    let mut url = None;
    let mut model = "default".to_string();
    let mut mode = Mode::Open;
    let mut process = Process::Poisson;
    let mut rate = 10.0;
    let mut duration = Duration::from_secs(30);
    let mut warmup = Duration::from_secs(2);
    let mut connections = 64;
    let mut max_queue = 1000;
    let mut prompt_words = 64;
    let mut max_tokens = 128;
    let mut timeout = Duration::from_secs(120);
    let mut seed = 1;
    let mut api_key = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|k| !k.is_empty());
    let mut json = None;
    for (name, value) in pairs(args)? {
        match name.as_str() {
            "url" => url = Some(value),
            "model" => model = value,
            "mode" => {
                mode = match value.as_str() {
                    "open" => Mode::Open,
                    "closed" => Mode::Closed,
                    _ => return Err(format!("--mode: {value:?} is not open or closed")),
                }
            }
            "arrival" => {
                process = match value.as_str() {
                    "poisson" => Process::Poisson,
                    "constant" => Process::Constant,
                    _ => return Err(format!("--arrival: {value:?} is not poisson or constant")),
                }
            }
            "rate" => rate = number("rate", &value)?,
            "duration" => duration = duration_option("duration", &value)?,
            "warmup" => warmup = duration_option("warmup", &value)?,
            "connections" => connections = number("connections", &value)?,
            "max-queue" => max_queue = number("max-queue", &value)?,
            "prompt-words" => prompt_words = number("prompt-words", &value)?,
            "max-tokens" => max_tokens = number("max-tokens", &value)?,
            "timeout" => timeout = duration_option("timeout", &value)?,
            "seed" => seed = number("seed", &value)?,
            "api-key" => api_key = Some(value),
            "json" => json = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown option --{name} for run")),
        }
    }
    let url = url.ok_or("--url is required")?;
    let url = parse_url(&url).map_err(|e| format!("--url: {e}"))?;
    if timeout.is_zero() {
        return Err("--timeout must be longer than zero".to_string());
    }
    let request = RequestSpec {
        url,
        model,
        prompt: client::prompt(prompt_words, seed),
        max_tokens,
        timeout,
        api_key,
    };
    let config = RunConfig {
        mode,
        process,
        rate,
        duration,
        warmup,
        connections,
        max_queue,
        seed,
        request,
    };
    Ok(Command::Run {
        config: Box::new(config),
        json,
    })
}

fn parse_stall(value: &str) -> Result<Stall, String> {
    let (after, length) = value
        .split_once(':')
        .ok_or_else(|| format!("--stall: {value:?} is not AFTER:LEN, for example 5:1"))?;
    Ok(Stall {
        after: parse_duration(after).map_err(|e| format!("--stall: {e}"))?,
        length: parse_duration(length).map_err(|e| format!("--stall: {e}"))?,
    })
}

fn parse_mock(args: &[String]) -> Result<Command, String> {
    let mut host = "127.0.0.1".to_string();
    let mut port = 8000;
    let mut config = MockConfig::default();
    for (name, value) in pairs(args)? {
        match name.as_str() {
            "host" => host = value,
            "port" => port = number("port", &value)?,
            "ttft-ms" => config.ttft = Duration::from_millis(number("ttft-ms", &value)?),
            "itl-ms" => config.itl = Duration::from_millis(number("itl-ms", &value)?),
            "tokens" => config.default_tokens = number("tokens", &value)?,
            "slots" => config.slots = number("slots", &value)?,
            "stall" => config.stall = Some(parse_stall(&value)?),
            "error-status" => config.error_status = Some(number("error-status", &value)?),
            "die-after" => config.die_after_tokens = Some(number("die-after", &value)?),
            _ => return Err(format!("unknown option --{name} for mock")),
        }
    }
    Ok(Command::Mock { host, port, config })
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    match args.first().map(String::as_str) {
        None | Some("--help" | "-h" | "help") => Ok(Command::Help),
        Some("--version" | "-V") => Ok(Command::Version),
        Some("run") => parse_run(&args[1..]),
        Some("mock") => parse_mock(&args[1..]),
        Some(other) => Err(format!("unknown command {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    fn run_config(line: &str) -> RunConfig {
        match parse(&args(line)).unwrap() {
            Command::Run { config, .. } => *config,
            other => panic!("not a run: {other:?}"),
        }
    }

    #[test]
    fn durations() {
        for (text, want) in [
            ("30", 30.0),
            ("30s", 30.0),
            ("1.5s", 1.5),
            ("500ms", 0.5),
            ("2m", 120.0),
            ("1h", 3600.0),
            ("0", 0.0),
        ] {
            assert_eq!(
                parse_duration(text).unwrap(),
                Duration::from_secs_f64(want),
                "{text}"
            );
        }
        for bad in [
            "", "s", "abc", "-1", "1x", "1.2.3", "nan", "inf", "1e999s", "5 s",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_run_needs_only_a_url_and_has_sensible_defaults() {
        let cfg = run_config("run --url http://localhost:8000/v1/chat/completions");
        assert_eq!(cfg.mode, Mode::Open);
        assert_eq!(cfg.process, Process::Poisson);
        assert!((cfg.rate - 10.0).abs() < f64::EPSILON);
        assert_eq!(cfg.duration, Duration::from_secs(30));
        assert_eq!(cfg.warmup, Duration::from_secs(2));
        assert_eq!((cfg.connections, cfg.max_queue), (64, 1000));
        assert_eq!(cfg.request.max_tokens, 128);
        assert_eq!(cfg.request.prompt.split_whitespace().count(), 64);
        assert_eq!(cfg.request.timeout, Duration::from_secs(120));
        assert_eq!(cfg.request.url.port, 8000);
        assert_eq!(cfg.request.model, "default");
    }

    #[test]
    fn every_option_can_be_set_with_a_space_or_an_equals_sign() {
        let cfg = run_config(
            "run --url=http://h:1/x --model m --mode closed --arrival constant --rate=2.5 \
             --duration 10s --warmup=500ms --connections 3 --max-queue 7 --prompt-words 5 \
             --max-tokens 9 --timeout 4 --seed 99 --api-key=k --json out.json",
        );
        assert_eq!(cfg.mode, Mode::Closed);
        assert_eq!(cfg.process, Process::Constant);
        assert!((cfg.rate - 2.5).abs() < f64::EPSILON);
        assert_eq!(
            (cfg.duration, cfg.warmup),
            (Duration::from_secs(10), Duration::from_millis(500))
        );
        assert_eq!((cfg.connections, cfg.max_queue, cfg.seed), (3, 7, 99));
        assert_eq!(cfg.request.prompt.split_whitespace().count(), 5);
        assert_eq!(
            (cfg.request.max_tokens, cfg.request.timeout),
            (9, Duration::from_secs(4))
        );
        assert_eq!(cfg.request.api_key.as_deref(), Some("k"));
        assert_eq!(cfg.request.model, "m");
        let Command::Run { json, .. } =
            parse(&args("run --url http://h/ --json out.json")).unwrap()
        else {
            panic!()
        };
        assert_eq!(json, Some(PathBuf::from("out.json")));
    }

    #[test]
    fn the_last_occurrence_of_an_option_wins() {
        assert_eq!(
            run_config("run --url http://h/ --connections 1 --connections 2").connections,
            2
        );
    }

    #[test]
    fn mistakes_are_explained() {
        let cases = [
            ("run", "--url is required"),
            ("run --url", "--url needs a value"),
            (
                "run --url http://h/ --rate fast",
                "--rate: \"fast\" is not a valid number",
            ),
            ("run --url http://h/ --duration soon", "--duration:"),
            (
                "run --url http://h/ --mode fast",
                "--mode: \"fast\" is not open or closed",
            ),
            ("run --url http://h/ --arrival bursty", "--arrival:"),
            (
                "run --url http://h/ --nope 1",
                "unknown option --nope for run",
            ),
            ("run --url https://h/", "no TLS"),
            ("run --url http://h/ stray", "unexpected argument \"stray\""),
            (
                "run --url http://h/ --timeout 0",
                "--timeout must be longer than zero",
            ),
            ("run --url http://h/ --connections -1", "--connections"),
            ("frobnicate", "unknown command \"frobnicate\""),
            ("mock --nope 1", "unknown option --nope for mock"),
            ("mock --stall 5", "--stall:"),
            ("mock --port 99999", "--port"),
        ];
        for (line, expected) in cases {
            let err = parse(&args(line)).unwrap_err();
            assert!(
                err.contains(expected),
                "{line:?} gave {err:?}, wanted {expected:?}"
            );
        }
    }

    #[test]
    fn help_and_version() {
        for line in ["", "--help", "-h", "help"] {
            assert!(
                matches!(parse(&args(line)).unwrap(), Command::Help),
                "{line:?}"
            );
        }
        for line in ["--version", "-V"] {
            assert!(matches!(parse(&args(line)).unwrap(), Command::Version));
        }
    }

    #[test]
    fn the_usage_text_names_every_option_the_parser_accepts() {
        for option in [
            "url",
            "model",
            "mode",
            "arrival",
            "rate",
            "duration",
            "warmup",
            "connections",
            "max-queue",
            "prompt-words",
            "max-tokens",
            "timeout",
            "seed",
            "api-key",
            "json",
            "host",
            "port",
            "ttft-ms",
            "itl-ms",
            "tokens",
            "slots",
            "stall",
            "error-status",
            "die-after",
        ] {
            assert!(
                USAGE.contains(&format!("--{option}")),
                "--{option} is not in the usage text"
            );
        }
    }

    #[test]
    fn the_mock_has_defaults_and_options() {
        let Command::Mock { host, port, config } = parse(&args("mock")).unwrap() else {
            panic!()
        };
        assert_eq!((host.as_str(), port), ("127.0.0.1", 8000));
        assert_eq!(config.slots, 8);
        let Command::Mock { host, port, config } = parse(&args(
            "mock --host 0.0.0.0 --port 9000 --ttft-ms 5 --itl-ms 2 --tokens 3 --slots 1 \
             --stall 5:1.5 --error-status 503 --die-after 2",
        ))
        .unwrap() else {
            panic!()
        };
        assert_eq!((host.as_str(), port), ("0.0.0.0", 9000));
        assert_eq!(
            (config.ttft, config.itl),
            (Duration::from_millis(5), Duration::from_millis(2))
        );
        assert_eq!((config.default_tokens, config.slots), (3, 1));
        let stall = config.stall.unwrap();
        assert_eq!(
            (stall.after, stall.length),
            (Duration::from_secs(5), Duration::from_millis(1500))
        );
        assert_eq!(
            (config.error_status, config.die_after_tokens),
            (Some(503), Some(2))
        );
    }
}
