use std::sync::OnceLock;

pub const COLOR_VAR: &str = "COLOR";
pub fn should_color() -> bool {
    COLOR.get().is_some_and(|it| *it)
}

#[derive(Debug)]
pub enum ProcKindForLogger {
    Controller,
    Panel(crate::terminals::TermId),
}
impl std::fmt::Display for ProcKindForLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Controller => write!(f, "CONTROLLER"),
            Self::Panel(id) => write!(f, "{id:?}"),
        }
    }
}

static COLOR: OnceLock<bool> = OnceLock::new();
static PROC_NAME: OnceLock<String> = OnceLock::new();

pub fn init_logger(proc_kind: ProcKindForLogger) {
    let doit = || -> anyhow::Result<()> {
        use flexi_logger::*;

        PROC_NAME
            .set(proc_kind.to_string())
            .map_err(|_| anyhow::anyhow!("Already set"))?;

        fn format(
            w: &mut dyn std::io::Write,
            now: &mut DeferredNow,
            record: &Record,
        ) -> Result<(), std::io::Error> {
            let color = should_color();

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
                level_colored = style(level).paint(level.to_string());
                format_args!("{level_colored}")
            } else {
                format_args!("{level}")
            };

            write!(
                w,
                "[{now_display}] {} {level_display} [{}:{line_display}] {}",
                PROC_NAME.get().unwrap(),
                record.file().unwrap_or("<unknown>"),
                record.args(),
            )
        }

        let log_spec: LogSpecification = if cfg!(debug_assertions) {
            flexi_logger::LevelFilter::Debug.into()
        } else {
            flexi_logger::LevelFilter::Info.into()
        };

        let logger = Logger::with(log_spec).o_append(true).format(format);
        let logger = {
            match std::env::var("KITTY_STDIO_FORWARDED")
                .map_err(anyhow::Error::from)
                .and_then(|fd| fd.parse::<std::os::fd::RawFd>().map_err(Into::into))
            {
                Ok(fd) => logger.log_to_file(FileSpec::try_from(format!("/proc/self/fd/{fd}"))?),
                Err(_) => logger.log_to_stderr(),
            }
        };
        std::mem::forget(logger.start()?);

        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            log::error!("{info}");
            hook(info);
        }));

        let color = std::env::var(COLOR_VAR);
        let color = match color.as_deref().unwrap_or("auto") {
            "never" | "no" | "off" | "false" => false,
            "always" | "yes" | "on" | "true" => true,
            _ => std::io::IsTerminal::is_terminal(&std::io::stderr()),
        };
        _ = COLOR.set(color);

        Ok(())
    };
    match doit() {
        Ok(_) => log::info!("Started logger for {proc_kind:?}"),
        Err(err) => eprintln!("Failed to start logger: {err}."),
    }
}
