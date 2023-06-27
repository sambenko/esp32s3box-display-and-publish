#![no_std]
#![no_main]

use core::time::Duration;

use embedded_graphics::{
    pixelcolor::Rgb565, prelude::*,
};

use esp32s3_hal::{
    clock::{ClockControl, CpuClock},
    gpio::IO,
    i2c::I2C,
    peripherals::Peripherals,
    prelude::*,
    timer::TimerGroup,
    Rtc,
    Rng,
    Delay,
    spi
};

mod adapter;

use embedded_nal::TcpClientStack;

use embedded_svc::{
    ipv4::Interface,
    wifi::{ClientConfiguration, Configuration, Wifi},
};

use esp_wifi::{
    current_millis,
    initialize,
    EspWifiInitFor,
    wifi::{utils::create_network_interface, WifiMode},
    wifi_interface::WifiStack,
};

use esp_backtrace as _;
use esp_println::println;

use display_interface_spi::SPIInterfaceNoCS;
use mipidsi::{ColorOrder, Orientation};

extern crate ui;

use smoltcp::iface::SocketStorage;

use esp_mbedtls::X509;
use esp_mbedtls::Certificates;

use minimq::{Minimq, Publication};

use bme680::*;

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const CERT: &'static str = concat!(include_str!("../secrets/AmazonRootCA1.pem"), "\0");
const CLIENT_CERT: &'static str = concat!(include_str!("../secrets/device-certificate.pem.crt"), "\0");
const PRIVATE_KEY: &'static str = concat!(include_str!("../secrets/private.pem.key"), "\0");
const ENDPOINT: &'static str = include_str!("../secrets/endpoint.txt");

#[entry]
fn main() -> ! {

    let peripherals = Peripherals::take();
    let mut system = peripherals.SYSTEM.split();
    let clocks = ClockControl::configure(system.clock_control, CpuClock::Clock240MHz).freeze();

    let timer_group = TimerGroup::new(
        peripherals.TIMG1,
        &clocks,
        &mut system.peripheral_clock_control,
    );
    let timer = timer_group.timer0;

    let mut wdt = timer_group.wdt;
    let mut rtc = Rtc::new(peripherals.RTC_CNTL);

    // Disable the RTC and TIMG watchdog timers
    wdt.disable();
    rtc.rwdt.disable();
    
    let io = IO::new(peripherals.GPIO, peripherals.IO_MUX);

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

    let rngp = Rng::new(peripherals.RNG);

    let init = initialize(
        EspWifiInitFor::Wifi,
        timer,
        rngp,
        system.radio_clock_control,
        &clocks,
    )
    .unwrap();

    let (wifi, _) = peripherals.RADIO.split();
    let mut socket_set_entries: [SocketStorage; 3] = Default::default();
    let (iface, device, mut controller, sockets) =
        create_network_interface(&init, wifi, WifiMode::Sta, &mut socket_set_entries);
    let wifi_stack = WifiStack::new(iface, device, sockets, current_millis);

    println!("Call wifi_connect");
    let client_config = Configuration::Client(ClientConfiguration {
        ssid: SSID.into(),
        password: PASSWORD.into(),
        ..Default::default()
    });

    let res = controller.set_configuration(&client_config);
    println!("wifi_set_configuration returned {:?}", res);

    controller.start().unwrap();
    println!("is wifi started: {:?}", controller.is_started());

    println!("wifi_connect {:?}", controller.connect());

    println!("Wait to get connected");
    loop {
        let res = controller.is_connected();
        match res {
            Ok(connected) => {
                if connected {
                    break;
                }
            }
            Err(err) => {
                println!("{:?}", err);
                loop {}
            }
        }
    }
    println!("{:?}", controller.is_connected());

    let mut local_ip = [0u8; 4];
    println!("Wait to get an ip address");
    loop {
        wifi_stack.work();

        if wifi_stack.is_iface_up() {
            println!("Got ip {:?}", wifi_stack.get_ip_info());
            local_ip.copy_from_slice(&wifi_stack.get_ip_info().unwrap().ip.octets());
            break;
        }
    }

    println!("We are connected!");

    let mut rx_buffer = [0u8; 1536];
    let mut tx_buffer = [0u8; 1536];
    let socket = wifi_stack.get_socket(&mut rx_buffer, &mut tx_buffer);

    let certificates = Certificates {
        certs: Some(X509::pem(CERT.as_bytes()).unwrap()),
        client_cert: Some(X509::pem(CLIENT_CERT.as_bytes()).unwrap()),
        client_key: Some(X509::pem(PRIVATE_KEY.as_bytes()).unwrap()),
        password: None,
    };

    let mut nal = adapter::WifiTcpClientStack::new([adapter::WrappedSocket::new(
        socket,
        ENDPOINT,
        certificates,
    )]);

    println!("Endpoint: {}", ENDPOINT);

    let mut s = nal.socket().unwrap();

    println!("Start tls connect");

    nal.connect(
        &mut s,
        embedded_nal::SocketAddr::V4(embedded_nal::SocketAddrV4::new(
            embedded_nal::Ipv4Addr::new(3, 124, 161, 238),
            8883,
        )),
    )
    .unwrap();

    println!("Tls connected. Initializing MQTT client");

    nal.close(s).unwrap();

    println!("Socket closed.");

    // Create a new peripheral object with the described wiring
    // and standard I2C clock speed
    let i2c = I2C::new(
        peripherals.I2C0,
        io.pins.gpio41,
        io.pins.gpio40,
        100u32.kHz(),
        &mut system.peripheral_clock_control,
        &clocks,
    );

    let mut bme = Bme680::init(i2c, &mut delay, I2CAddress::Primary).expect("Failed to initialize Bme680");
    let settings = SettingsBuilder::new()
        .with_humidity_oversampling(OversamplingSetting::OS2x)
        .with_pressure_oversampling(OversamplingSetting::OS4x)
        .with_temperature_oversampling(OversamplingSetting::OS8x)
        .with_temperature_filter(IIRFilterSize::Size3)
        .with_gas_measurement(Duration::from_millis(1500), 320, 25)
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

        ui::temperature::update_temperature(&mut display, temp);
        ui::humidity::update_humidity(&mut display, hum);
        ui::pressure::update_pressure(&mut display, pres);

        delay.delay_ms(10000u32);
    }
}
