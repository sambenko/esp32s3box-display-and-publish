#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use core::time::Duration as CoreDuration;

// display and graphics imports
use embedded_graphics::{
    pixelcolor::Rgb565, prelude::*,
};
use display_interface_spi::SPIInterfaceNoCS;
use mipidsi::{ColorOrder, Orientation};
extern crate ui;

// peripherals imports
use hal::{
    clock::{ClockControl, CpuClock},
    i2c::I2C,
    peripherals::{Interrupt, Peripherals, I2C0},
    prelude::{_fugit_RateExtU32, *},
    timer::TimerGroup,
    Rng, Rtc, IO, Delay, spi,
    {embassy, interrupt},
};

//wifi imports
use embedded_svc::wifi::{ClientConfiguration, Configuration, Wifi};
use esp_wifi::wifi::{WifiController, WifiDevice, WifiEvent, WifiMode, WifiState};
use esp_wifi::{initialize, EspWifiInitFor};

// embassy imports
use embassy_executor::Executor;
use embassy_executor::_export::StaticCell;
use embassy_net::tcp::TcpSocket;
use embassy_net::dns::DnsQueryType;
use embassy_net::{Config, Stack, StackResources};
use embassy_time::{Duration, Timer};

// mqtt imports
use rust_mqtt::{
    client::{client::MqttClient, client_config::ClientConfig},
    packet::v5::reason_codes::ReasonCode,
    utils::rng_generator::CountingRng,
};

// tls imports
use esp_mbedtls::{asynch::Session, set_debug, Mode, TlsVersion};
use esp_mbedtls::{Certificates, X509};

use bme680::*;

use heapless::String;
use core::fmt::Write;

use esp_backtrace as _;
use esp_println::println;

static EXECUTOR: StaticCell<Executor> = StaticCell::new();

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const CERT: &'static str = concat!(include_str!("../secrets/AmazonRootCA1.pem"), "\0");
const CLIENT_CERT: &'static str = concat!(include_str!("../secrets/VendingMachine.pem.crt"), "\0");
const PRIVATE_KEY: &'static str = concat!(include_str!("../secrets/VendingMachine-private.pem.key"), "\0");
const ENDPOINT: &'static str = include_str!("../secrets/endpoint.txt");
const CLIENT_ID: &'static str = include_str!("../secrets/client_id.txt");

macro_rules! singleton {
    ($val:expr) => {{
        type T = impl Sized;
        static STATIC_CELL: StaticCell<T> = StaticCell::new();
        let (x,) = STATIC_CELL.init(($val,));
        x
    }};
}

#[entry]
fn main() -> ! {
    let peripherals = Peripherals::take();

    let mut system = peripherals.SYSTEM.split();
    let clocks = ClockControl::configure(system.clock_control, CpuClock::Clock240MHz).freeze();
    let mut rtc = Rtc::new(peripherals.RTC_CNTL);

    let timer1 = TimerGroup::new(
        peripherals.TIMG1,
        &clocks,
        &mut system.peripheral_clock_control,
    )
    .timer0;

    let timer0 = TimerGroup::new(
        peripherals.TIMG0,
        &clocks,
        &mut system.peripheral_clock_control,
    )
    .timer0;

    rtc.swd.disable();
    rtc.rwdt.disable();

    let init = initialize(
        EspWifiInitFor::Wifi,
        timer1,
        Rng::new(peripherals.RNG),
        system.radio_clock_control,
        &clocks,
    )
    .expect("Failed to initialize Wifi");

    embassy::init(
        &clocks,
        timer0,
    );

    let io = IO::new(peripherals.GPIO, peripherals.IO_MUX);
    let delay = Delay::new(&clocks);
    
    let (wifi, _) = peripherals.RADIO.split();
    let (wifi_interface, controller) =
        match esp_wifi::wifi::new_with_mode(&init, wifi, WifiMode::Sta) {
            Ok((wifi_interface, controller)) => (wifi_interface, controller),
            Err(..) => panic!("WiFi mode Error!"),
        };

    let sclk = io.pins.gpio7;
    let mosi = io.pins.gpio6;
    let mut backlight = io.pins.gpio45.into_push_pull_output();

    backlight.set_high().unwrap();

    let spi = spi::Spi::new_no_cs_no_miso(
        peripherals.SPI2,
        sclk,
        mosi,
        60u32.MHz(),
        spi::SpiMode::Mode0,
        &mut system.peripheral_clock_control,
        &clocks,
    );
    
    let di = SPIInterfaceNoCS::new(spi, io.pins.gpio4.into_push_pull_output());
    let reset = io.pins.gpio48.into_push_pull_output();
    let mut delay = Delay::new(&clocks);

    let mut display = mipidsi::Builder::ili9342c_rgb565(di)
        .with_display_size(320, 240)
        .with_orientation(Orientation::PortraitInverted(false))
        .with_color_order(ColorOrder::Bgr)
        .init(&mut delay, Some(reset))
        .unwrap();

    display.clear(Rgb565::WHITE).unwrap();

    ui::build_ui(&mut display);
    
    let i2c = I2C::new(
        peripherals.I2C0,
        io.pins.gpio41,
        io.pins.gpio40,
        100u32.kHz(),
        &mut system.peripheral_clock_control,
        &clocks,
    );

    let config = Config::dhcpv4(Default::default());

    let seed = 1234;

    let stack = &*singleton!(Stack::new(
        wifi_interface,
        config,
        singleton!(StackResources::<3>::new()),
        seed
    ));

    interrupt::enable(Interrupt::I2C_EXT0, interrupt::Priority::Priority1)
        .expect("Invalid Interrupt Priority Error");

    let executor = EXECUTOR.init(Executor::new());
    executor.run(|spawner| {
        spawner.spawn(connection(controller)).ok();
        spawner.spawn(net_task(&stack)).ok();
        spawner.spawn(task(&stack, i2c, delay)).ok();
    });
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.get_capabilities());
    loop {
        match esp_wifi::wifi::get_wifi_state() {
            WifiState::StaConnected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: SSID.into(),
                password: PASSWORD.into(),
                ..Default::default()
            });

            match controller.set_configuration(&client_config) {
                Ok(()) => {}
                Err(e) => {
                    println!("Failed to connect to wifi: {e:?}");
                    continue;
                }
            }
            println!("Starting wifi");
            match controller.start().await {
                Ok(()) => {}
                Err(e) => {
                    println!("Failed to connect to wifi: {e:?}");
                    continue;
                }
            }
            println!("Wifi started!");
        }
        println!("About to connect...");

        match controller.connect().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<WifiDevice<'static>>) {
    stack.run().await
}

#[embassy_executor::task]
async fn task(stack: &'static Stack<WifiDevice<'static>>, i2c: I2C<'static, I2C0>, mut delay:Delay) {

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    //wait until wifi connected
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address); //dhcp IP address
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    loop {
        Timer::after(Duration::from_millis(1_000)).await;

        let mut socket = TcpSocket::new(&stack, &mut rx_buffer, &mut tx_buffer);

        socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));

        let address = match stack
            .dns_query(ENDPOINT, DnsQueryType::A)
            .await
            .map(|a| a[0])
        {
            Ok(address) => address,
            Err(e) => {
                println!("DNS lookup error: {e:?}");
                continue;
            }
        };

        let remote_endpoint = (address, 8883);
        println!("connecting...");
        let connection = socket.connect(remote_endpoint).await;
        if let Err(e) = connection {
            println!("connect error: {:?}", e);
            continue;
        }
        println!("connected!");

        set_debug(0);

        let certificates = Certificates {
            ca_chain: X509::pem(CERT.as_bytes(),
            )
            .ok(),
            certificate: X509::pem(CLIENT_CERT.as_bytes())
                .ok(),
            private_key: X509::pem(PRIVATE_KEY.as_bytes())
                .ok(),
            password: None,
        };

        let tls: Session<_, 4096> = Session::new(
            &mut socket,
            ENDPOINT,
            Mode::Client,
            TlsVersion::Tls1_3,
            certificates,
        )
        .unwrap();

        println!("Start tls connect");

        let connected_tls = tls.connect().await.expect("TLS connect failed");
    
        println!("Tls connected!");

        let mut config = ClientConfig::new(
            rust_mqtt::client::client_config::MqttVersion::MQTTv5,
            CountingRng(20000),
        );
        config.add_max_subscribe_qos(rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1);
        config.add_client_id(CLIENT_ID);
        config.max_packet_size = 149504;
        let mut recv_buffer = [0; 4096];
        let mut write_buffer = [0; 4096];

        let mut client =
            MqttClient::<_, 5, _>::new(connected_tls, &mut write_buffer, 4096, &mut recv_buffer, 4096, config);

        match client.connect_to_broker().await {
            Ok(()) => {}
            Err(mqtt_error) => match mqtt_error {
                ReasonCode::NetworkError => {
                    println!("MQTT Network Error");
                    continue;
                }
                _ => {
                    println!("Other MQTT Error: {:?}", mqtt_error);
                    continue;
                }
            },
        }

        //initialize BME680
        let mut bme = Bme680::init(i2c, &mut delay, I2CAddress::Primary).expect("Failed to initialize Bme680");
        let settings = SettingsBuilder::new()
            .with_humidity_oversampling(OversamplingSetting::OS2x)
            .with_pressure_oversampling(OversamplingSetting::OS4x)
            .with_temperature_oversampling(OversamplingSetting::OS8x)
            .with_temperature_filter(IIRFilterSize::Size3)
            .with_gas_measurement(CoreDuration::from_millis(1500), 320, 25)
            .with_run_gas(true)
            .build();
        bme.set_sensor_settings(&mut delay, settings).expect("Failed to set the settings");

        loop {
            bme.set_sensor_mode(&mut delay, PowerMode::ForcedMode).expect("Failed to set sensor mode");

            let profile_duration = bme.get_profile_dur(&settings.0).expect("Failed to get profile duration");
            let duration_ms = profile_duration.as_millis() as u32;
            delay.delay_ms(duration_ms);

            let (data, _state) = bme.get_sensor_data(&mut delay).expect("Failed to get sensor data");
            
            let temp = data.temperature_celsius();
            let hum = data.humidity_percent();
            let pres = data.pressure_hpa();
            let gas = data.gas_resistance_ohm();

            println!("|========================|");
            println!("| Temperature {:.2}°C    |", temp);
            println!("| Humidity {:.2}%        |", hum);
            println!("| Pressure {:.2}hPa     |", pres);
            println!("| Gas Resistance {:.2}Ω ", gas);
            println!("|========================|");

            // ui::temperature::update_temperature(&mut display, temp);
            // ui::humidity::update_humidity(&mut display, hum);
            // ui::pressure::update_pressure(&mut display, pres);

            // Convert data into Strings
            let mut temperature_string: String<32> = String::new();
            write!(temperature_string, "{:.2}", temp).expect("write! failed!");

            let mut pressure_string: String<32> = String::new();
            write!(pressure_string, "{:.2}", pres).expect("write! failed!");

            let mut humidity_string: String<32> = String::new();
            write!(humidity_string, "{:.2}", hum).expect("write! failed!");

            let mut gas_string: String<32> = String::new();
            write!(gas_string, "{:.2}", gas).expect("write! failed!");

            match client
                .send_message(
                    "Temperature",
                    temperature_string.as_bytes(),
                    rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1,
                    true,
                )
                .await
            {
                Ok(()) => {}
                Err(mqtt_error) => match mqtt_error {
                    ReasonCode::NetworkError => {
                        println!("MQTT Network Error");
                        continue;
                    }
                    _ => {
                        println!("Other MQTT Error: {:?}", mqtt_error);
                        continue;
                    }
                },
            }

            match client
                .send_message(
                    "Pressure",
                    pressure_string.as_bytes(),
                    rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1,
                    true,
                )
                .await
            {
                Ok(()) => {}
                Err(mqtt_error) => match mqtt_error {
                    ReasonCode::NetworkError => {
                        println!("MQTT Network Error");
                        continue;
                    }
                    _ => {
                        println!("Other MQTT Error: {:?}", mqtt_error);
                        continue;
                    }
                },
            }

            match client
                .send_message(
                    "Humidity",
                    humidity_string.as_bytes(),
                    rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1,
                    true,
                )
                .await
            {
                Ok(()) => {}
                Err(mqtt_error) => match mqtt_error {
                    ReasonCode::NetworkError => {
                        println!("MQTT Network Error");
                        continue;
                    }
                    _ => {
                        println!("Other MQTT Error: {:?}", mqtt_error);
                        continue;
                    }
                },
            }

            match client
                .send_message(
                    "Gas",
                    gas_string.as_bytes(),
                    rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1,
                    true,
                )
                .await
            {
                Ok(()) => {}
                Err(mqtt_error) => match mqtt_error {
                    ReasonCode::NetworkError => {
                        println!("MQTT Network Error");
                        continue;
                    }
                    _ => {
                        println!("Other MQTT Error: {:?}", mqtt_error);
                        continue;
                    }
                },
            }

            Timer::after(Duration::from_millis(3000)).await;
        }
    }
}

pub async fn sleep(millis: u32) {
    Timer::after(Duration::from_millis(millis as u64)).await;
}