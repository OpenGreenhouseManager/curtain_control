#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use embassy_net::Runner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::wifi::{
    self, ClientConfig, ModeConfig, WifiController, WifiDevice, WifiEvent, WifiStaState,
};
use embedded_io_async::{Read as _, Write as _};
use log::{debug, error, info, trace};
use serde::Deserialize;

extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

const SERVER_IP_V4: [u8; 4] = [192, 168, 178, 21]; // Raspberry Pi IP
const SERVER_PORT: u16 = 9000; // TCP server port on the Pi
const RECONNECT_DELAY_MS: u64 = 2_000;
const CLIENT_UUID: &str = "8a3a3b0e-10b0-4f5e-bb14-7eac9ced0001";

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // generator version: 1.1.0

    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 66320);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);
    let mut rng = esp_hal::rng::Rng::new();
    info!("Embassy initialized!");

    let radio_init = alloc::boxed::Box::leak(alloc::boxed::Box::new(
        esp_radio::init().expect("Failed to initialize Wi-Fi/BLE controller"),
    ));
    let (mut wifi_controller, interfaces) =
        esp_radio::wifi::new(radio_init, peripherals.WIFI, Default::default())
            .expect("Failed to initialize Wi-Fi controller");

    // TODO: Spawn some tasks
    let config = embassy_net::Config::dhcpv4(Default::default());

    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // Init network stack
    let (stack, runner) = embassy_net::new(
        interfaces.sta,
        config,
        mk_static!(
            embassy_net::StackResources<3>,
            embassy_net::StackResources::<3>::new()
        ),
        seed,
    );

    spawner.spawn(connection(wifi_controller)).ok();
    spawner.spawn(net_task(runner)).ok();

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];
    let mut cached_value: u8 = 0;

    //wait until wifi connected
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    info!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            info!("Got IP: {}", config.address); //dhcp IP address
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    // Main client loop: connect, read lines, reconnect on error/close
    loop {
        // Small delay to avoid tight reconnect loops
        Timer::after(Duration::from_millis(1_000)).await;

        let mut socket = embassy_net::tcp::TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        // Do not set a read timeout; idle periods are expected. The server may stay silent
        // between commands, so we keep the connection open indefinitely.
        socket.set_timeout(None);

        let address = embassy_net::IpAddress::Ipv4(SERVER_IP_V4.into());
        info!("Connecting to {}.{}.{}.{}:{} ...", SERVER_IP_V4[0], SERVER_IP_V4[1], SERVER_IP_V4[2], SERVER_IP_V4[3], SERVER_PORT);
        match socket.connect((address, SERVER_PORT)).await {
            Ok(()) => info!("TCP connected"),
            Err(e) => {
                error!("Connect error: {:?}", e);
                Timer::after(Duration::from_millis(RECONNECT_DELAY_MS)).await;
                continue;
            }
        }

        // Send register immediately after connect
        {
            let reg = alloc::format!(r#"{{"type":"register","uuid":"{}"}}"#, CLIENT_UUID);
            debug!("TX: {}", reg);
            if let Err(e) = socket.write_all(reg.as_bytes()).await {
                error!("Register write error: {:?}", e);
                // reconnect
                continue;
            }
            if let Err(e) = socket.write_all(b"\n").await {
                error!("Register newline write error: {:?}", e);
                continue;
            }
            info!("Sent register");
        }

        // Read newline-delimited messages and respond
        let mut line_buf = [0u8; 512];
        let mut line_len: usize = 0;
        let mut chunk = [0u8; 128];

        'read_loop: loop {
            match socket.read(&mut chunk).await {
                Ok(0) => {
                    info!("Server closed connection");
                    break 'read_loop;
                }
                Ok(n) => {
                    trace!("RX chunk ({} bytes): {:02X?}", n, &chunk[..n]);
                    for &b in &chunk[..n] {
                        if b == b'\n' {
                            // process the completed line
                            let line = &line_buf[..line_len];
                            if let Ok(mut s) = core::str::from_utf8(line) {
                                // Trim CR if present
                                s = s.trim_end_matches('\r');
                                if !s.is_empty() {
                                    // Try to parse JSON command
                                    debug!("RX line: {}", s);
                                    handle_line(s, &mut cached_value, &mut socket).await;
                                }
                            } else {
                                error!("Received non-UTF8 line ({} bytes), ignoring", line_len);
                            }
                            line_len = 0;
                        } else if line_len < line_buf.len() {
                            line_buf[line_len] = b;
                            line_len += 1;
                        } else {
                            // overflow; drop the line
                            error!("Line too long; dropping");
                            line_len = 0;
                        }
                    }
                }
                Err(e) => {
                    error!("Read error: {:?}", e);
                    break 'read_loop;
                }
            }
        }

        // Allow some time before reconnecting
        Timer::after(Duration::from_millis(RECONNECT_DELAY_MS)).await;
    }

    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/esp-hal-v~1.0/examples
}

// maintains wifi connection, when it disconnects it tries to reconnect
#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    info!("start connection task");
    debug!("Device capabilities: {:?}", controller.capabilities());
    loop {
        match wifi::sta_state() {
            WifiStaState::Connected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }
        let c = ClientConfig::default()
            .with_ssid("FRITZ!Box 7530 PS".into())
            .with_password("06346084740791889371".into());
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = ModeConfig::Client(c);
            controller.set_config(&client_config).unwrap();
            info!("Starting wifi");
            controller.start_async().await.unwrap();
            info!("Wifi started!");
        }
        info!("About to connect...");

        match controller.connect_async().await {
            Ok(_) => info!("Wifi connected!"),
            Err(e) => {
                error!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

// A background task, to process network events - when new packets, they need to processed, embassy-net, wraps smoltcp
#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}

#[derive(Deserialize)]
struct IncomingCommand<'a> {
    #[serde(rename = "type")]
    cmd_type: &'a str,
    #[serde(default)]
    id: Option<u32>,
    #[serde(default)]
    value: Option<u32>,
}

async fn handle_line(
    s: &str,
    cached_value: &mut u8,
    socket: &mut embassy_net::tcp::TcpSocket<'_>,
) {
    // Parse with serde-json-core; ignore on failure
    match serde_json_core::de::from_str::<IncomingCommand>(s) {
        Ok((cmd, _rest)) => {
            match cmd.cmd_type {
                "set_value" => {
                    if let (Some(id), Some(v)) = (cmd.id, cmd.value) {
                        if v <= 100 {
                            info!("set_value id={} value={}", id, v);
                            *cached_value = v as u8;
                            // Acknowledge success
                            let msg = alloc::format!(r#"{{"type":"ack","id":{},"ok":true}}"#, id);
                            debug!("TX: {}", msg);
                            if let Err(e) = socket.write_all(msg.as_bytes()).await {
                                error!("Write error (ack set_value id={}): {:?}", id, e);
                            }
                            if let Err(e) = socket.write_all(b"\n").await {
                                error!("Write error (newline ack set_value id={}): {:?}", id, e);
                            }
                        } else {
                            // Invalid range
                            let msg = alloc::format!(
                                r#"{{"type":"error","id":{},"message":"value out of range 0..100"}}"#,
                                id
                            );
                            debug!("TX: {}", msg);
                            if let Err(e) = socket.write_all(msg.as_bytes()).await {
                                error!("Write error (error set_value id={}): {:?}", id, e);
                            }
                            if let Err(e) = socket.write_all(b"\n").await {
                                error!("Write error (newline error set_value id={}): {:?}", id, e);
                            }
                        }
                    } else if let Some(id) = cmd.id {
                        let msg = alloc::format!(
                            r#"{{"type":"error","id":{},"message":"missing value"}}"#,
                            id
                        );
                        debug!("TX: {}", msg);
                        if let Err(e) = socket.write_all(msg.as_bytes()).await {
                            error!("Write error (error missing value id={}): {:?}", id, e);
                        }
                        if let Err(e) = socket.write_all(b"\n").await {
                            error!("Write error (newline error missing value id={}): {:?}", id, e);
                        }
                    }
                }
                "get_value" => {
                    if let Some(id) = cmd.id {
                        info!("get_value id={} -> {}", id, *cached_value as u8);
                        let msg = alloc::format!(
                            r#"{{"type":"value","id":{},"value":{}}}"#,
                            id,
                            *cached_value as u8
                        );
                        debug!("TX: {}", msg);
                        if let Err(e) = socket.write_all(msg.as_bytes()).await {
                            error!("Write error (value id={}): {:?}", id, e);
                        }
                        if let Err(e) = socket.write_all(b"\n").await {
                            error!("Write error (newline value id={}): {:?}", id, e);
                        }
                    }
                }
                "calibrate" => {
                    if let Some(id) = cmd.id {
                        info!("calibrate start (id={})", id);
                        calibrate_routine().await;
                        info!("calibrate done (id={})", id);
                        let msg = alloc::format!(r#"{{"type":"ack","id":{},"ok":true}}"#, id);
                        debug!("TX: {}", msg);
                        if let Err(e) = socket.write_all(msg.as_bytes()).await {
                            error!("Write error (ack calibrate id={}): {:?}", id, e);
                        }
                        if let Err(e) = socket.write_all(b"\n").await {
                            error!("Write error (newline ack calibrate id={}): {:?}", id, e);
                        }
                    }
                }
                _ => {
                    // Ignore unknown types
                }
            }
        }
        Err(e) => {
            let _ = e; // ignore parse errors; robustness over strictness
        }
    }
}

async fn calibrate_routine() {
    // Placeholder: simulate calibration delay
    Timer::after(Duration::from_millis(200)).await;
}
