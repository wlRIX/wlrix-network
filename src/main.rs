// SPDX-License-Identifier: GPL-3.0-or-later
//! wlRIX network status: NetworkManager's Wi-Fi state, as an icon in `wlrix-tray`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut rest = args.iter();
    if let Some(argument) = rest.next() {
        match argument.as_str() {
            "--help" | "-h" => {
                println!(
                    "wlrix-network {}\n\n\
                     Network status for the wlRIX desktop: NetworkManager's Wi-Fi state as\n\
                     a StatusNotifierItem, drawn by wlrix-tray. Started by wlrix-session;\n\
                     needs NetworkManager on the system bus and a session D-Bus.\n\n\
                     Usage: wlrix-network [options]\n\n\
                     Options:\n  \
                       --status               read NetworkManager once, say what the icon\n                         \
                                              would show, exit\n  \
                       --check-config <path>  say whether that file would be accepted, exit\n  \
                       -h, --help             this message\n  \
                       -V, --version          print the version\n\n\
                     Settings live in ~/.config/wlrix/network.toml, or /etc/wlrix/network.toml.",
                    env!("CARGO_PKG_VERSION")
                );
                return;
            }
            "--version" | "-V" => {
                println!("wlrix-network {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--check-config" => {
                let Some(path) = rest.next() else {
                    eprintln!("wlrix-network: --check-config needs a path");
                    std::process::exit(2);
                };
                if let Err(why) = wlrix_network::config::check(std::path::Path::new(path)) {
                    eprintln!("{why}");
                    std::process::exit(1);
                }
                return;
            }
            "--status" => {
                if let Err(err) = wlrix_network::daemon::status() {
                    eprintln!("wlrix-network: {err}");
                    std::process::exit(1);
                }
                return;
            }
            other => {
                eprintln!("wlrix-network: unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    if let Err(err) = wlrix_network::daemon::run() {
        eprintln!("wlrix-network: {err}");
        std::process::exit(1);
    }
}
