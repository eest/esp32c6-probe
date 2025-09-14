#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use core::net::Ipv4Addr;
use core::str::FromStr;

use embassy_executor::Spawner;
use embassy_net::Stack;
use embassy_net::{tcp::TcpSocket, Runner, StackResources};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::channel::Receiver;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::timer::systimer::SystemTimer;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::timer::OneShotTimer;
use esp_hal::{clock::CpuClock, i2c::master, rng::Rng, time::Rate};
use esp_println::logger::init_logger_from_env;
use esp_wifi::{
    init,
    wifi::{ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiState},
    EspWifiController,
};
use heapless::String;
use log::info;
use rust_mqtt::{
    client::{client::MqttClient, client_config::ClientConfig},
    packet::v5::reason_codes::ReasonCode,
    utils::rng_generator::CountingRng,
};
use serde::Serialize;
use static_cell::StaticCell;

extern crate alloc;

use hdc302x::{Hdc302x, I2cAddr, LowPowerMode, ManufacturerId};

#[derive(Serialize)]
struct ProbeData {
    centigrade: f32,
    humidity_percent: f32,
}

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");
const MQTT_SERVER_IPV4: &str = env!("MQTT_SERVER_IPV4");
const MQTT_USERNAME: &str = env!("MQTT_USERNAME");
const MQTT_PASSWORD: &str = env!("MQTT_PASSWORD");

// When you are okay with using a nightly compiler it's better to use https://docs.rs/static_cell/2.1.0/static_cell/macro.make_static.html
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

static CHANNEL: StaticCell<Channel<NoopRawMutex, String<128>, 3>> = StaticCell::new();

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    // generator version: 0.4.0
    init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let mut rng = Rng::new(peripherals.RNG);

    let esp_wifi_ctrl = &*mk_static!(
        EspWifiController<'static>,
        init(timg0.timer0, rng, peripherals.RADIO_CLK).unwrap()
    );

    let (controller, interfaces) = esp_wifi::wifi::new(&esp_wifi_ctrl, peripherals.WIFI).unwrap();

    let wifi_interface = interfaces.sta;

    let systimer = SystemTimer::new(peripherals.SYSTIMER);
    esp_hal_embassy::init(systimer.alarm0);

    let i2c = master::I2c::new(
        peripherals.I2C0,
        master::Config::default().with_frequency(Rate::from_khz(400)),
    )
    .unwrap()
    .with_sda(peripherals.GPIO6)
    .with_scl(peripherals.GPIO7)
    .into_async();

    let delay = OneShotTimer::new(systimer.alarm1).into_async();

    let mut hdc302x = Hdc302x::new(i2c, delay, I2cAddr::Addr00);

    match hdc302x.read_manufacturer_id_async().await {
        Ok(ManufacturerId::TexasInstruments) => {
            info!(
                "hdc302x: manufacturer id: {}",
                ManufacturerId::TexasInstruments
            );
        }
        Ok(manuf_id) => {
            info!("hdc302x: unexpected manufacturer id: {manuf_id}");
            return;
        }
        Err(e) => {
            info!("hdc302x: read_manufacturer_id error: {e:?}");
            return;
        }
    }

    match hdc302x.read_serial_number_async().await {
        Ok(serial_number) => {
            info!("hdc302x: serial_number: {serial_number}");
        }
        Err(e) => {
            info!("hdc302x: read_serial_number error: {e:?}");
            return;
        }
    }

    match hdc302x.read_status_async(true).await {
        Ok(status_bits) => {
            info!("hdc302x: status_bits: {status_bits}");
        }
        Err(e) => {
            info!("hdc302x: read_status error: {e:?}");
            return;
        }
    }

    let config = embassy_net::Config::dhcpv4(Default::default());

    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // Init network stack
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(runner)).ok();

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    info!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            info!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    //let temp_channel = mk_static!(Channel::<NoopRawMutex, String<128>, 3>::new();
    let temp_channel = CHANNEL.init(Channel::new());
    spawner
        .spawn(mqtt_task(stack, temp_channel.receiver()))
        .ok();
    let temp_sender = temp_channel.sender();

    loop {
        Timer::after(Duration::from_millis(1_000)).await;

        loop {
            Timer::after(Duration::from_millis(1_000)).await;

            match hdc302x.read_status_async(true).await {
                Ok(status_bits) => {
                    info!("hdc302x: status_bits: {status_bits}");
                }
                Err(e) => {
                    info!("hdc302x: read_status error: {e:?}");
                    return;
                }
            }

            let raw_datum = hdc302x
                .one_shot_async(LowPowerMode::lowest_noise())
                .await
                .unwrap();

            let d = hdc302x::Datum::from(&raw_datum);
            info!("{:?}", d);

            let centigrade = raw_datum.centigrade().unwrap();
            let humidity_percent = raw_datum.humidity_percent().unwrap();

            let pdata = ProbeData {
                centigrade,
                humidity_percent,
            };

            let serialized: String<128> = serde_json_core::to_string(&pdata).unwrap();

            temp_sender.send(serialized).await;

            Timer::after(Duration::from_millis(3000)).await;
        }
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    info!("start connection task");
    info!("Device capabilities: {:?}", controller.capabilities());
    loop {
        match esp_wifi::wifi::wifi_state() {
            WifiState::StaConnected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: WIFI_SSID.into(),
                password: WIFI_PASSWORD.into(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            info!("Starting wifi");
            controller.start_async().await.unwrap();
            info!("Wifi started!");

            info!("Scan");
            let result = controller.scan_n_async(10).await.unwrap();
            for ap in result {
                info!("{:?}", ap);
            }
        }
        info!("About to connect...");

        match controller.connect_async().await {
            Ok(_) => info!("Wifi connected!"),
            Err(e) => {
                info!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}

#[embassy_executor::task]
async fn mqtt_task(
    stack: Stack<'static>,
    temp_receiver: Receiver<'static, NoopRawMutex, String<128>, 3>,
) {
    loop {
        let mut rx_buffer = [0; 4096];
        let mut tx_buffer = [0; 4096];

        let server_ipv4 = Ipv4Addr::from_str(MQTT_SERVER_IPV4).unwrap();

        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);

        socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));

        let remote_endpoint = (server_ipv4, 1883);
        info!("connecting to {}...", server_ipv4);
        let r = socket.connect(remote_endpoint).await;
        if let Err(e) = r {
            info!("connect error: {:?}", e);
            continue;
        }
        info!("connected!");

        let mut mqtt_config = ClientConfig::new(
            rust_mqtt::client::client_config::MqttVersion::MQTTv5,
            CountingRng(20000),
        );

        mqtt_config
            .add_max_subscribe_qos(rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1);
        mqtt_config.add_client_id("clientId-rs-testing");
        mqtt_config.add_username(MQTT_USERNAME);
        mqtt_config.add_password(MQTT_PASSWORD);
        mqtt_config.max_packet_size = 100;

        let mut recv_buffer = [0; 80];
        let mut write_buffer = [0; 80];

        let mut mqtt_client = MqttClient::<_, 5, _>::new(
            socket,
            &mut write_buffer,
            80,
            &mut recv_buffer,
            80,
            mqtt_config,
        );

        match mqtt_client.connect_to_broker().await {
            Ok(()) => {}
            Err(mqtt_error) => match mqtt_error {
                ReasonCode::NetworkError => {
                    info!("MQTT Network Error");
                    continue;
                }
                _ => {
                    info!("Other MQTT Error: {:?}", mqtt_error);
                    continue;
                }
            },
        }

        loop {
            let serialized = temp_receiver.receive().await;
            info!("sending MQTT message");
            match mqtt_client
                .send_message(
                    "temperature/1",
                    serialized.as_bytes(),
                    rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1,
                    false,
                )
                .await
            {
                Ok(()) => {}
                Err(mqtt_error) => match mqtt_error {
                    ReasonCode::NetworkError => {
                        info!("MQTT Network Error");
                        continue;
                    }
                    _ => {
                        info!("Other MQTT Error: {:?}", mqtt_error);
                        continue;
                    }
                },
            }
        }
    }
}
