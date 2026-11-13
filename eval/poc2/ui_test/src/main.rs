// see ui_test/pinout.md for hardware setup

mod aw9523;
mod encoder;
mod leds;

use aw9523::{Aw9523, SwitchDebouncer, SwitchEvent, SwitchId};
use leds::{
    open_strips, LedState, Meter, LED_BT_SW1, LED_BT_SW2, LED_BT_SW3, LED_BT_SW4, LED_BT_SW5,
    LED_BT_SW6, LED_SW2, LED_SW6, FRONT_COUNT, PIN_FRONT, PIN_TOP, TOP_COUNT,
};
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

static RUNNING: AtomicBool = AtomicBool::new(true);

const DEFAULT_BRIGHTNESS_PCT: u8 = 30;
const DEFAULT_DEBOUNCE_MS: u64 = 20;
const DEFAULT_I2C_BUS: u8 = 1;
const DEFAULT_I2C_ADDR: u16 = 0x58;
const CHASE_FPS: u64 = 30;

struct AppConfig {
    brightness_pct: u8,
    debounce_ms: u64,
    i2c_bus: u8,
    i2c_addr: u16,
    front_leds: i32,
    top_leds: i32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            brightness_pct: DEFAULT_BRIGHTNESS_PCT,
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            i2c_bus: DEFAULT_I2C_BUS,
            i2c_addr: DEFAULT_I2C_ADDR,
            front_leds: FRONT_COUNT as i32,
            top_leds: TOP_COUNT as i32,
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    ctrlc::set_handler(|| RUNNING.store(false, Ordering::SeqCst))?;

    let app = parse_args();
    let bus_path = PathBuf::from(format!("/dev/i2c-{}", app.i2c_bus));

    println!(
        "POC2 UI test — encoders + AW9523 switches + WS2811 LEDs\n\
         Front GPIO {PIN_FRONT} ({} LEDs), top GPIO {PIN_TOP} ({} LEDs)\n\
         Brightness {}%, debounce {} ms, I2C {} @ 0x{:02x}\n\
         Ctrl-C to exit and clear",
        app.front_leds,
        app.top_leds,
        app.brightness_pct,
        app.debounce_ms,
        bus_path.display(),
        app.i2c_addr
    );

    let mut encoders = encoder::open_all()?;
    let mut aw = Aw9523::open(&bus_path, app.i2c_addr)?;
    let mut debounce = SwitchDebouncer::new(app.debounce_ms);
    let mut controller = open_strips(app.front_leds, app.top_leds, app.brightness_pct)?;
    let mut leds = LedState::new_all_base(CHASE_FPS, leds::rgb(255, 0, 0));

    leds.apply_to(&mut controller);
    controller
        .render()
        .map_err(|e| format!("render failed: {e:?}"))?;
    println!("All LEDs red (SW7 high→red, low→green) — interact…");

    while RUNNING.load(Ordering::SeqCst) {
        let mut dirty = false;

        // Encoder rotation → meters
        for (i, enc) in encoders.iter_mut().enumerate() {
            let step = enc.poll();
            if step == 0 {
                continue;
            }
            let meter = match i {
                0 => Meter::Vul,  // SWE1 → left volume
                1 => Meter::Swe3, // SWE3 → SWE3 meter
                2 => Meter::Swe4, // SWE4 → SWE4 meter
                _ => Meter::Vur,  // SWE5 → right volume
            };
            leds.adjust_meter(meter, step);
            println!("{} rotate {step:+} → {:?}", enc.name, meter_label(meter));
            dirty = true;
        }

        // Switch events
        let (p0, p1) = aw.read_ports()?;
        for event in debounce.poll_events(p0, p1) {
            match event {
                SwitchEvent::Level { id: SwitchId::Sw7, low } => {
                    let level = if low { "low" } else { "high" };
                    leds.set_base_from_sw7(low);
                    println!(
                        "SW7 {level} → base colour {}",
                        leds.base_name()
                    );
                    dirty = true;
                }
                SwitchEvent::Level { id, low } => {
                    println!(
                        "{} level {}",
                        id.label(),
                        if low { "low" } else { "high" }
                    );
                }
                SwitchEvent::Pressed(id) if id.has_toggle_led() => {
                    println!(
                        "{} pressed  (PA=0b{:08b} PB=0b{:08b})",
                        id.label(),
                        p0,
                        p1
                    );
                    apply_toggle(&mut leds, id);
                    leds.start_nfc_chase();
                    println!("  NFC chase started");
                    dirty = true;
                }
                SwitchEvent::Pressed(id) if id.is_swe() => {
                    println!("{} pressed", id.label());
                    leds.start_nfc_chase();
                    if id == SwitchId::Swe1 {
                        leds.toggle_power();
                        println!(
                            "  power LED → {}",
                            if leds.power_blue { "blue" } else { leds.base_name() }
                        );
                    }
                    println!("  NFC chase started");
                    dirty = true;
                }
                SwitchEvent::Pressed(id) => {
                    println!("{} pressed", id.label());
                }
                SwitchEvent::Released(id) => {
                    println!(
                        "{} released  (PA=0b{:08b} PB=0b{:08b})",
                        id.label(),
                        p0,
                        p1
                    );
                }
            }
        }

        if leds.tick_chase() {
            dirty = true;
        }

        if dirty {
            leds.apply_to(&mut controller);
            if let Err(e) = controller.render() {
                eprintln!("render failed: {e:?}");
            }
        }

        thread::sleep(Duration::from_millis(1));
    }

    leds.clear_all();
    leds.apply_to(&mut controller);
    let _ = controller.render();
    println!("Done.");
    Ok(())
}

fn apply_toggle(leds: &mut LedState, id: SwitchId) {
    match id {
        SwitchId::Sw2 => leds.toggle_button_led(true, LED_SW2),
        SwitchId::Sw6 => leds.toggle_button_led(true, LED_SW6),
        SwitchId::BtSw1 => leds.toggle_button_led(false, LED_BT_SW1),
        SwitchId::BtSw2 => leds.toggle_button_led(false, LED_BT_SW2),
        SwitchId::BtSw3 => leds.toggle_button_led(false, LED_BT_SW3),
        SwitchId::BtSw4 => leds.toggle_button_led(false, LED_BT_SW4),
        SwitchId::BtSw5 => leds.toggle_button_led(false, LED_BT_SW5),
        SwitchId::BtSw6 => leds.toggle_button_led(false, LED_BT_SW6),
        _ => {}
    }
}

fn meter_label(m: Meter) -> &'static str {
    match m {
        Meter::Vul => "left volume",
        Meter::Vur => "right volume",
        Meter::Swe3 => "SWE3 meter",
        Meter::Swe4 => "SWE4 meter",
    }
}

fn parse_args() -> AppConfig {
    let mut app = AppConfig::default();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--brightness" => {
                app.brightness_pct = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_BRIGHTNESS_PCT)
                    .min(100);
            }
            "--debounce-ms" => {
                app.debounce_ms = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_DEBOUNCE_MS);
            }
            "--i2c-bus" => {
                app.i2c_bus = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_I2C_BUS);
            }
            "--i2c-addr" => {
                let raw = args.next().unwrap_or_else(|| format!("0x{DEFAULT_I2C_ADDR:02x}"));
                app.i2c_addr = parse_hex_u16(&raw).unwrap_or(DEFAULT_I2C_ADDR);
            }
            "--front-leds" => {
                app.front_leds = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(FRONT_COUNT as i32);
            }
            "--top-leds" => {
                app.top_leds = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(TOP_COUNT as i32);
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => eprintln!("unknown arg: {other} (try --help)"),
        }
    }
    app
}

fn parse_hex_u16(s: &str) -> Option<u16> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16).ok()
    } else {
        s.parse().ok()
    }
}

fn print_usage() {
    println!(
        "usage: poc2_ui_test [options]\n\
         \n\
         Interactive UI test: rotary encoders + switches (AW9523) + WS2811 LEDs.\n\
         Starts with all LEDs red. Requires root (sudo) for PWM/DMA/GPIO.\n\
         \n\
         options:\n\
           --brightness P     brightness percent 0–100 (default {DEFAULT_BRIGHTNESS_PCT})\n\
           --debounce-ms N    switch debounce ms (default {DEFAULT_DEBOUNCE_MS})\n\
           --i2c-bus N        I2C bus number (default {DEFAULT_I2C_BUS})\n\
           --i2c-addr 0xHH    AW9523 address (default 0x{DEFAULT_I2C_ADDR:02x})\n\
           --front-leds N     front strip length (default {FRONT_COUNT})\n\
           --top-leds N       top strip length (default {TOP_COUNT})\n\
         \n\
         Wiring: see eval/poc2/ui_test/pinout.md"
    );
}
