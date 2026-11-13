# Plippasoft
v0.5.0 

Reference software for the [Noubin standard](https://github.com/TobyKLight/Noubin) 
It powers the [Plippa media player](https://nowwemake.com/plippa)



## Status 

**Overall if you're an expert in wiring up RPIs then I think you could do a lot with Plippasoft already. But it's not in any shape for novices yet.** 

Today the Plippa software is only tested running on my initial prototype target, a RPI CM4 (1GB ram), with a specific hardware loadout, details are here `Plippa_PreRelease\boards\prototypes\plippa-D-poc2` 
- It's not generalised as a linux desktop app 
- UI is only designed for a 320x240px screen 
- There is the start of a hardware abstraction layer but it's not complete.
- It's a bit of an extreme case using almost all of the RPI GPIO.
- No binaries provided yet, you have to build yourself  
- So far it's developed as if the RPI is dedicated to only this app. 
- Security wise neither the app or deploy process is particularly hardened so take care. I develop on a dedicated linux VM, connecting to an RPI on a LAN behind a NAT/firewall that isn't allowing incoming connections from the web.

Beyond what's in `Plippa_PreRelease\boards\prototypes\plippa-D-poc2` the hardware design (e.g. laser cut files, kicad files) is not yet open source. The reason is the current hardware is actually pretty difficult to put together right now, quite gnarly and with a couple of bugs. I do plan to publish a maker friendly hardware design of a Plippa at some point. In the meantime though there's enough info here for experts to design their own cases and boards and wiring harnesses (that's the fun part anyway).

## Features 

- Plays MP3, FLAC and WAV 
- Read and Write NFC tags 
- Waveform analysis and display 
- Playback speed control with pitch corrected / vinyl modes 
- "Cassette Deck" style fast forward and rewind 
- Display cover art
- Portable Media Library on a USB stick (Format as exFAT for best resistance to power loss corruption)
- Optional browser portal for adding and managing media 
- Connect to bandcamp (via subsonic API) to browse and download releases 
- Plippa can automatically download bandcamp media on tap when the following is true: 
  - Plippa has internet access  
  - The Plippa is signed into the bandcamp subsonic API and you own the relevant bandcamp album 
  - The Noubin is a "public Noubin" with  a properly set up a Noubin URL, whose metadata contains the bandcamp release link 
    - you can create Noubin URLs for free on noubin.com or selfhost 
Note it's only possible to download from bandcamp as MP3 at this time. (Bandcamp needs to enable the full download endpoint on their subsonic API, fingers crossed that comes soon)


## Licence summary 

- Plippasoft is open source.  
- You can use Plippasoft to make a commercial media player you sell BUT you must come up with your own name, design, branding and UI sound ("Plippa") for your product. We reserve the exclusive right to the Plippa design and branding so that only we sell the original Plippa. 
- However that restriction only applies to commercial use. For personal use you are welcome and encouraged to name your Media Player Plippa and use our UI. 
- If you publish a fork of Plippasoft or your own Plippa hardware design, you may call it Plippa for personal, non-commercial use, but if you include the branding you must include a copy of `USE OF BRANDING.md` in your repo (and link to it from your README) so that everyone who finds it knows the Plippa name and branding can't be used commercially. Say that it is unofficial and link back to this repo. If you drop the Plippa name, logo and `assets/plippaSFX` entirely you don't need to include it.

For full details see 
LICENCE.md (Apache-2.0)  
USE OF BRANDING.md (the Plippa name, logo, design and the UI sounds in `assets/plippaSFX`)  
USE OF TEST MEDIA.md (the music in `assets/testMedia`, downloaded by `scripts/fetch-testmedia.sh`)

## Contributing 

I'd love to have collaborators, but I don't accept random PRs. 
If you want to collab open a discussion first about what you wanna do. 

This is because as a designer I've got a pretty fixed idea where I want to go with the feature set of Plippa and I think probably not everything will fit. 
That being said if we don't align I'm not at all offended by forks, please fork away, do great things, always happy to see that.  

# Usage 

Written in rust.

Deployment scripts are set up for compiling on my linux VM then pushing to the RPI over the network. 
(Initially I was compilng on the RPI itself but if you only have a 1GB ram CM4 it's gonna take a while)

For the scripts below the RPI is setup with a user called `plip`.
You can use a password or setup an ssh key.  
IMPORTANT: If you use a password the deploy script will prompt you for it the first time, and then save it in plain text to `~/.config/plippa-deploy/password` on your development machine.  

## Original Target 

RPI CM4, 1GB Ram, 8GB emmc + wifi, running RPI OS Debian 13 "Trixie", 64-bit (aarch64)
+ USB stick formatted with exFAT as the media library. 

Hardware and OS setup (overlays, PipeWire, groups, LEDs, autostart): [`boards/prototypes/plippa-D-poc2/boardSetup.md`](boards/prototypes/plippa-D-poc2/boardSetup.md). Bring-up checklist: [`bringup-poc2.md`](boards/prototypes/plippa-D-poc2/bringup-poc2.md).

## Docs 

There are some architecture docs and info about the IPC protocol in `docs\design`

## Evals 

There are some evaluation apps, basically test apps under /eval. 
Use these to ensure that e.g. your NFC transceiver is properly connected. 

## Build and deploy

The CM4 is too small to compile this workspace, so [`scripts/deploy.sh`](scripts/deploy.sh) cross-compiles for aarch64 on a Linux x86_64 development machine (I use a VM running on windows host) and rsyncs to the Pi. 

**One-time setup on the dev machine**

1. Install Rust (`rustup`), `rsync`, `ssh`, `python3`, and about 6 GB of free disk.
2. Install the toolchain and target:
   ```bash
   sudo apt install gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libclang-dev
   rustup target add aarch64-unknown-linux-gnu
   ```
3. Copy [`scripts/deploy/deploy.env.example`](scripts/deploy/deploy.env.example) to `~/.config/plippa-deploy/deploy.env` and set `PI_HOST` (and `PI_USER` / `PI_DIR` if you differ from the defaults: `plip`, `/home/plip/projects/Plippa_PreRelease`).
4. Authenticate with an ssh key (recommended: `ssh-copy-id plip@<pi-host>`) or a password. `scripts/deploy.sh setup` prompts for the password, stores it in plain text at `~/.config/plippa-deploy/password` (mode 600, outside the repo), trusts the host key and syncs the Pi's libraries into `~/.config/plippa-deploy/sysroot` for linking. Use a password you don't use anywhere else. `sudo` on the Pi (setcap, service restart) still needs the password unless you configure passwordless sudo.
5. Optional, only if you want to run the tests on this machine: `scripts/fetch-testmedia.sh` downloads the test audio (not stored in git) into `assets/testMedia/`.

**Deploy**

```bash
scripts/deploy.sh          # build + push + setcap plippa-leds + restart plippa.service
scripts/deploy.sh build    # build only (target/cross/aarch64-unknown-linux-gnu/<profile>/)
scripts/deploy.sh push     # push last build only
scripts/deploy.sh sysroot  # re-sync Pi libraries (after an OS upgrade or installing -dev libs)
```

**Build profile.** The device runs whatever `ExecStart` its systemd unit names. The base unit ([`plippa.service`](boards/prototypes/plippa-D-poc2/plippa.service)) names `target/debug/plippa`; a drop-in at `/etc/systemd/system/plippa.service.d/profile.conf` should override it to `target/release/plippa` (debug is about 10x slower on the CM4 and cannot hold 30 FPS). `deploy.sh` defaults to `PROFILE=release`. If the drop-in and the profile disagree, the push succeeds but the device keeps running the old build. Check the effective unit with `systemctl cat plippa.service`.

## Run

Service control on the Pi:

```bash
sudo systemctl start|stop|restart plippa.service
sudo systemctl enable|disable --now plippa.service   # autostart on boot
journalctl -u plippa.service -f                      # logs
```

**Media library.** By default the server and engine auto-discover volumes mounted under `/media/<user>/`. To pin one path, set `PLIPPA_LIBRARY_ROOT` or pass `--library-root`; discovery is then skipped. The first scan creates `<library>/.plippa/` (SQLite index).

**Portal.** `http://<pi-ip>:8787/` (default bind `0.0.0.0:8787`, override with `--bind` / `PLIPPA_BIND`). Shown on the device under settings > network.

**Mock mode (desktop, no hardware).** Build `plippa-server`, `plippa-engine`, `plippa-ui`, `plippa-leds` (without the `ws281x` feature) and `plippa` with `cargo build -p <name>`, then:

```bash
export PLIPPA_ROOT="$PWD" PLIPPA_SINK=mock
cd target/debug && ./plippa --board=plippa-D-poc2 --backend=mock --sink=mock
```

**Tests (no hardware).** The tests use real audio files that are not stored in git. Fetch them once with `scripts/fetch-testmedia.sh` (downloads a pinned, checksum-verified archive from the GitHub release into `assets/testMedia/`), then `cargo test -p plippa -p plippa-ui -p plippa-engine -p plippa-server`.

## Backup the media library

[`scripts/backup.sh`](scripts/backup.sh) copies the library from the device to your machine, verifies it, then atomically replaces the previous backup. It reuses the connection settings and credentials from `deploy.sh`. Needs `bash`, `ssh`, `tar` (`rsync` optional).

```bash
scripts/backup.sh --dest ~/PlippaBackup --quiesce   # --dest is required; --quiesce stops the service while copying
scripts/backup.sh --help      # destination, source, verify mode, excludes
```

## Notes

- Set `core_freq_min=500` in `/boot/firmware/config.txt` on the CM4 so SPI display speed doesn't collapse when the core clock drops at idle.

# Credits

UI icons (shuffle, repeat) by [Smallbits](https://smallbits.design). Third-party code and asset attribution: [`docs/third-party-notices.md`](docs/third-party-notices.md), with the complete crate inventory in [`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md).
