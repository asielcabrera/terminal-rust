//! The `terminal` binary: subcommand dispatch. The bare invocation is the window
//! (terminal-client), `server` is the headless server process (terminal-server), `gateway`
//! fronts the local server for remote clients, and the automation CLI subcommands are
//! short-lived clients of a running server.

use terminal_client::app;

fn main() {
    // 0. The subcommands: `terminal server` runs the headless server process, `terminal
    //    gateway` fronts it for remote clients over the tailnet, and the automation CLI
    //    subcommands are short-lived clients of a running server (stage 6); the bare
    //    binary is the window
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("server") => {
            terminal_server::run_server_process();
            unreachable!("the server process serves until it is killed");
        }
        Some("gateway") => {
            // The allow-list lives in the configuration (empty denies everyone)
            let loaded = terminal_client::config::load();
            if let Some(error) = &loaded.error {
                eprintln!("[config] {error}");
            }
            // Same endpoint resolution as the window: connect, or start the server detached
            // and retry
            let endpoint = terminal_protocol::ipc::IpcEndpoint::resolve();
            app::ensure_server_running(&endpoint);
            terminal_server::run_gateway_process(
                loaded.config.remote.allow,
                app::gateway_port(&args),
            );
            unreachable!("the gateway serves until it is killed");
        }
        Some("ls") => {
            app::cli_ls();
        }
        Some("send") => {
            app::cli_send(&args[2..]);
        }
        Some("capture") => {
            app::cli_capture(&args[2..]);
        }
        Some("attach") => {
            app::cli_attach(&args[2..]);
        }
        // Reachable for future subcommands only; today the binary is the window, the
        // server, the gateway and the automation CLI
        Some(other) => {
            eprintln!(
                "unknown subcommand {other:?}; usage: terminal [server|gateway|ls|send|capture|attach]"
            );
            std::process::exit(2);
        }
        None => app::run_window(),
    }
}
