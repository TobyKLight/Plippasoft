//! warp_bench: WARP_02 eval app for the Plippa CM4. See README.md in this directory.
//!
//! Standalone and self-contained: it depends on no production Plippa crate and no production crate
//! depends on it. Its voices are reference implementations, not the production DSP.

mod args;
mod cmds;
mod cpu;
mod decode;
mod gen;
mod info;
mod latency;
mod listen;
mod profiles;
mod report;
mod seek;
mod sink;
mod stats;
mod suggest;
mod usbread;
mod voices;

use anyhow::{bail, Result};
use args::Args;

fn listen_cmd(argv: &[String]) -> Result<()> {
    let Some(which) = argv.first() else {
        bail!("usage: warp_bench listen speed|scan|reverse|jump|xfade [file] [options] (see warp_bench --help)");
    };
    let rest = &argv[1..];
    let (opts, help): (Vec<&'static str>, &str) = match which.as_str() {
        "speed" => (listen::speed_opts(), listen::SPEED_HELP),
        "scan" => (listen::scan_opts(), listen::SCAN_HELP),
        "reverse" => (listen::reverse_opts(), listen::REVERSE_HELP),
        "jump" => (listen::jump_opts(), listen::JUMP_HELP),
        "xfade" => (listen::xfade_opts(), listen::XFADE_HELP),
        other => bail!("unknown listening test '{other}' (speed|scan|reverse|jump|xfade)"),
    };
    let a = Args::parse(rest, &opts, listen::COMMON_FLAGS)?;
    if a.help() {
        print!("{help}");
        return Ok(());
    }
    match which.as_str() {
        "speed" => listen::run_speed(&a),
        "scan" => listen::run_scan(&a),
        "reverse" => {
            if a.positional.is_empty() && a.get("out").is_none() {
                println!("Note: no file given, playing a generated signal from memory; the backward-seek cost is NOT represented.");
            }
            listen::run_reverse(&a)
        }
        "jump" => listen::run_jump(&a),
        "xfade" => listen::run_xfade(&a),
        _ => unreachable!(),
    }
}

fn run(argv: Vec<String>) -> Result<()> {
    let Some(sub) = argv.first().map(String::as_str) else {
        print!("{}", cmds::MAIN_HELP);
        return Ok(());
    };
    let rest = &argv[1..];
    match sub {
        "-h" | "--help" | "help" => {
            print!("{}", cmds::MAIN_HELP);
            Ok(())
        }
        "info" => cmds::cmd_info(rest),
        "decode" => cmds::cmd_decode(rest),
        "usbread" => cmds::cmd_usbread(rest),
        "seek" => cmds::cmd_seek(rest),
        "latency" => cmds::cmd_latency(rest),
        "cpu" => cmds::cmd_cpu(rest),
        "all" => cmds::cmd_all(rest),
        "suggest" => cmds::cmd_suggest(rest),
        "gen" => cmds::cmd_gen(rest),
        "sheet" => cmds::cmd_sheet(rest),
        "listen" => listen_cmd(rest),
        other => bail!("unknown sub-command '{other}' (try: warp_bench --help)"),
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(argv) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
