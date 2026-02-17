use crate::cache::{hash_files, Cache};
use crate::tracking;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

pub fn run(args: &[String], verbose: u8, use_cache: bool) -> Result<()> {
    let cmd_name = args.first().map(|s| s.as_str()).unwrap_or("");
    let mut trigger_hash = None;
    let mut cache_key = None;

    if use_cache {
        let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
        cache_key = Some(Cache::generate_key(&cwd, "mix", args));

        // Define triggers based on subcommand
        if cmd_name == "phx.routes" {
            let mut triggers = vec![
                PathBuf::from("mix.lock"),
                PathBuf::from("lib/core_platform_web/router.ex"),
            ];
            // Also try a more generic path if that one doesn't exist
            if !triggers[1].exists() {
                // Try to find any router.ex
                if let Ok(paths) = std::fs::read_dir("lib") {
                    for entry in paths.flatten() {
                        let path = entry.path();
                        if path.is_dir() {
                            let router = path.join("router.ex");
                            if router.exists() {
                                triggers.push(router);
                            }
                        }
                    }
                }
            }
            trigger_hash = Some(hash_files(&triggers)?);
        }

        if let Some(key) = &cache_key {
            if let Ok(cache) = Cache::new() {
                if let Ok(Some(cached_output)) = cache.get(key, trigger_hash.as_deref()) {
                    if verbose > 0 {
                        eprintln!("rtk: cache hit for mix {}", args.join(" "));
                    }
                    println!("{}", cached_output);
                    return Ok(());
                }
            }
        }
    }

    let timer = tracking::TimedExecution::start();

    let mut cmd = Command::new("mix");
    cmd.args(args);

    let output = cmd.output().context("Failed to execute mix")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        // Many mix commands (credo, dialyzer, etc.) write their report to stdout
        // even on non-zero exit codes, so we must print both streams.
        let filtered = filter_mix_output(&stdout, args, verbose);
        if !filtered.trim().is_empty() {
            println!("{}", filtered);
        }
        if !stderr.trim().is_empty() {
            eprintln!("{}", stderr);
        }
        // Track before exiting so failed commands still appear in rtk gain
        timer.track(
            &format!("mix {}", args.join(" ")),
            &format!("rtk mix {}", args.join(" ")),
            &stdout,
            &filtered,
        );
        std::process::exit(output.status.code().unwrap_or(1));
    }

    let filtered = filter_mix_output(&stdout, args, verbose);
    println!("{}", filtered);

    if let (Some(key), Some(cache)) = (cache_key, Cache::new().ok()) {
        let _ = cache.set(&key, &filtered, trigger_hash.as_deref());
    }

    timer.track(
        &format!("mix {}", args.join(" ")),
        &format!("rtk mix {}", args.join(" ")),
        &stdout,
        &filtered,
    );

    Ok(())
}

fn filter_mix_output(stdout: &str, args: &[String], verbose: u8) -> String {
    if verbose >= 3 {
        return stdout.to_string();
    }

    let cmd = args.first().map(|s| s.as_str()).unwrap_or("");

    // Check for compound command names (e.g., "ash.codegen", "ash_postgres.generate_migrations")
    let is_codegen = cmd == "ash.codegen" || cmd == "ash_postgres.generate_migrations";
    let is_credo = cmd == "credo";
    let is_compile = cmd == "compile";

    match cmd {
        "phx.routes" => filter_routes(stdout),
        "help" => filter_help(stdout),
        _ if is_codegen => filter_codegen(stdout, args),
        _ if is_credo => filter_credo(stdout, verbose),
        _ if is_compile => filter_compile(stdout),
        _ => stdout.to_string(),
    }
}

/// Filter ash.codegen output — strip JSON snapshots, keep migration SQL and file paths.
fn filter_codegen(stdout: &str, args: &[String]) -> String {
    let is_check = args.iter().any(|a| a == "--check");
    let is_dry_run = args.iter().any(|a| a == "--dry-run");

    // For --check, just show the summary
    if is_check {
        let mut result = Vec::new();
        for line in stdout.lines() {
            let trimmed = line.trim();
            // Keep "Running codegen for ..." lines (1 line each)
            // Keep "Pending Code Generation" or similar error lines
            if trimmed.starts_with("Running codegen for")
                || trimmed.starts_with("Getting extensions")
                || trimmed.contains("Pending Code Generation")
                || trimmed.contains("Code generation is up to date")
                || trimmed.starts_with("Compiling")
                || trimmed.starts_with("Generated")
            {
                result.push(trimmed.to_string());
            }
        }
        if result.is_empty() {
            return "ash.codegen --check: ok".to_string();
        }
        return result.join("\n");
    }

    // For --dry-run or regular codegen, keep file paths and migration SQL,
    // strip the JSON resource snapshots entirely.
    let mut result = Vec::new();
    let mut in_json = false;
    let mut json_file: Option<String> = None;
    let mut files_created = Vec::new();

    for line in stdout.lines() {
        let trimmed = line.trim();

        // Detect start of a JSON snapshot block (path ending in .json followed by {)
        if trimmed.ends_with(".json") && !in_json {
            json_file = Some(trimmed.to_string());
            continue;
        }

        if json_file.is_some() && trimmed == "{" {
            in_json = true;
            continue;
        }

        if in_json {
            if trimmed == "}" {
                // End of JSON block — emit a summary line instead
                if let Some(ref path) = json_file {
                    result.push(format!("  snapshot: {} (contents stripped)", path));
                }
                in_json = false;
                json_file = None;
            }
            // Skip all JSON content
            continue;
        }

        // Track created files
        if trimmed.starts_with("* creating") {
            files_created.push(trimmed.to_string());
            result.push(trimmed.to_string());
            continue;
        }

        // Keep status lines, migration content, and file paths
        if trimmed.starts_with("Running codegen for")
            || trimmed.starts_with("Getting extensions")
            || trimmed.starts_with("Compiling")
            || trimmed.starts_with("Generated")
            || trimmed.ends_with(".exs")
            || trimmed.starts_with("defmodule")
            || trimmed.starts_with("use ")
            || trimmed.starts_with("def up")
            || trimmed.starts_with("def down")
            || trimmed.starts_with("alter ")
            || trimmed.starts_with("create ")
            || trimmed.starts_with("add ")
            || trimmed.starts_with("remove ")
            || trimmed.starts_with("end")
            || trimmed.starts_with("modify ")
            || trimmed.starts_with("rename ")
            || trimmed.starts_with("drop ")
            || trimmed.is_empty()
        {
            result.push(line.to_string());
        }
    }

    // Trim consecutive blank lines
    let mut final_result = Vec::new();
    let mut prev_blank = false;
    for line in &result {
        if line.trim().is_empty() {
            if !prev_blank {
                final_result.push(line.clone());
            }
            prev_blank = true;
        } else {
            final_result.push(line.clone());
            prev_blank = false;
        }
    }

    final_result.join("\n")
}

/// Filter credo output — show summary + only warnings/errors, skip design/readability suggestions.
fn filter_credo(stdout: &str, verbose: u8) -> String {
    // At verbose >= 2, show everything
    if verbose >= 2 {
        return stdout.to_string();
    }

    let lines: Vec<&str> = stdout.lines().collect();
    let mut result = Vec::new();
    let mut skipped_count: usize = 0;
    let mut in_low_priority_block = false;

    // Credo uses priority arrows:
    //   ↑ = high, ↗ = medium-high, → = normal  (actionable — keep these)
    //   ↘ = low, ↓ = very low                   (suggestions — skip these)
    // We only keep lines with ↑ ↗ → arrows, plus non-issue lines (headers, footers).

    for line in &lines {
        let trimmed = line.trim();

        // Issue lines live inside ┃ blocks
        if trimmed.starts_with('┃') {
            // Check if this line starts a new issue (has a severity marker)
            let is_new_issue = trimmed.contains("[D]")
                || trimmed.contains("[R]")
                || trimmed.contains("[C]")
                || trimmed.contains("[W]")
                || trimmed.contains("[F]");

            if is_new_issue {
                // Keep issues with high-priority arrows, skip low-priority ones
                if trimmed.contains('↑') || trimmed.contains('↗') || trimmed.contains('→') {
                    in_low_priority_block = false;
                    result.push(line.to_string());
                } else {
                    // ↘ or ↓ — skip this issue and its continuation lines
                    in_low_priority_block = true;
                    skipped_count += 1;
                }
            } else {
                // Continuation line (file path, details) — follows the previous issue's fate
                if !in_low_priority_block {
                    result.push(line.to_string());
                }
            }
            continue;
        }

        // Non-┃ lines: headers, footers, blank lines, category names
        in_low_priority_block = false;
        result.push(line.to_string());
    }

    // Append a note about skipped low-priority issues
    if skipped_count > 0 {
        result.push(format!(
            "\n({} low-priority suggestions hidden, use -vv to see all)",
            skipped_count
        ));
    }

    result.join("\n")
}

/// Filter compile output — keep only errors, warnings, and summary.
fn filter_compile(stdout: &str) -> String {
    let lines: Vec<&str> = stdout.lines().collect();

    // If output is short (< 10 lines), pass through as-is
    if lines.len() < 10 {
        return stdout.to_string();
    }

    let mut result = Vec::new();
    let mut in_warning = false;
    let mut in_error = false;

    for line in &lines {
        let trimmed = line.trim();

        // Always keep these status lines
        if trimmed.starts_with("Compiling")
            || trimmed.starts_with("Generated")
            || trimmed.starts_with("==>")
        {
            result.push(line.to_string());
            in_warning = false;
            in_error = false;
            continue;
        }

        // Detect warning/error blocks
        if trimmed.starts_with("warning:") {
            in_warning = true;
            in_error = false;
            result.push(line.to_string());
            continue;
        }

        if trimmed.starts_with("error:") || trimmed.starts_with("** (") {
            in_error = true;
            in_warning = false;
            result.push(line.to_string());
            continue;
        }

        // Continuation of warning/error block (indented or file reference)
        if (in_warning || in_error)
            && (trimmed.starts_with('│')
                || trimmed.starts_with('└')
                || trimmed.starts_with("~")
                || trimmed.contains(".ex:")
                || trimmed.contains(".exs:")
                || trimmed.is_empty())
        {
            result.push(line.to_string());
            if trimmed.is_empty() {
                in_warning = false;
                in_error = false;
            }
            continue;
        }

        // Reset if we hit a non-continuation line
        if in_warning || in_error {
            in_warning = false;
            in_error = false;
        }

        // Skip "Compiling N files" noise lines for dependencies
        // and other informational output
    }

    if result.is_empty() {
        return "compile: ok".to_string();
    }

    result.join("\n")
}

fn filter_routes(stdout: &str) -> String {
    let lines: Vec<&str> = stdout.lines().collect();
    if lines.len() <= 10 {
        return stdout.to_string();
    }

    let mut result = Vec::new();
    result.push(lines[0].to_string()); // Header

    // Phoenix routes output usually looks like:
    //   page_path  GET  /  PageController :index

    for line in lines.iter().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            // Keep it simple but compact
            result.push(line.trim().to_string());
        }
    }

    let count = result.len();
    if count > 100 {
        result.truncate(50);
        result.push("...".to_string());
        result.push(format!("({} total routes)", count));
    }

    result.join("\n")
}

fn filter_help(stdout: &str) -> String {
    // Only keep the first paragraph of mix help
    let mut result = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() && !result.is_empty() {
            break;
        }
        result.push(line.to_string());
    }
    result.join("\n")
}
