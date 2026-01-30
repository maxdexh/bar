use std::sync::{LazyLock, OnceLock};

use anyhow::Context as _;

fn parse_env<T: std::str::FromStr<Err: std::error::Error + Send + Sync + 'static>>(
    name: &str,
) -> anyhow::Result<T> {
    let val = std::env::var(name).with_context(|| format!("Failed to get env var {name:?}"))?;
    val.parse()
        .with_context(|| format!("Failed to parse from env var {name:?}. Value: {val:?} "))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProcKind {
    Controller,
    Panel,
}

pub const LOG_NAME_ENV_VAR: &str = "BAR_PROC_LOG_NAME";

const COLOR_VAR: &str = "COLOR";

static PROC_NAME: OnceLock<String> = OnceLock::new();

fn format_log(
    w: &mut dyn std::io::Write,
    now: &mut flexi_logger::DeferredNow,
    record: &log::Record,
) -> Result<(), std::io::Error> {
    struct Format {
        color: bool,
        pid: u32,
        proc_name: String,
    }
    static FORMAT: LazyLock<Format> = LazyLock::new(|| Format {
        pid: std::process::id(),
        // FIXME: This is only OK for the controller. The panels should instead use the settings
        // of the controller.
        color: match std::env::var(COLOR_VAR).as_deref().unwrap_or("auto") {
            "never" | "no" | "off" | "false" => false,
            "always" | "yes" | "on" | "true" => true,
            _ => std::io::IsTerminal::is_terminal(&std::io::stderr()),
        },
        proc_name: PROC_NAME
            .get()
            .map(|s| &**s)
            .unwrap_or("UNKNOWN")
            .to_owned(),
    });
    let Format {
        color,
        pid,
        ref proc_name,
    } = *FORMAT;

    let line_display = record.line();
    let line_display = if let Some(line) = &line_display {
        format_args!("{}", *line)
    } else {
        format_args!("?")
    };

    let now_display = now.format("%Y-%m-%d %H:%M:%S");
    let now_display = if color {
        format_args!("\x1b[35m{now_display}\x1b[0m")
    } else {
        format_args!("{now_display}")
    };

    let level = record.level();

    let level_colored;
    let level_display = if color {
        level_colored = flexi_logger::style(level).paint(level.to_string());
        format_args!("{level_colored}")
    } else {
        format_args!("{level}")
    };

    write!(
        w,
        "[{now_display}] {proc_name} ({pid}) {level_display} [{}:{line_display}] {}",
        record.file().unwrap_or("<unknown>"),
        record.args(),
    )
}

pub fn init_logger(proc_kind: ProcKind, log_name: String) {
    _ = PROC_NAME.set(log_name);
    match try_init_logger(proc_kind) {
        Ok(_) => log::info!("Started logger"),
        Err(err) => {
            let err = err.context(format!(
                "Failed to start logger for {:?} (pid {})",
                &proc_kind,
                std::process::id()
            ));
            eprintln!("{err:?}");
        }
    }
}

// FIXME: Move most of this function to controller and mgr respectively?
// I.e. only keep the formatter around in common
fn try_init_logger(proc_kind: ProcKind) -> anyhow::Result<()> {
    static PROC_KIND: OnceLock<ProcKind> = OnceLock::new();
    let proc_kind = PROC_KIND.get_or_init(|| proc_kind);

    use flexi_logger::*;

    let log_spec: LogSpecification = if cfg!(debug_assertions) {
        flexi_logger::LevelFilter::Debug.into()
    } else {
        flexi_logger::LevelFilter::Info.into()
    };

    let logger = Logger::with(log_spec).o_append(true).format(format_log);
    let logger = match proc_kind {
        ProcKind::Controller => logger.log_to_stderr(),
        ProcKind::Panel => match parse_env::<std::os::fd::RawFd>("KITTY_STDIO_FORWARDED") {
            Ok(fd) => logger.log_to_file(FileSpec::try_from(format!("/proc/self/fd/{fd}"))?),
            Err(_) => logger.log_to_stderr(),
        },
    };
    std::mem::forget(logger.start()?);

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("{info}");
        hook(info);
    }));

    Ok(())
}
