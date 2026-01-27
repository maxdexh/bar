use std::sync::LazyLock;

use anyhow::Context as _;

fn parse_env<T: std::str::FromStr<Err: std::error::Error + Send + Sync + 'static>>(
    name: &str,
) -> anyhow::Result<T> {
    let val = std::env::var(name).with_context(|| format!("Failed to get env var {name:?}"))?;
    val.parse()
        .with_context(|| format!("Failed to parse from env var {name:?}. Value: {val:?} "))
}

#[derive(Debug, Clone)]
pub enum ProcKind {
    Controller,
    Panel,
}

pub fn proc_kind_from_args() -> &'static ProcKind {
    static PROC_KIND: LazyLock<ProcKind> =
        LazyLock::new(|| match std::env::args().nth(1).as_deref() {
            Some(crate::panels::proc::PANEL_PROC_ARG) => ProcKind::Panel,
            _ => ProcKind::Controller,
        });
    &PROC_KIND
}

const COLOR_VAR: &str = "COLOR";

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
    static PROC_NAME: LazyLock<anyhow::Result<String>> =
        LazyLock::new(|| match proc_kind_from_args() {
            ProcKind::Controller => Ok("CONTROLLER".into()),
            ProcKind::Panel => parse_env::<String>(crate::panels::proc::PROC_LOG_NAME_VAR),
        });
    static FORMAT: LazyLock<Format> = LazyLock::new(|| Format {
        pid: std::process::id(),
        color: {
            let color = std::env::var(COLOR_VAR);
            match color.as_deref().unwrap_or("auto") {
                "never" | "no" | "off" | "false" => false,
                "always" | "yes" | "on" | "true" => true,
                _ => std::io::IsTerminal::is_terminal(&std::io::stderr()),
            }
        },
        proc_name: PROC_NAME.as_deref().unwrap_or("UNKNOWN").to_owned(),
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

pub fn init_logger() {
    match try_init_logger() {
        Ok(_) => log::info!("Started logger"),
        Err(err) => {
            let err = err.context(format!(
                "Failed to start logger for {:?} (pid {})",
                proc_kind_from_args(),
                std::process::id()
            ));
            eprintln!("{err:?}");
        }
    }
}

fn try_init_logger() -> anyhow::Result<()> {
    use flexi_logger::*;

    let log_spec: LogSpecification = if cfg!(debug_assertions) {
        flexi_logger::LevelFilter::Debug.into()
    } else {
        flexi_logger::LevelFilter::Info.into()
    };

    let logger = Logger::with(log_spec).o_append(true).format(format_log);
    let logger = match proc_kind_from_args() {
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
