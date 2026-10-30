use std::io::{self, IsTerminal as _};
use std::path::Path;

use tracing::level_filters::LevelFilter;
use tracing::{span, Level};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::{fmt, EnvFilter};

const ENV_NAME_COMMON: &str = "TORTURE_ALL_LOG";
const ENV_NAME_AGENT: &str = "TORTURE_AGENT_LOG";
const ENV_NAME_SUPERVISOR: &str = "TORTURE_SUPERVISOR_LOG";

enum Kind {
    Agent,
    Supervisor,
}

fn istty() -> bool {
    io::stdout().is_terminal() && io::stderr().is_terminal()
}

/// Creates env filter for the agent or supervisor (depending on the `agent_not_supervisor`
/// argument).
///
/// This function tries to read the most specific environment variable first, then falls back to
/// the common one ([`ENV_NAME_COMMON`]).
fn env_filter(kind: Kind) -> EnvFilter {
    let specific_env_name = match kind {
        Kind::Agent => ENV_NAME_AGENT,
        Kind::Supervisor => ENV_NAME_SUPERVISOR,
    };

    return try_parse_env(specific_env_name).unwrap_or_else(|| {
        try_parse_env(ENV_NAME_COMMON).unwrap_or_else(|| {
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .parse("")
                .unwrap()
        })
    });

    fn try_parse_env(var_name: &str) -> Option<EnvFilter> {
        match std::env::var(var_name) {
            Ok(env) => Some(
                EnvFilter::builder()
                    .with_default_directive(LevelFilter::INFO.into())
                    .parse(env)
                    .unwrap(),
            ),
            Err(std::env::VarError::NotPresent) => {
                return None;
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                panic!("Environment variable {} is not unicode", var_name);
            }
        }
    }
}

fn create_subscriber<W>(kind: Kind, writer: W, ansi: bool) -> impl tracing::Subscriber
where
    W: for<'writer> MakeWriter<'writer> + 'static + Sync + Send,
{
    let format = fmt::format()
        .with_level(true)
        .with_target(false)
        .with_thread_ids(false)
        .with_thread_names(false)
        .compact()
        .with_timer(fmt::time::SystemTime::default());

    fmt::Subscriber::builder()
        .with_env_filter(env_filter(kind))
        .with_writer(writer)
        .with_ansi(ansi)
        .event_format(format)
        .finish()
}

pub fn init_supervisor() {
    let subscriber = create_subscriber(Kind::Supervisor, io::stdout, istty());
    tracing::subscriber::set_global_default(subscriber)
        .expect("Failed to set supervisor subscriber");
}

pub fn workload_subscriber(workload_dir: &impl AsRef<Path>) -> impl tracing::Subscriber {
    let log_file = std::fs::File::options()
        .create(true)
        .append(true)
        .open(workload_dir.as_ref().join("log.txt"))
        .expect("Failed to create log file");
    create_subscriber(Kind::Supervisor, log_file, false)
}

pub fn init_agent(agent_id: &str, workload_dir: &impl AsRef<Path>) {
    let log_file = std::fs::File::options()
        .create(false)
        .append(true)
        .open(workload_dir.as_ref().join("log.txt"))
        .expect("Log file is expected to be created by the supervisor");
    let subscriber = create_subscriber(Kind::Agent, log_file, false);

    // Set the agent global subscriber
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set agent subscriber");

    let pid = std::process::id();
    let span = span!(Level::INFO, "agent", agent_id, pid);
    let _enter = span.enter();
    // We intentionally `forget` the guard so the span remains open
    // for the lifetime of the entire agent process if desired.
    std::mem::forget(_enter);
}
