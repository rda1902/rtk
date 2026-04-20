//! Elixir mix test output filter.
//!
//! Parses ExUnit text output using a state machine: extracts failures/errors,
//! compacts stacktraces (strips deps/ paths), and shows a one-line summary.
//! Falls back to last N lines when output format is unrecognized.

use crate::core::runner;
use crate::core::utils::{fallback_tail, strip_ansi, truncate};
use anyhow::Result;
use lazy_static::lazy_static;
use regex::Regex;
use std::process::Command;

lazy_static! {
    static ref RE_SUMMARY: Regex =
        Regex::new(r"(\d+)\s+tests?,\s*(\d+)\s+failures?").unwrap();
    static ref RE_FINISHED_IN: Regex = Regex::new(r"^Finished in \d").unwrap();
    static ref RE_RANDOM_SEED: Regex = Regex::new(r"^Randomized with seed \d+").unwrap();
    static ref RE_FAILURE_HEADER: Regex = Regex::new(r"^\s*\d+\)\s").unwrap();
    static ref RE_LOG_NOISE: Regex = Regex::new(r"^\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}").unwrap();
    static ref RE_LOG_TIME_ONLY: Regex = Regex::new(r"^\d{2}:\d{2}:\d{2}\.\d+\s+\[").unwrap();
}

fn is_failure_header(line: &str) -> bool {
    RE_FAILURE_HEADER.is_match(line)
}

fn is_log_noise(line: &str) -> bool {
    let t = line.trim();
    if RE_LOG_NOISE.is_match(t) {
        return true;
    }
    if t.starts_with('.') && t.contains(" pid=<") {
        return true;
    }
    if RE_LOG_TIME_ONLY.is_match(t) {
        return true;
    }
    t.starts_with("Running ExUnit")
}

fn is_test_progress(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.chars().all(|c| matches!(c, '.' | 'F' | 'E' | '*' | 'S'))
}

fn is_failure_noise(line: &str) -> bool {
    let t = line.trim();
    is_log_noise(t) || is_test_progress(t) || t.starts_with("The following output was logged:")
}

fn is_deps_backtrace(line: &str) -> bool {
    let t = line.trim();
    if t.contains("/deps/") || t.contains("lib/ex_unit/") {
        return true;
    }
    if let Some(rest) = t.strip_prefix('(') {
        if let Some(end) = rest.find(')') {
            let app = &rest[..end];
            return matches!(
                app,
                "elixir"
                    | "eex"
                    | "mix"
                    | "logger"
                    | "iex"
                    | "ex_unit"
                    | "kernel"
                    | "stdlib"
                    | "compiler"
            );
        }
    }
    false
}

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = Command::new("mix");
    cmd.arg("test");
    cmd.args(args);

    if verbose > 0 {
        eprintln!("Running: mix test {}", args.join(" "));
    }

    runner::run_filtered(
        cmd,
        "mix-test",
        &args.join(" "),
        filter_mix_test_output,
        runner::RunOptions::stdout_only().tee("mix-test"),
    )
}

#[derive(PartialEq)]
enum ParseState {
    Header,
    Failures,
    Summary,
}

fn filter_mix_test_output(output: &str) -> String {
    let clean = strip_ansi(output);

    if clean.trim().is_empty() {
        return "mix test: No output".to_string();
    }

    let mut state = ParseState::Header;
    let mut failures: Vec<String> = Vec::new();
    let mut current_failure: Vec<String> = Vec::new();
    let mut summary_line = String::new();

    for line in clean.lines() {
        let trimmed = line.trim();

        if RE_RANDOM_SEED.is_match(trimmed) {
            continue;
        }

        if RE_SUMMARY.captures(trimmed).is_some() {
            summary_line = trimmed.to_string();
            state = ParseState::Summary;
            continue;
        }

        match state {
            ParseState::Header => {
                if RE_FINISHED_IN.is_match(trimmed) {
                    state = ParseState::Failures;
                    continue;
                }
                if is_failure_header(trimmed) {
                    state = ParseState::Failures;
                    if !current_failure.is_empty() {
                        failures.push(current_failure.join("\n"));
                        current_failure.clear();
                    }
                    current_failure.push(trimmed.to_string());
                    continue;
                }
            }
            ParseState::Failures => {
                if is_failure_header(trimmed) {
                    if !current_failure.is_empty() {
                        failures.push(current_failure.join("\n"));
                        current_failure.clear();
                    }
                    current_failure.push(trimmed.to_string());
                } else if trimmed.is_empty() && !current_failure.is_empty() {
                    failures.push(current_failure.join("\n"));
                    current_failure.clear();
                } else if !trimmed.is_empty()
                    && !is_deps_backtrace(trimmed)
                    && !is_failure_noise(trimmed)
                {
                    current_failure.push(line.to_string());
                }
            }
            ParseState::Summary => {
                break;
            }
        }
    }

    if !current_failure.is_empty() && state == ParseState::Failures {
        failures.push(current_failure.join("\n"));
    }

    build_mix_test_summary(&summary_line, &failures, &clean)
}

fn parse_summary(summary: &str) -> (usize, usize) {
    if let Some(caps) = RE_SUMMARY.captures(summary) {
        let tests: usize = caps
            .get(1)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let fail_count: usize = caps
            .get(2)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        (tests, fail_count)
    } else {
        (0, 0)
    }
}

fn extract_duration(output: &str) -> Option<String> {
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Finished in ") {
            let secs = rest.split_whitespace().next().unwrap_or("");
            if let Ok(secs_f) = secs.parse::<f64>() {
                return Some(format!("{:.2}s", secs_f));
            }
            return Some(secs.to_string());
        }
    }
    None
}

fn build_mix_test_summary(summary_line: &str, failures: &[String], clean: &str) -> String {
    let (tests, fail_count) = parse_summary(summary_line);

    if tests == 0 && summary_line.is_empty() {
        return "mix test: No output".to_string();
    }

    let duration = extract_duration(clean);

    if fail_count == 0 && !summary_line.is_empty() {
        let mut result = format!("✓ mix test: {} tests, 0 failures", tests);
        if let Some(d) = duration {
            result.push_str(&format!(" ({})", d));
        }
        return result;
    }

    if summary_line.is_empty() {
        return fallback_tail(clean, "mix-test", 5);
    }

    let mut result = format!("mix test: {} tests, {} failures", tests, fail_count);
    if let Some(d) = duration {
        result.push_str(&format!(" ({})", d));
    }
    result.push('\n');
    result.push_str("═══════════════════════════════════════\n");

    if failures.is_empty() {
        let tail = fallback_tail(clean, "mix-test", 10);
        return format!("{}\n{}", result.trim(), tail);
    }

    result.push('\n');
    result.push_str("Failures:\n");

    for (i, failure) in failures.iter().take(5).enumerate() {
        let compacted = compact_failure(failure);
        result.push_str(&format!("{}. ❌ {}\n", i + 1, compacted));
        if i < 4 {
            result.push('\n');
        }
    }

    if failures.len() > 5 {
        result.push_str(&format!("\n... +{} more failures\n", failures.len() - 5));
    }

    result.trim().to_string()
}

fn compact_failure(block: &str) -> String {
    let mut lines: Vec<&str> = block.lines().collect();
    lines.retain(|l| !l.trim().is_empty());

    if lines.is_empty() {
        return String::new();
    }

    let mut result_lines: Vec<String> = Vec::new();
    let mut file_line = String::new();

    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();

        if i == 0 {
            result_lines.push(t.to_string());
            continue;
        }

        if t.starts_with("stacktrace:") {
            continue;
        }

        if is_failure_noise(t) {
            continue;
        }

        if t.starts_with("test/") || t.starts_with("lib/") || t.contains("_test.exs") {
            file_line = t.to_string();
            continue;
        }

        if result_lines.len() < 4 {
            result_lines.push(truncate(t, 120));
        }
    }

    if !file_line.is_empty() {
        result_lines.push(file_line);
    }

    result_lines.join("\n   ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::utils::count_tokens;

    #[test]
    fn test_filter_mix_test_all_pass() {
        let output = "\
......

Finished in 0.05 seconds
2 doctests, 6 tests, 0 failures

Randomized with seed 584732
";

        let result = filter_mix_test_output(output);
        assert!(result.starts_with("✓ mix test:"));
        assert!(result.contains("6 tests"));
        assert!(result.contains("0 failures"));
    }

    #[test]
    fn test_filter_mix_test_with_failures() {
        let output = "\
..F...

  1) failure: test greets the world (MyApp.GreeterTest)
     test/my_app/greeter_test.exs:5
     Assertion with == failed
     code:  assert Greeter.greet() == \"hello\"
     left:  \"hi\"
     right: \"hello\"
     stacktrace:
       test/my_app/greeter_test.exs:6: (test)

Finished in 0.08 seconds
2 doctests, 6 tests, 1 failures
";

        let result = filter_mix_test_output(output);
        assert!(result.contains("1 failures"));
        assert!(result.contains("❌"));
        assert!(result.contains("greeter_test.exs"));
        assert!(result.contains("test greets the world"));
    }

    #[test]
    fn test_filter_mix_test_with_error() {
        let output = "\
.E....

  1) error: test something (MyApp.UserTest)
     test/user_test.exs:10
     ** (RuntimeError) something went wrong
     stacktrace:
       test/user_test.exs:11: (test)

Finished in 0.12 seconds
6 tests, 1 failures
";

        let result = filter_mix_test_output(output);
        assert!(result.contains("1 failures"));
        assert!(result.contains("❌"));
        assert!(result.contains("RuntimeError"));
    }

    #[test]
    fn test_filter_mix_test_empty() {
        let result = filter_mix_test_output("");
        assert_eq!(result, "mix test: No output");
    }

    #[test]
    fn test_filter_mix_test_multiple_failures() {
        let output = "\
.FF...

  1) failure: test alpha (MyApp.AlphaTest)
     test/alpha_test.exs:5
     Assertion with == failed
     code:  assert 1 == 2
     stacktrace:
       test/alpha_test.exs:6: (test)

  2) failure: test beta (MyApp.BetaTest)
     test/beta_test.exs:10
     Assertion with == failed
     code:  assert \"a\" == \"b\"
     stacktrace:
       test/beta_test.exs:11: (test)

Finished in 0.15 seconds
6 tests, 2 failures

Randomized with seed 123456
";

        let result = filter_mix_test_output(output);
        assert!(result.contains("2 failures"));
        assert!(result.contains("test alpha"));
        assert!(result.contains("test beta"));
    }

    #[test]
    fn test_filter_mix_test_many_failures_caps_at_five() {
        let mut output = String::from("FFFFFF\n\n");
        for i in 1..=7u8 {
            output.push_str(&format!(
                "  {i}) failure: test {i} (MyApp.Test{i})\n\
test/test_{i}.exs:{line}\n\
Assertion failed\n\
stacktrace:\n\
test/test_{i}.exs:{next}: (test)\n\n",
                i = i,
                line = i as usize * 5,
                next = i as usize * 5 + 1
            ));
        }
        output.push_str("Finished in 0.3 seconds\n7 tests, 7 failures\n");

        let result = filter_mix_test_output(&output);
        assert!(result.contains("1. ❌"), "should show first failure");
        assert!(result.contains("5. ❌"), "should show fifth failure");
        assert!(!result.contains("6. ❌"), "should not show sixth inline");
        assert!(result.contains("+2 more"), "should show overflow: {}", result);
    }

    #[test]
    fn test_filter_mix_test_ansi() {
        let output = "\x1b[32m......\x1b[0m\n\n\
             \x1b[32mFinished in 0.05 seconds\x1b[0m\n\
             6 tests, 0 failures\n\n\
             Randomized with seed 123\n";
        let result = filter_mix_test_output(output);
        assert!(result.contains("✓ mix test:"));
        assert!(result.contains("6 tests"));
    }

    #[test]
    fn test_filter_mix_test_strips_deps_backtrace() {
        let output = "\
F

  1) failure: test something (MyApp.Test)
     test/my_test.exs:5
     Assertion failed
     stacktrace:
       (elixir) lib/enum.ex:123: Enum.map/2
       (ecto) /deps/ecto/lib/ecto/repo.ex:45: Ecto.Repo.all/2
       (my_app) lib/my_app/worker.ex:42: MyApp.Worker.run/1
       test/my_test.exs:6: (test)

Finished in 0.1 seconds
1 tests, 1 failures
";

        let result = filter_mix_test_output(output);
        assert!(result.contains("my_test.exs"));
        assert!(result.contains("my_app/worker"), "user project stacktrace should be kept");
        assert!(!result.contains("deps/ecto"));
        assert!(!result.contains("lib/enum.ex"));
    }

    #[test]
    fn test_filter_mix_test_no_summary_fallback() {
        let output = "some unrecognized output\nmore lines";
        let result = filter_mix_test_output(output);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_parse_summary() {
        assert_eq!(parse_summary("6 tests, 0 failures"), (6, 0));
        assert_eq!(parse_summary("10 tests, 3 failures"), (10, 3));
        assert_eq!(parse_summary("1 test, 1 failure"), (1, 1));
        assert_eq!(parse_summary(""), (0, 0));
    }

    #[test]
    fn test_token_savings_all_pass() {
        let mut dots = String::new();
        for _ in 0..20 {
            dots.push_str("......................................................................\n");
        }
        let output = format!(
            "{}\nFinished in 2.345 seconds\n500 tests, 0 failures\n\nRandomized with seed 12345\n",
            dots
        );

        let input_tokens = count_tokens(&output);
        let result = filter_mix_test_output(&output);
        let output_tokens = count_tokens(&result);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 60.0,
            "mix test all-pass: expected >=60% savings, got {:.1}% (in={}, out={})",
            savings,
            input_tokens,
            output_tokens
        );
    }

    #[test]
    fn test_token_savings_with_failures() {
        let mut output = String::from("F.F..F....F..F..F\n\n");
        for i in 1..=6u8 {
            output.push_str(&format!(
                "  {i}) failure: test feature_{i} works correctly (MyApp.Feature{i}Test)\n\
test/feature_{i}_test.exs:{line}\n\
Assertion with == failed\n\
code:  assert Feature_{i}.run(%{{a: 1, b: 2}}) == {{:ok, result_{i}}}\n\
left:  {{:error, :not_found}}\n\
right: {{:ok, %{{value: {i}}}}}\n\
stacktrace:\n\
test/feature_{i}_test.exs:{ln}: (test)\n\n",
                i = i,
                line = i as usize * 10,
                ln = i as usize * 10 + 1
            ));
        }
        output.push_str(
            "Compiling 15 files (.ex)\n\
             Generated my_app app\n\
             \n\
             Finished in 0.891 seconds\n\
             20 tests, 6 failures\n",
        );

        let input_tokens = count_tokens(&output);
        let result = filter_mix_test_output(&output);
        let output_tokens = count_tokens(&result);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 25.0,
            "mix test failures: expected >=25% savings, got {:.1}% (in={}, out={})",
            savings,
            input_tokens,
            output_tokens
        );
    }

    #[test]
    fn test_extract_duration() {
        assert_eq!(
            extract_duration("Finished in 0.05 seconds"),
            Some("0.05s".to_string())
        );
        assert_eq!(
            extract_duration("Finished in 2.345 seconds"),
            Some("2.35s".to_string())
        );
        assert_eq!(extract_duration("no duration here"), None);
    }

    #[test]
    fn test_filter_mix_test_with_excluded() {
        let output = "\
......
Finished in 0.05 seconds
6 tests, 0 failures, 2 excluded

Randomized with seed 584732
";

        let result = filter_mix_test_output(output);
        assert!(result.contains("✓ mix test:"));
        assert!(result.contains("0 failures"));
    }

    #[test]
    fn test_filter_real_exunit_format() {
        let output = "\
.2026-04-20 20:48:27.471 pid=<0.503.0> [info] [Elixir.Transports.Widget] calling webhook
.2026-04-20 20:48:27.481 pid=<0.503.0> [info] [Elixir.Transports.Widget] outbox stuff
.

  1) test prepare_request/2 attachment .txt (Transports.WidgetTest)
     test/gateway/core/transports/widget_test.exs:165
     Assertion with == failed
     code:  assert attachments.original_file_name == \"document.txt1\"
     left:  \"document.txt\"
     right: \"document.txt1\"
     stacktrace:
       test/gateway/core/transports/widget_test.exs:193: (test)

2026-04-20 20:48:27.742 pid=<0.530.0> [info] [Elixir.Transports.Widget] calling webhook
.2026-04-20 20:48:27.748 pid=<0.541.0> [info] [Elixir.Transports.Widget] calling read
.

  2) test send_read_status/2 handles successful read status (Transports.WidgetTest)
     test/gateway/core/transports/widget_test.exs:132
     Assertion with == failed
     code:  assert result.status == 400
     left:  200
     right: 400
     stacktrace:
       test/gateway/core/transports/widget_test.exs:147: (test)

     The following output was logged:
     
     20:48:27.874 [info] [Elixir.Transports.Widget] calling livechat read
     20:48:27.874 [info] [Elixir.Transports.Widget] params %{\"key\" => \"val\"}
     
2026-04-20 20:48:27.988 pid=<0.557.0> [info] [Elixir.Transports.Widget] hook params JSON
.2026-04-20 20:48:27.994 pid=<0.567.0> [info] [Elixir.Transports.Widget] calling read
.
Finished in 1.0 seconds (0.00s async, 1.0s sync)
9 tests, 2 failures
";

        let result = filter_mix_test_output(output);
        assert!(result.contains("2 failures"), "should contain failure count: {}", result);
        assert!(result.contains("❌"), "should have failure markers: {}", result);
        assert!(
            result.contains("test prepare_request"),
            "should show first test name: {}",
            result
        );
        assert!(
            result.contains("test send_read_status"),
            "should show second test name: {}",
            result
        );
        assert!(
            result.contains("widget_test.exs"),
            "should show file location: {}",
            result
        );
        assert!(
            result.contains("Assertion with == failed"),
            "should show error type: {}",
            result
        );
        assert!(
            !result.contains("pid=<"),
            "should strip Logger noise: {}",
            result
        );
        assert!(
            !result.contains("calling webhook"),
            "should strip log messages: {}",
            result
        );
    }

    #[test]
    fn test_filter_exunit_format_no_prefix() {
        let output = "\
..F...

  1) test greets the world (MyApp.GreeterTest)
     test/my_app/greeter_test.exs:5
     Assertion with == failed
     code:  assert Greeter.greet() == \"hello\"
     left:  \"hi\"
     right: \"hello\"
     stacktrace:
       test/my_app/greeter_test.exs:6: (test)

Finished in 0.08 seconds
2 doctests, 6 tests, 1 failures
";

        let result = filter_mix_test_output(output);
        assert!(result.contains("1 failures"));
        assert!(result.contains("❌"));
        assert!(result.contains("greeter_test.exs"));
        assert!(result.contains("test greets the world"));
    }
}
