use anyhow::{Result, anyhow};

use crate::logging::{ProcKindForLogger, init_logger};

pub mod bar_panel;
pub mod controller;
pub mod menu_panel;

const INTERNAL_BAR_ARG: &str = "bar";
const INTERNAL_MENU_ARG: &str = "menu";
const EDGE: &str = "top";

// FIXME: We need one of each panel for every monitor...
// See https://sw.kovidgoyal.net/kitty/kittens/panel/#cmdoption-kitty-kitten-panel-output-name
// For this, we should probably move the bootstrap logic into the controller
pub async fn entry_point() -> Result<()> {
    let (mk_bar, controller_connect_bar) = crate::utils::bootstrap_panel();
    let (mk_menu, controller_connect_menu) = crate::utils::bootstrap_panel();

    let mut args = std::env::args().skip(1);

    // declare this here to make sure we don't accidentally drop this too early.
    // this is not set in every branch.
    let menu_temp_dir_guard;

    match args.next().as_deref() {
        Some("internal") => {
            let mode = args.next().ok_or_else(|| anyhow!("Missing mode arg"))?;
            let bootstrap_path = args
                .next()
                .ok_or_else(|| anyhow!("Missing bootstrap arg"))?;

            match mode.as_str() {
                INTERNAL_BAR_ARG => {
                    init_logger(ProcKindForLogger::Bar);
                    let (ctrl_tx, ctrl_rx) = controller_connect_bar(&bootstrap_path).await?;
                    bar_panel::main(ctrl_tx, ctrl_rx).await
                }
                INTERNAL_MENU_ARG => {
                    init_logger(ProcKindForLogger::Menu);
                    let (ctrl_tx, ctrl_rx) = controller_connect_menu(&bootstrap_path).await?;
                    menu_panel::main(ctrl_tx, ctrl_rx).await
                }
                _ => Err(anyhow!("Bad arguments")),
            }
        }
        None => {
            init_logger(ProcKindForLogger::Controller);

            // TODO: Move the spawn commands to the respective modules
            let bar_task = mk_bar(async |path| {
                Ok(tokio::process::Command::new("kitten")
                    .stdout(std::io::stderr())
                    .args([
                        "panel",
                        // Allow logging to $KITTY_STDIO_FORWARDED
                        "-o=forward_stdio=yes",
                        // Do not use the system's kitty.conf
                        "--config=NONE",
                        // Basic look of the bar
                        "-o=foreground=white",
                        "-o=background=black",
                        // location of the bar
                        &format!("--edge={EDGE}"),
                        // disable hiding the mouse
                        "-o=mouse_hide_wait=0",
                    ])
                    .arg(&std::env::current_exe()?)
                    .args(["internal", INTERNAL_BAR_ARG])
                    .arg(path)
                    .kill_on_drop(true)
                    .spawn()?)
            });

            menu_temp_dir_guard = tempfile::tempdir()?;
            let menu_temp_path = menu_temp_dir_guard
                .path()
                .to_str()
                .ok_or_else(|| anyhow!("tempdir path must be valid utf8"))?
                .to_owned();
            log::info!("Using menu tempdir {menu_temp_path}");

            let menu_task = mk_menu(async move |path| {
                let watcher_py = format!("{menu_temp_path}/menu_watcher.py");
                tokio::fs::write(&watcher_py, include_bytes!("menu_panel/menu_watcher.py")).await?;

                let watcher_sock_path = format!("{menu_temp_path}/menu_watcher.sock");
                let watcher_listener = tokio::net::UnixListener::bind(&watcher_sock_path)?;

                let child = tokio::process::Command::new("kitten")
                    .stdout(std::io::stderr())
                    .env("BAR_MENU_WATCHER_SOCK", watcher_sock_path)
                    .args([
                        "panel",
                        // Configure remote control via socket
                        "-o=allow_remote_control=socket-only",
                        &format!("--listen-on=unix:{menu_temp_path}/menu-panel.sock"),
                        // Allow logging to $KITTY_STDIO_FORWARDED
                        "-o=forward_stdio=yes",
                        // Do not use the system's kitty.conf
                        "--config=NONE",
                        // Basic look of the menu
                        "-o=background_opacity=0.85",
                        "-o=background=black",
                        "-o=foreground=white",
                        // location of the menu
                        "--edge=top",
                        // disable hiding the mouse
                        "-o=mouse_hide_wait=0",
                        // Window behavior of the menu panel. Makes panel
                        // act as an overlay on top of other windows.
                        // We do not want tilers to dedicate space to it.
                        // Taken from the args that quick-access-terminal uses.
                        "--exclusive-zone=0",
                        "--override-exclusive-zone",
                        "--layer=overlay",
                        // Focus behavior of the panel. Since we cannot tell from
                        // mouse events alone when the cursor leaves the panel
                        // (since terminal mouse capture only gives us mouse
                        // events inside the panel), we need external support for
                        // hiding it automatically. We use a watcher to be able
                        // to reset the menu state when this happens.
                        "--focus-policy=on-demand",
                        "--hide-on-focus-loss",
                        &format!("-o=watcher={watcher_py}"),
                        // Since we control resizes from the program and not from
                        // a somewhat continuous drag-resize, debouncing between
                        // resize and reloads is completely inappropriate and
                        // just results in a larger delay between resize and
                        // the old menu content being replaced with the new one.
                        "-o=resize_debounce_time=0 0",
                        // TODO: Mess with repaint_delay, input_delay
                    ])
                    .arg(&std::env::current_exe()?)
                    .args(["internal", INTERNAL_MENU_ARG])
                    .arg(path)
                    .kill_on_drop(true)
                    .spawn()?;

                Ok((child, watcher_listener.accept().await?.0))
            });

            let (bar_res, menu_res) = tokio::join!(bar_task, menu_task);
            let ((bar, bar_tx, bar_rx), ((menu, menu_watcher_stream), menu_tx, menu_rx)) =
                (bar_res?, menu_res?);

            controller::main(
                bar_tx,
                bar_rx,
                menu_tx,
                menu_rx,
                menu_panel::MenuWatcherEvents::listen_from(menu_watcher_stream),
                bar,
                menu,
            )
            .await
        }
        _ => Err(anyhow!("Bad arguments")),
    }
}
