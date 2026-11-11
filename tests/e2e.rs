//! The real binary, run as a user would run it.

use llm_loadgen::json::{self, Value};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_llm-loadgen");

/// A `llm-loadgen mock` process, killed when dropped.
struct Mock {
    child: Child,
    url: String,
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_mock(options: &[&str]) -> Mock {
    let mut child = Command::new(BIN)
        .args(["mock", "--port", "0"])
        .args(options)
        .stdout(Stdio::piped())
        .spawn()
        .expect("the binary starts");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = line
        .trim()
        .strip_prefix("mock endpoint listening on ")
        .unwrap_or_else(|| panic!("unexpected first line {line:?}"))
        .to_string();
    Mock { child, url }
}

fn loadgen(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("the binary runs")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}

fn temp_file(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("llm-loadgen-{}-{name}", std::process::id()))
}

fn run_json(mock: &Mock, name: &str, options: &[&str]) -> Value {
    let out = temp_file(name);
    let mut args = vec![
        "run",
        "--url",
        &mock.url,
        "--warmup",
        "0",
        "--json",
        out.to_str().unwrap(),
    ];
    args.extend_from_slice(options);
    let output = loadgen(&args);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let report = json::parse(&std::fs::read_to_string(&out).unwrap()).unwrap();
    let _ = std::fs::remove_file(out);
    report
}

fn number(report: &Value, path: &[&str]) -> f64 {
    let mut value = report;
    for key in path {
        value = value
            .get(key)
            .unwrap_or_else(|| panic!("no {key} in the report"));
    }
    value.as_f64().unwrap()
}

#[test]
fn a_run_prints_the_report_and_writes_the_json() {
    let mock = start_mock(&["--ttft-ms", "20", "--itl-ms", "5"]);
    let out = temp_file("report.json");
    let output = loadgen(&[
        "run",
        "--url",
        &mock.url,
        "--arrival",
        "constant",
        "--rate",
        "10",
        "--duration",
        "1s",
        "--warmup",
        "0",
        "--max-tokens",
        "4",
        "--json",
        out.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains("open loop, constant 10.0 req/s"),
        "{stdout}"
    );
    assert!(
        stdout.contains("requests    offered 10  ok 10  failed 0  dropped 0"),
        "{stdout}"
    );
    assert!(stdout.contains("ttft") && stdout.contains("itl") && stdout.contains("e2e"));

    let report = json::parse(&std::fs::read_to_string(&out).unwrap()).unwrap();
    let _ = std::fs::remove_file(out);
    assert_eq!(number(&report, &["ok"]), 10.0);
    assert_eq!(number(&report, &["target_rate"]), 10.0);
    assert!(number(&report, &["ttft_ms", "p50"]) >= 20.0);
    assert_eq!(number(&report, &["e2e_ms", "count"]), 10.0);
}

#[test]
fn a_stalled_server_is_visible_in_open_loop_and_hidden_in_closed_loop() {
    // requests take about 10 ms, but the server freezes for a second, 0.3 s after it started
    let options = [
        "--ttft-ms",
        "10",
        "--itl-ms",
        "0",
        "--stall",
        "0.3:1",
        "--slots",
        "64",
    ];
    let open = run_json(
        &start_mock(&options),
        "open.json",
        &[
            "--rate",
            "50",
            "--arrival",
            "constant",
            "--duration",
            "2",
            "--connections",
            "200",
        ],
    );
    let closed = run_json(
        &start_mock(&options),
        "closed.json",
        &["--mode", "closed", "--duration", "2", "--connections", "4"],
    );
    let (open_p90, closed_p90) = (
        number(&open, &["e2e_ms", "p90"]),
        number(&closed, &["e2e_ms", "p90"]),
    );
    assert!(open_p90 > 500.0, "open loop p90 {open_p90} ms");
    assert!(closed_p90 < 250.0, "closed loop p90 {closed_p90} ms");
    assert!(
        number(&open, &["e2e_ms", "max"]) > 900.0,
        "someone waited for the whole freeze"
    );
}

#[test]
fn errors_from_the_server_are_counted_not_fatal() {
    let mock = start_mock(&["--error-status", "503"]);
    let report = run_json(
        &mock,
        "errors.json",
        &["--rate", "10", "--arrival", "constant", "--duration", "1"],
    );
    assert_eq!(number(&report, &["ok"]), 0.0);
    assert_eq!(number(&report, &["failed"]), 10.0);
    assert_eq!(number(&report, &["errors", "status 503"]), 10.0);
}

#[test]
fn a_mistake_gives_a_message_and_exit_code_2() {
    let output = loadgen(&["run"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("--url is required"));
    assert!(text(&output.stderr).contains("--help"));
    let output = loadgen(&["run", "--url", "https://example.com/"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("no TLS"));
    let output = loadgen(&["frobnicate"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn bad_settings_that_only_the_runner_can_see_exit_with_2_before_sending_anything() {
    let mock = start_mock(&[]);
    let output = loadgen(&["run", "--url", &mock.url, "--rate", "0"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("positive"));
}

#[test]
fn help_and_version_succeed() {
    let output = loadgen(&["--help"]);
    assert!(output.status.success());
    assert!(text(&output.stdout).contains("USAGE"));
    assert!(text(&output.stdout).contains("--stall AFTER:LEN"));
    let output = loadgen(&["--version"]);
    assert!(text(&output.stdout).starts_with("llm-loadgen "));
}

#[test]
fn an_unwritable_json_path_is_an_error_after_the_report_is_printed() {
    let mock = start_mock(&[]);
    let dir = std::env::temp_dir();
    let output = loadgen(&[
        "run",
        "--url",
        &mock.url,
        "--rate",
        "5",
        "--arrival",
        "constant",
        "--duration",
        "1",
        "--warmup",
        "0",
        "--json",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stdout).contains("requests    offered 5"));
    assert!(text(&output.stderr).contains("cannot write"));
}

#[test]
fn a_dead_endpoint_is_reported_as_connection_failures() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
    let output = loadgen(&[
        "run",
        "--url",
        &url,
        "--rate",
        "5",
        "--arrival",
        "constant",
        "--duration",
        "1",
        "--warmup",
        "0",
    ]);
    assert!(output.status.success());
    let stdout = text(&output.stdout);
    assert!(stdout.contains("ok 0  failed 5"), "{stdout}");
    assert!(stdout.contains("failed: connect"), "{stdout}");
    assert!(stdout.contains("note: latency percentiles cover successful requests only."));
}
