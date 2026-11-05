//! Non-destructive GPIO/button check. No Bluetooth, flash writes or pairing changes.
#[path = "../src/hardware_controls.rs"]
mod hardware_controls;
#[path = "../src/hardware_logic.rs"]
mod hardware_logic;
use catplay_c2a::HardwareConfig;
use hardware_controls::HardwareControls;
use hardware_logic::IndicatorState;
use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

fn main() {
    catplay_tracing::logger::setup_prod_logger();
    let args: Vec<_> = std::env::args().collect();
    if args.len() == 2 && args[1] == "--list-phones" {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        match runtime.block_on(catplay_bt::BluezManager::new().paired_iphones("hci0")) {
            Ok(phones) => println!("Paired iAP2 phones: {} (addresses omitted)", phones.len()),
            Err(err) => {
                eprintln!("Phone query failed: {err}");
                std::process::exit(1);
            }
        }
        return;
    }
    if args.len() == 3 && args[1] == "--led" {
        let cfg = HardwareConfig {
            enabled: true,
            led_spi_device: Some(args[2].clone()),
            ..HardwareConfig::default()
        };
        let mut controls = HardwareControls::new(cfg);
        controls.update(IndicatorState::Waiting);
        if !controls.led_ready() {
            eprintln!("SPI LED unavailable");
            std::process::exit(1);
        }
        for state in [
            IndicatorState::Waiting,
            IndicatorState::Connected,
            IndicatorState::Switching,
            IndicatorState::Error,
        ] {
            println!("LED pattern: {state:?} (6 seconds)");
            io::stdout().flush().unwrap();
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(6) {
                controls.update(state);
                if !controls.led_ready() {
                    std::process::exit(1);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        return;
    }
    if args.len() != 3 {
        eprintln!("Usage: hardware_probe /dev/gpiochipN LINE_OFFSET");
        eprintln!("       hardware_probe --list-phones | --led /dev/spidevN.M");
        std::process::exit(2);
    }
    let config = HardwareConfig {
        enabled: true,
        button_gpiochip: Some(args[1].clone()),
        button_line: Some(args[2].parse().expect("line offset")),
        button_active_low: true,
        button_bias_pull_up: true,
        ..HardwareConfig::default()
    };
    let mut controls = HardwareControls::new(config);
    controls.update(IndicatorState::Waiting);
    if !controls.button_ready() {
        eprintln!("GPIO request failed; check chip, offset, ownership and pull-up support");
        std::process::exit(1);
    }
    println!("GPIO ready. Release button first; short presses will be counted for 45 seconds.");
    io::stdout().flush().unwrap();
    let start = Instant::now();
    let mut count = 0;
    while start.elapsed() < Duration::from_secs(45) {
        if controls.update(IndicatorState::Waiting) {
            count += 1;
            println!("Short press {count}");
            io::stdout().flush().unwrap();
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    println!("Finished: {count} short presses. GPIO released on exit.");
}
