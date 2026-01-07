#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use curtain_control::tcp_client::TcpClient;
use embassy_executor::Spawner;
use embassy_net::Runner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::wifi::{
    self, ClientConfig, ModeConfig, WifiController, WifiDevice, WifiEvent, WifiStaState,
};
use log::{debug, error, info};
extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write($val);
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
    let rng = esp_hal::rng::Rng::new();
    info!("Embassy initialized!");

    let radio_init = alloc::boxed::Box::leak(alloc::boxed::Box::new(
        esp_radio::init().expect("Failed to initialize Wi-Fi/BLE controller"),
    ));
    let (wifi_controller, interfaces) =
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

    let _rx_buffer: [i32; _] = [0; 4096];
    let _tx_buffer = [0; 4096];
    let _cached_value: u8 = 0;

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

        let mut client = TcpClient::new().await;
        client.connect(&stack).await;

        // Send register immediately after connect

        client.serve().await;

        // Allow some time before reconnecting
        Timer::after(Duration::from_millis(curtain_control::RECONNECT_DELAY_MS)).await;
    }

    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/esp-hal-v~1.0/examples
}

// maintains wifi connection, when it disconnects it tries to reconnect
#[allow(clippy::large_stack_frames)]
#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    info!("start connection task");
    debug!("Device capabilities: {:?}", controller.capabilities());
    loop {
        if wifi::sta_state() == WifiStaState::Connected {
            // wait until we're no longer connected
            controller.wait_for_event(WifiEvent::StaDisconnected).await;
            Timer::after(Duration::from_millis(5000)).await
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
