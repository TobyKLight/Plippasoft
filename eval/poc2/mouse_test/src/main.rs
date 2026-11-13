//! USB mouse detector — lists plugged-in USB mice and prints raw HID reports.

use anyhow::{bail, Context, Result};
use nix::poll::{poll, PollFd, PollFlags};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::BorrowedFd;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const USB_BUSTYPE: &str = "0003";

#[derive(Debug, Clone)]
struct MouseInfo {
    hidraw_path: PathBuf,
    event_path: Option<PathBuf>,
    vendor_id: u16,
    product_id: u16,
    name: String,
    input_name: String,
    phys: String,
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let list_only = args.iter().any(|a| a == "--list");
    let device_index = parse_device_index(&args)?;

    let running = Arc::new(AtomicBool::new(true));
    {
        let running = Arc::clone(&running);
        ctrlc::set_handler(move || running.store(false, Ordering::SeqCst))
            .context("install Ctrl+C handler")?;
    }

    let mice = find_usb_mice().context("enumerate USB mice")?;

    if mice.is_empty() {
        bail!(
            "no USB mouse found on any port.\n\
             hints: plug in a mouse; check `lsusb`; confirm a hidraw node exists under /sys/class/hidraw"
        );
    }

    println!("Found {} USB mouse interface(s):", mice.len());
    for (i, mouse) in mice.iter().enumerate() {
        let marker = if Some(i) == device_index { " (selected)" } else { "" };
        println!(
            "  [{i}]{marker} {:04x}:{:04x} {} ({}) phys={} hidraw={}",
            mouse.vendor_id,
            mouse.product_id,
            mouse.name,
            mouse.input_name,
            mouse.phys,
            mouse.hidraw_path.display()
        );
        if let Some(event) = &mouse.event_path {
            println!("       evdev: {}", event.display());
        }
    }

    if list_only {
        return Ok(());
    }

    let index = device_index.unwrap_or_else(|| pick_default_mouse(&mice));
    let mouse = mice
        .get(index)
        .with_context(|| format!("device index {index} out of range ({} found)", mice.len()))?;

    if mice.len() > 1 && device_index.is_none() {
        println!(
            "\nMultiple mice detected — reading from [{index}] {}. Pass --device N to choose another.",
            mouse.hidraw_path.display()
        );
    } else {
        println!("\nReading from [{}] {} …", index, mouse.hidraw_path.display());
    }
    println!("Move the mouse and click buttons. Ctrl+C to stop.\n");

    match HidrawReader::open(&mouse.hidraw_path) {
        Ok(mut device) => {
            println!("source: hidraw (raw HID reports)\n");
            let mut buttons = 0u8;
            while running.load(Ordering::SeqCst) {
                let report = match device.read_report()? {
                    Some(report) => report,
                    None => continue,
                };
                print_hidraw_report(&report, &mut buttons);
            }
        }
        Err(hidraw_err) => {
            let event_path = mouse
                .event_path
                .as_ref()
                .context("hidraw unavailable and no evdev node found")?;
            eprintln!("note: hidraw open failed ({hidraw_err:#}); using evdev instead");
            println!("source: evdev {} (kernel input events)\n", event_path.display());

            let mut device = EvdevReader::open(event_path)?;
            while running.load(Ordering::SeqCst) {
                for event in device.poll_events()? {
                    print_evdev_event(&event);
                }
            }
        }
    }

    Ok(())
}

fn print_hidraw_report(report: &[u8], buttons: &mut u8) {
    let hex: String = report
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");

    let mut parts = vec![format!("report [{hex}]")];

    if let Some((dx, dy)) = parse_xy(report) {
        parts.push(format!("X={dx:+} Y={dy:+}"));
    }

    if let Some(new_buttons) = report.first().copied() {
        let clicks = describe_button_changes(*buttons, new_buttons);
        *buttons = new_buttons;
        if !clicks.is_empty() {
            parts.push(clicks);
        }
    }

    println!("{}", parts.join("  "));
    let _ = std::io::stdout().flush();
}

fn print_evdev_event(event: &EvdevEvent) {
    match event {
        EvdevEvent::Move { dx, dy } => {
            println!("X={dx:+} Y={dy:+}");
        }
        EvdevEvent::Button { name, pressed } => {
            println!(
                "click [{}:{}]",
                name,
                if *pressed { "down" } else { "up" }
            );
        }
    }
    let _ = std::io::stdout().flush();
}

struct HidrawReader {
    file: File,
}

enum EvdevEvent {
    Move { dx: i32, dy: i32 },
    Button { name: &'static str, pressed: bool },
}

struct EvdevReader {
    device: evdev::Device,
}

impl EvdevReader {
    fn open(path: &Path) -> Result<Self> {
        let mut device = evdev::Device::open(path)
            .with_context(|| format!("open {}", path.display()))?;
        if let Err(err) = device.grab() {
            eprintln!("warning: grab {} failed ({err})", path.display());
        }
        Ok(Self { device })
    }

    fn poll_events(&mut self) -> Result<Vec<EvdevEvent>> {
        use evdev::{InputEventKind, RelativeAxisType};

        let fd = self.device.as_raw_fd();
        if !wait_readable(fd, 200)? {
            return Ok(Vec::new());
        }

        let events = match self.device.fetch_events() {
            Ok(events) => events,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };

        let mut out = Vec::new();
        let mut dx = 0i32;
        let mut dy = 0i32;

        for event in events {
            match event.kind() {
                InputEventKind::RelAxis(RelativeAxisType::REL_X) => dx += event.value(),
                InputEventKind::RelAxis(RelativeAxisType::REL_Y) => dy += event.value(),
                InputEventKind::RelAxis(RelativeAxisType::REL_HWHEEL)
                | InputEventKind::RelAxis(RelativeAxisType::REL_WHEEL) => {
                    println!("wheel {:+}", event.value());
                }
                InputEventKind::Key(key) => {
                    if let Some(name) = button_name(key) {
                        out.push(EvdevEvent::Button {
                            name,
                            pressed: event.value() == 1,
                        });
                    }
                }
                _ => {}
            }
        }

        if dx != 0 || dy != 0 {
            out.insert(0, EvdevEvent::Move { dx, dy });
        }

        Ok(out)
    }
}

fn button_name(key: evdev::Key) -> Option<&'static str> {
    use evdev::Key;
    match key {
        Key::BTN_LEFT => Some("L"),
        Key::BTN_RIGHT => Some("R"),
        Key::BTN_MIDDLE => Some("M"),
        Key::BTN_SIDE => Some("B4"),
        Key::BTN_EXTRA => Some("B5"),
        _ => None,
    }
}

impl HidrawReader {
    fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .with_context(|| format!("open {} (try running as root or add udev rules)", path.display()))?;
        Ok(Self { file })
    }

    fn read_report(&mut self) -> Result<Option<Vec<u8>>> {
        if !wait_readable(self.file.as_raw_fd(), 200)? {
            return Ok(None);
        }

        let mut buf = [0u8; 64];
        let n = self.file.read(&mut buf).context("hidraw read")?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(buf[..n].to_vec()))
    }
}

fn find_usb_mice() -> Result<Vec<MouseInfo>> {
    let mut found = Vec::new();

    for entry in fs::read_dir("/sys/class/hidraw").context("read /sys/class/hidraw")? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("hidraw") {
            continue;
        }

        let device_dir = entry.path().join("device");
        let uevent = fs::read_to_string(device_dir.join("uevent")).unwrap_or_default();
        let (bus, vid, pid) = parse_hid_ids(&uevent);
        if bus != USB_BUSTYPE {
            continue;
        }

        let product_string = parse_uevent_field(&uevent, "HID_NAME=").unwrap_or_default();
        let phys = parse_uevent_field(&uevent, "HID_PHYS=").unwrap_or_default();
        let Some((event_path, input_name)) = find_mouse_input(&device_dir) else {
            continue;
        };

        found.push(MouseInfo {
            hidraw_path: PathBuf::from(format!("/dev/{name}")),
            event_path: Some(event_path),
            vendor_id: vid,
            product_id: pid,
            name: product_string,
            input_name,
            phys,
        });
    }

    found.sort_by(|a, b| a.hidraw_path.cmp(&b.hidraw_path));
    Ok(found)
}

fn pick_default_mouse(mice: &[MouseInfo]) -> usize {
    mice.iter()
        .enumerate()
        .max_by_key(|(_, m)| mouse_priority(m))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn mouse_priority(mouse: &MouseInfo) -> i32 {
    let mut score = 0;
    let input = mouse.input_name.to_ascii_lowercase();
    let hid = mouse.name.to_ascii_lowercase();

    if input.contains("mouse") && !input.contains("keyboard") {
        score += 10;
    }
    if hid.contains("mouse") {
        score += 5;
    }
    // Prefer dedicated pointing devices over macro-pad secondary interfaces.
    if hid == "sayodevice" {
        score -= 20;
    }
    score
}

fn parse_device_index(args: &[String]) -> Result<Option<usize>> {
    let mut index = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--device" {
            let value = args
                .get(i + 1)
                .context("--device requires an index (from --list)")?;
            index = Some(
                value
                    .parse()
                    .with_context(|| format!("invalid device index '{value}'"))?,
            );
            i += 2;
        } else {
            i += 1;
        }
    }
    Ok(index)
}

fn has_mouse_handler(input_dir: &Path) -> bool {
    fs::read_dir(input_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("mouse")
        })
}

fn find_mouse_input(device_dir: &Path) -> Option<(PathBuf, String)> {
    let input_root = device_dir.join("input");
    let entries = fs::read_dir(&input_root).ok()?;
    for entry in entries.filter_map(|e| e.ok()) {
        let input_dir = entry.path();
        if !has_mouse_handler(&input_dir) {
            continue;
        }
        let rel = fs::read_to_string(input_dir.join("capabilities/rel")).unwrap_or_default();
        if rel.trim() == "0" {
            continue;
        }
        let input_name = fs::read_to_string(input_dir.join("name"))
            .unwrap_or_default()
            .trim()
            .to_string();
        for child in fs::read_dir(&input_dir).ok()?.filter_map(|e| e.ok()) {
            let fname = child.file_name();
            let fname = fname.to_string_lossy();
            if fname.starts_with("event") {
                return Some((
                    PathBuf::from(format!("/dev/input/{fname}")),
                    input_name,
                ));
            }
        }
    }
    None
}

fn parse_hid_ids(uevent: &str) -> (&str, u16, u16) {
    for line in uevent.lines() {
        if let Some(rest) = line.strip_prefix("HID_ID=") {
            let parts: Vec<&str> = rest.split(':').collect();
            if parts.len() == 3 {
                let bus = parts[0];
                let vid = u16::from_str_radix(parts[1], 16).unwrap_or(0);
                let pid = u16::from_str_radix(parts[2], 16).unwrap_or(0);
                return (bus, vid, pid);
            }
        }
    }
    ("", 0, 0)
}

fn parse_uevent_field(uevent: &str, key: &str) -> Option<String> {
    uevent
        .lines()
        .find_map(|line| line.strip_prefix(key).map(str::to_string))
}

/// Parse X/Y from common HID mouse report layouts (boot protocol and typical 4+ byte reports).
fn parse_xy(report: &[u8]) -> Option<(i32, i32)> {
    if report.len() < 3 {
        return None;
    }

    let dx = report[1] as i8 as i32;
    let dy = report[2] as i8 as i32;

    if dx != 0 || dy != 0 {
        return Some((dx, dy));
    }

    // Some mice use 16-bit deltas at bytes 1-4 (buttons in byte 0).
    if report.len() >= 5 {
        let dx16 = i16::from_le_bytes([report[1], report[2]]) as i32;
        let dy16 = i16::from_le_bytes([report[3], report[4]]) as i32;
        if dx16 != 0 || dy16 != 0 {
            return Some((dx16, dy16));
        }
    }

    Some((dx, dy))
}

fn describe_button_changes(prev: u8, cur: u8) -> String {
    const BUTTONS: [(u8, &str); 5] = [
        (0x01, "L"),
        (0x02, "R"),
        (0x04, "M"),
        (0x08, "B4"),
        (0x10, "B5"),
    ];

    let mut changes = Vec::new();
    for (mask, label) in BUTTONS {
        let was = prev & mask != 0;
        let now = cur & mask != 0;
        if was == now {
            continue;
        }
        changes.push(format!("{label}:{}", if now { "down" } else { "up" }));
    }

    if changes.is_empty() {
        String::new()
    } else {
        format!("click [{}]", changes.join(", "))
    }
}

fn wait_readable(fd: i32, timeout_ms: u32) -> Result<bool> {
    let mut fds = [PollFd::new(
        unsafe { BorrowedFd::borrow_raw(fd) },
        PollFlags::POLLIN,
    )];
    let rc = poll(
        &mut fds,
        nix::poll::PollTimeout::try_from(timeout_ms).unwrap_or(nix::poll::PollTimeout::NONE),
    )?;
    Ok(rc > 0 && fds[0].revents().is_some_and(|r| r.contains(PollFlags::POLLIN)))
}
