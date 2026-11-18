use g502_hub::{ipc, service, updater};
use serde_json::json;

fn print_help() {
    println!(
        "g502-hubd {} — Background HID++ and lighting service for Logitech G502 HERO\n\n\
         Usage:\n  \
         g502-hubd                 Run the background service\n  \
         g502-hubd --status        Print the running service status as JSON\n  \
         g502-hubd --stop          Stop the running service\n  \
         g502-hubd --check-update  Check GitHub Releases for a newer version\n  \
         g502-hubd -u, --update    Download, verify, and install the latest release\n  \
         g502-hubd --update-demo   Preview the terminal updater animation\n  \
         g502-hubd --version       Print version information\n  \
         g502-hubd --help          Show this help message",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("g502-hubd {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let result = match args.as_slice() {
        [] => service::run(),
        [flag] if flag == "--status" => ipc::request("snapshot", json!({}))
            .map(|v| println!("{}", serde_json::to_string_pretty(&v).unwrap())),
        [flag] if flag == "--stop" => ipc::request("stop", json!({})).map(|_| ()),
        [flag] if flag == "--check-update" => updater::run_cli_check(),
        [flag] if flag == "--update" || flag == "-u" => updater::run_cli_update(),
        [flag] if flag == "--update-demo" => {
            updater::run_cli_demo();
            Ok(())
        }
        [other, ..] => Err(anyhow::anyhow!(
            "Unknown option '{other}'. Run 'g502-hubd --help' for usage."
        )),
    };
    if let Err(e) = result {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}
