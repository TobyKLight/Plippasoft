//! `plippa-media-helper`: decodes or probes ONE media file in a process of its own, so a
//! decoder that aborts (Symphonia allocates by attacker-controlled length fields) takes down
//! only this helper. Started by `plippa_audio::isolate`, never by hand: it refuses to run
//! unless its parent applied the resource limits and handed it the confined media file.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(plippa_audio::isolate::server::helper_main(args));
}
