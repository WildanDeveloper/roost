//! CLI subcommands — port of wings `cmd/configure.go` + `cmd/diagnostics.go`.
//!
//! Non-interactive by design: the interactive `survey` prompts of wings are
//! replaced by explicit flags so the commands work in scripts and CI.

use std::path::PathBuf;

use crate::config::Config;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const DEFAULT_CONFIG_PATH: &str = "/etc/pterodactyl/config.yml";

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("--version") | Some("version") => {
            println!("roost v{VERSION}");
            0
        }
        Some("configure") => configure(&args[1..]),
        Some("diagnostics") => diagnostics(&args[1..]),
        Some(other) => {
            eprintln!(
                "unknown command `{other}`\n\nusage:\n  roost                    run the daemon\n  roost configure [flags]\n  roost diagnostics [flags]\n  roost version"
            );
            1
        }
        None => 0,
    }
}

struct Flags {
    values: std::collections::HashMap<&'static str, String>,
    switches: std::collections::HashSet<&'static str>,
}

fn parse_flags(args: &[String], with_values: &[&'static str], switches: &[&'static str]) -> Result<Flags, String> {
    let mut flags = Flags { values: Default::default(), switches: Default::default() };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let name = a.trim_start_matches("--");
        let alias = match name {
            "p" => "panel-url",
            "t" => "token",
            "n" => "node",
            "c" => "config-path",
            other => other,
        };
        if let Some(key) = with_values.iter().find(|k| **k == alias) {
            i += 1;
            let value = args.get(i).ok_or_else(|| format!("flag --{alias} requires a value"))?;
            flags.values.insert(key, value.clone());
        } else if let Some(key) = switches.iter().find(|k| **k == alias) {
            flags.switches.insert(key);
        } else {
            return Err(format!("unknown flag --{alias}"));
        }
        i += 1;
    }
    Ok(flags)
}

/// `roost configure --panel-url URL --token TOKEN --node ID [--config-path PATH]
/// [--override] [--allow-insecure]`
///
/// Fetches the node configuration from the panel application API and writes
/// it as a drop-in `config.yml` (wings configureCmdRun).
fn configure(args: &[String]) -> i32 {
    let flags = match parse_flags(
        args,
        &["panel-url", "token", "node", "config-path"],
        &["override", "allow-insecure"],
    ) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("configure: {e}\n\nusage: roost configure --panel-url URL --token TOKEN --node ID [--config-path PATH] [--override] [--allow-insecure]");
            return 1;
        }
    };
    let panel_url = match flags.values.get("panel-url") {
        Some(u) => u.trim_end_matches('/').to_string(),
        None => {
            eprintln!("configure: --panel-url is required");
            return 1;
        }
    };
    let token = match flags.values.get("token") {
        Some(t) if !t.is_empty() => t.clone(),
        _ => {
            eprintln!("configure: --token is required");
            return 1;
        }
    };
    let node = match flags.values.get("node") {
        Some(n) if n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty() => n.clone(),
        _ => {
            eprintln!("configure: --node must be the numeric ID of the node");
            return 1;
        }
    };
    let config_path = flags
        .values
        .get("config-path")
        .cloned()
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string());
    if PathBuf::from(&config_path).exists() && !flags.switches.contains("override") {
        eprintln!(
            "Aborting process; a configuration file already exists for this node (use --override to replace it)."
        );
        return 1;
    }

    let url = format!("{panel_url}/api/application/nodes/{node}/configuration");
    let fetch = async {
        let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30));
        if flags.switches.contains("allow-insecure") {
            builder = builder.danger_accept_invalid_certs(true);
        }
        let client = builder
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {e}"))?;
        let resp = client
            .get(&url)
            .header("Accept", "application/vnd.pterodactyl.v1+json")
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| format!("Failed to fetch configuration from the panel.\n{e}"))?;
        if resp.status() == reqwest::StatusCode::FORBIDDEN
            || resp.status() == reqwest::StatusCode::UNAUTHORIZED
        {
            return Err("The authentication credentials provided were not valid.".to_string());
        } else if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("An error occurred while processing this request.\nHTTP {status}\n{body}"));
        }
        resp.json::<serde_json::Value>()
            .await
            .map_err(|e| format!("Failed to decode the panel response: {e}"))
    };
    let json: serde_json::Value = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start runtime: {e}"))
        .and_then(|rt| rt.block_on(fetch))
    {
        Ok(j) => j,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    // Merge the panel configuration over the built-in defaults (wings
    // config.NewAtPath then json.Unmarshal), then force the panel URL since
    // it is not part of the decoded payload.
    let merged = match merge_defaults(&json) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to merge configuration: {e}");
            return 1;
        }
    };
    let mut cfg: Config = match serde_yaml::from_value(merged) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Panel returned an unexpected configuration: {e}");
            return 1;
        }
    };
    cfg.remote = panel_url.clone();
    cfg.path = Some(config_path.clone());
    cfg.resolve_token();

    let yaml = match serde_yaml::to_string(&cfg) {
        Ok(y) => y,
        Err(e) => {
            eprintln!("Failed to serialize configuration: {e}");
            return 1;
        }
    };
    if let Some(parent) = PathBuf::from(&config_path).parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("Failed to create {}: {e}", parent.display());
            return 1;
        }
    }
    if let Err(e) = std::fs::write(&config_path, yaml) {
        eprintln!("Failed to write {config_path}: {e}");
        return 1;
    }
    println!("Successfully configured roost.");
    0
}

/// Merge the panel JSON over the default configuration example (defaults win
/// only where the panel does not specify a value).
fn merge_defaults(json: &serde_json::Value) -> Result<serde_yaml::Value, String> {
    let defaults: serde_yaml::Value = serde_yaml::from_str(Config::DEFAULTS)
        .map_err(|e| format!("cannot parse built-in defaults: {e}"))?;
    let panel: serde_yaml::Value =
        serde_yaml::to_value(json).map_err(|e| format!("cannot convert panel payload: {e}"))?;
    Ok(merge_values(defaults, panel))
}

fn merge_values(base: serde_yaml::Value, overlay: serde_yaml::Value) -> serde_yaml::Value {
    match (base, overlay) {
        (serde_yaml::Value::Mapping(mut m), serde_yaml::Value::Mapping(o)) => {
            for (k, v) in o {
                let merged = match m.get(&k) {
                    Some(existing) => merge_values(existing.clone(), v),
                    None => v,
                };
                m.insert(k, merged);
            }
            serde_yaml::Value::Mapping(m)
        }
        (_, overlay) => overlay,
    }
}

/// `roost diagnostics [--config-path PATH] [--log-lines N] [--output FILE]
/// [--include-endpoints] [--no-logs]`
///
/// Collects a debugging report (wings diagnosticsCmdRun): versions,
/// sanitized configuration, docker info, managed containers and the latest
/// daemon logs. Never includes tokens.
fn diagnostics(args: &[String]) -> i32 {
    let flags = match parse_flags(
        args,
        &["config-path", "log-lines", "output"],
        &["include-endpoints", "no-logs"],
    ) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("diagnostics: {e}\n\nusage: roost diagnostics [--config-path PATH] [--log-lines N] [--output FILE] [--include-endpoints] [--no-logs]");
            return 1;
        }
    };
    let config_path = flags
        .values
        .get("config-path")
        .cloned()
        .unwrap_or_else(|| {
            std::env::var("ROOST_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string())
        });
    let log_lines: usize = flags
        .values
        .get("log-lines")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let include_endpoints = flags.switches.contains("include-endpoints");
    let include_logs = !flags.switches.contains("no-logs");

    let config = Config::load(&config_path);
    let output = collect_diagnostics(&config_path, config.as_ref().ok(), log_lines, include_endpoints, include_logs);

    match flags.values.get("output") {
        Some(path) => {
            if let Err(e) = std::fs::write(path, &output) {
                eprintln!("Failed to write {path}: {e}");
                return 1;
            }
            println!("Diagnostics report written to {path}");
        }
        None => {
            println!("---------------  generated report  ---------------");
            println!("{output}");
            println!("---------------   end of report    ---------------");
        }
    }
    0
}

fn redact_endpoint(value: &str, include_endpoints: bool) -> String {
    if include_endpoints {
        value.to_string()
    } else {
        "{redacted}".to_string()
    }
}

fn collect_diagnostics(
    config_path: &str,
    config: Option<&Config>,
    log_lines: usize,
    include_endpoints: bool,
    include_logs: bool,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    writeln!(out, "Pterodactyl Roost - Diagnostics Report").unwrap();

    writeln!(out, "== Versions ==").unwrap();
    writeln!(out, "  Roost: v{VERSION}").unwrap();
    let docker = crate::docker::DockerClient::connect();
    match &docker {
        Ok(d) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(rt) = rt {
                match rt.block_on(async {
                    let v = d.engine_version().await?;
                    let i = d.engine_info().await?;
                    Ok::<_, crate::error::AppError>((v, i))
                }) {
                    Ok((version, info)) => {
                        writeln!(out, "  Docker: {}", version.version.unwrap_or_default()).unwrap();
                        writeln!(out, "== Docker: Info ==").unwrap();
                        writeln!(out, "  Server Version: {}", info.server_version.unwrap_or_default()).unwrap();
                        writeln!(out, "  Storage Driver: {}", info.driver.unwrap_or_default()).unwrap();
                        writeln!(out, "  Cgroup Driver: {}", info.cgroup_driver.map(|d| format!("{d:?}")).unwrap_or_else(|| "unknown".into())).unwrap();
                        if let Some(warnings) = &info.warnings {
                            for w in warnings {
                                writeln!(out, "  {w}").unwrap();
                            }
                        }
                        writeln!(out, "== Docker: Containers ==").unwrap();
                        match rt.block_on(d.list_managed_containers()) {
                            Ok(containers) if containers.is_empty() => {
                                writeln!(out, "  (none)").unwrap();
                            }
                            Ok(containers) => {
                                for c in containers {
                                    writeln!(out, "  {c}").unwrap();
                                }
                            }
                            Err(e) => writeln!(out, "  Couldn't list containers: {e}").unwrap(),
                        }
                    }
                    Err(e) => {
                        writeln!(out, "  Docker: {e}").unwrap();
                    }
                }
            }
        }
        Err(e) => {
            writeln!(out, "  Docker: {e}").unwrap();
        }
    }
    writeln!(out, "  OS: {} {}", std::env::consts::OS, std::env::consts::ARCH).unwrap();

    writeln!(out, "== Configuration ({config_path}) ==").unwrap();
    if let Some(cfg) = config {
        writeln!(out, "  Panel Location: {}", redact_endpoint(&cfg.remote, include_endpoints)).unwrap();
        writeln!(out, "  Internal Webserver: {} : {}", redact_endpoint(&cfg.api.host, include_endpoints), cfg.api.port).unwrap();
        writeln!(out, "  SSL Enabled: {}", cfg.api.ssl.enabled).unwrap();
        writeln!(out, "  SFTP Server: {} : {}", redact_endpoint(&cfg.system.sftp.bind_address, include_endpoints), cfg.system.sftp.bind_port).unwrap();
        writeln!(out, "  SFTP Read-Only: {}", cfg.system.sftp.read_only).unwrap();
        writeln!(out, "  Root Directory: {}", cfg.system.root_directory).unwrap();
        writeln!(out, "  Logs Directory: {}", cfg.system.log_directory).unwrap();
        writeln!(out, "  Data Directory: {}", cfg.system.data).unwrap();
        writeln!(out, "  Archive Directory: {}", cfg.system.archive_directory).unwrap();
        writeln!(out, "  Backup Directory: {}", cfg.system.backup_directory).unwrap();
        writeln!(out, "  Debug Mode: {}", cfg.debug).unwrap();
        // Server states (wings includes a server list in diagnostics).
        let states_path = cfg.states_path();
        if let Ok(states) = std::fs::read_to_string(&states_path) {
            writeln!(out, "== Server States ({}) ==", states_path.display()).unwrap();
            writeln!(out, "  {}", states).unwrap();
        }
        // Logs, sanitized of anything token-looking.
        writeln!(out, "== Latest Roost Logs ==").unwrap();
        if include_logs {
            let log_path = cfg.log_dir().join("roost.log");
            match std::fs::read_to_string(&log_path) {
                Ok(content) => {
                    let tail: Vec<&str> = content.lines().collect();
                    let start = tail.len().saturating_sub(log_lines);
                    for line in &tail[start..] {
                        writeln!(out, "  {line}").unwrap();
                    }
                }
                Err(_) => writeln!(out, "  No logs found or an error occurred.").unwrap(),
            }
        } else {
            writeln!(out, "  Logs redacted.").unwrap();
        }
    } else {
        writeln!(out, "  (config could not be loaded; tokens are never included in this report)").unwrap();
    }
    out
}
