//! `memoryos reranker`: shows and changes the reranker settings in `server.env`, and benches the
//! models on this machine.

use clap::Subcommand;
use memoryos_embed::{bench, RerankModel};

use crate::config::{RerankerMode, RerankerSettings, DEFAULT_BIND};
use crate::settings::{self, Lookup};

const MODE: &str = "MEMORYOS_RERANKER";
const MODEL: &str = "MEMORYOS_RERANK_MODEL";
const TOP: &str = "MEMORYOS_RERANK_TOP";

#[derive(Debug, Subcommand)]
pub enum RerankerCommand {
    /// Show the reranker settings and the model the running server uses.
    Status,
    /// List the models `use` accepts.
    List,
    /// Rerank with a model from `list`, or `off`, then restart the local server.
    Use {
        model: String,
        /// Only save the setting.
        #[arg(long)]
        no_restart: bool,
    },
    /// Set how many fused facts one recall reranks, then restart the local server.
    Top {
        #[arg(value_parser = clap::value_parser!(u16).range(1..))]
        count: u16,
        /// Only save the setting.
        #[arg(long)]
        no_restart: bool,
    },
    /// Score the 15 bench questions with each named model, or every model. Downloads models that
    /// are not cached yet, and loads one at a time.
    Bench { models: Vec<String> },
}

pub fn run(command: RerankerCommand) -> Result<(), String> {
    match command {
        RerankerCommand::Status => status(),
        RerankerCommand::List => list(),
        RerankerCommand::Use { model, no_restart } => {
            let updates = if model.trim().eq_ignore_ascii_case("off") {
                vec![(MODE, "off".to_owned())]
            } else {
                let model: RerankModel = model.parse()?;
                vec![(MODE, "local".to_owned()), (MODEL, model.name().to_owned())]
            };
            save(&updates, no_restart)
        }
        RerankerCommand::Top { count, no_restart } => save(&[(TOP, count.to_string())], no_restart),
        RerankerCommand::Bench { models } => bench_models(&models),
    }
}

fn settings_path() -> Result<std::path::PathBuf, String> {
    settings::path().ok_or_else(|| "HOME is not set, so there is no server.env".to_owned())
}

fn status() -> Result<(), String> {
    let path = settings_path()?;
    let lookup = Lookup::load()?;
    let current = RerankerSettings::from_lookup(&lookup)?;
    println!("settings  {}", path.display());
    match current.mode {
        RerankerMode::Off => println!("reranker  off"),
        RerankerMode::Local => {
            println!("reranker  local");
            println!(
                "model     {} ({} MB, {})",
                current.model,
                current.model.size_mb(),
                current.model.languages()
            );
            println!("top       {}", current.top);
        }
    }
    let bind = lookup
        .get("MEMORYOS_BIND")
        .unwrap_or_else(|| DEFAULT_BIND.to_owned());
    println!("server    {}", server_line(&bind, current));
    Ok(())
}

/// What `/readyz` says, and whether the server runs other settings than the file's.
fn server_line(bind: &str, current: RerankerSettings) -> String {
    let Ok(address) = bind.trim().parse() else {
        return format!("MEMORYOS_BIND {bind:?} is not an address like 127.0.0.1:8080");
    };
    let url = crate::restart::ready_url(address);
    let Some(body) = fetch(&url) else {
        return format!("not reachable at {url}");
    };
    let Ok(ready) = serde_json::from_str::<serde_json::Value>(&body) else {
        return format!("{url} did not answer with JSON");
    };
    let state = ready["reranker"].as_str().unwrap_or("unknown");
    let Some(running) = ready.get("reranker_model") else {
        return format!(
            "reranker {state}, model not reported by this older server; restart: memoryos restart"
        );
    };
    let running = running.as_str();
    let wanted = match current.mode {
        RerankerMode::Off => None,
        RerankerMode::Local => Some(current.model.name()),
    };
    let model = running.unwrap_or("no model");
    if running != wanted {
        format!("{model}, {state}; restart to apply the settings: memoryos restart")
    } else {
        format!("{model}, {state}")
    }
}

/// The body of a GET, or `None` when nothing answered. A 503 still has the readiness body.
fn fetch(url: &str) -> Option<String> {
    let output = std::process::Command::new("curl")
        .args(["-s", "--max-time", "2", url])
        .output()
        .ok()?;
    let body = String::from_utf8_lossy(&output.stdout).into_owned();
    (output.status.success() && !body.is_empty()).then_some(body)
}

fn list() -> Result<(), String> {
    let current = RerankerSettings::from_lookup(&Lookup::load()?)?;
    for model in RerankModel::ALL {
        let mark = if current.mode == RerankerMode::Local && model == current.model {
            "  current"
        } else {
            ""
        };
        println!(
            "{:<27}{:>4} MB  {:<14}{mark}",
            model.name(),
            model.size_mb(),
            model.languages()
        );
    }
    let mark = if current.mode == RerankerMode::Off {
        "  current"
    } else {
        ""
    };
    println!("{:<27}{:>7}  {:<14}{mark}", "off", "", "");
    Ok(())
}

fn save(updates: &[(&str, String)], no_restart: bool) -> Result<(), String> {
    let path = settings_path()?;
    settings::write(&path, updates)?;
    println!("saved to {}", path.display());
    for (name, value) in updates {
        if let Some(set) = std::env::var(name).ok().filter(|v| !v.trim().is_empty()) {
            if set.trim() != value {
                eprintln!(
                    "warning: {name}={set} is set in this shell and wins over server.env; unset it \
                     before restarting"
                );
            }
        }
    }
    if no_restart {
        println!("restart the server to apply: memoryos restart");
        return Ok(());
    }
    crate::restart::run()
}

fn bench_models(names: &[String]) -> Result<(), String> {
    let models = if names.is_empty() {
        RerankModel::ALL.to_vec()
    } else {
        names
            .iter()
            .map(|name| name.parse())
            .collect::<Result<Vec<RerankModel>, String>>()?
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the runtime: {e}"))?;
    println!(
        "{:<27}{:>8}{:>10}{:>9}{:>9}",
        "model", "passed", "median", "max", "load"
    );
    for model in models {
        eprintln!(
            "loading {model} ({} MB, downloaded on first use)",
            model.size_mb()
        );
        // `bench::run` drops its model before it returns, so one model is in memory at a time.
        let report = runtime
            .block_on(bench::run(model, None))
            .map_err(|e| format!("{model}: {e}"))?;
        println!(
            "{:<27}{:>8}{:>7.0} ms{:>6.0} ms{:>7.1} s",
            model.name(),
            format!("{}/{}", report.passed, report.cases),
            report.median_ms,
            report.max_ms,
            report.load_secs
        );
        for (query, top) in &report.misses {
            println!("    miss: {query} -> {top}");
        }
    }
    Ok(())
}
