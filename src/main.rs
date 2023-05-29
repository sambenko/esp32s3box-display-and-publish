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

use embedded_svc::{
    ipv4::Interface,
    wifi::{ClientConfiguration, Configuration, Wifi},
};

use esp_wifi::{
    current_millis,
    initialize,
    wifi::{utils::create_network_interface, WifiMode},
    wifi_interface::WifiStack,
};

use esp_backtrace as _;
use esp_println::println;

use display_interface_spi::SPIInterfaceNoCS;
use mipidsi::{ColorOrder, Orientation};

use ui::{ build_ui, update_data};

use smoltcp::{
    iface::SocketStorage, 
    wire::{ IpAddress, Ipv4Address }
};
use esp_mbedtls::{ Mode, TlsVersion };
use esp_mbedtls::{ Certificates, Session };

use bme680::*;

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const CERT: &'static str = concat!(include_str!("../certs/AmazonRootCA1.pem"), "\0");
const CLIENT_CERT: &'static str = concat!(include_str!("../certs/device-certificate.pem.crt"), "\0");
const PRIVATE_KEY: &'static str = concat!(include_str!("../certs/private.pem.key"), "\0");

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

    build_ui(&mut display);

    let (wifi, _) = peripherals.RADIO.split();
    let mut socket_set_entries: [SocketStorage; 3] = Default::default();
    let (iface, device, mut controller, sockets) =
        create_network_interface(wifi, WifiMode::Sta, &mut socket_set_entries);
    let wifi_stack = WifiStack::new(iface, device, sockets, current_millis);

    let rngp = Rng::new(peripherals.RNG);
    

    initialize(
        timer,
        rngp,
        system.radio_clock_control,
        &clocks,
    )
    .unwrap();

    println!("Call wifi_connect");
    let client_config = Configuration::Client(ClientConfiguration {
        ssid: SSID.into(),
        password: PASSWORD.into(),
        ..Default::default()
    });

    controller.set_configuration(&client_config).unwrap();
    controller.start().unwrap();
    controller.connect().unwrap();

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

    println!("Wait to get an ip address");
    loop {
        wifi_stack.work();

        if wifi_stack.is_iface_up() {
            println!("Got ip {:?}", wifi_stack.get_ip_info());
            break;
        }
    }

    println!("We are connected!");

    println!("Making HTTP request");
    let mut rx_buffer = [0u8; 1536];
    let mut tx_buffer = [0u8; 1536];
    let mut socket = wifi_stack.get_socket(&mut rx_buffer, &mut tx_buffer);

    socket.work();

    socket
        .open(IpAddress::Ipv4(Ipv4Address::new(52, 28, 41, 87)), 8883)
        .unwrap();

    let certificates = Certificates {
        certs: Some(CERT),
        client_cert: Some(CLIENT_CERT),
        client_key: Some(PRIVATE_KEY),
        password: None,
    };

    let tls = Session::new(
        socket,
        "a3j3y1mdtdmkz5-ats.iot.eu-central-1.amazonaws.com",
        Mode::Client,
        TlsVersion::Tls1_2,
        certificates,
    )
    .unwrap();

    println!("Start tls connect");
    tls.connect().unwrap();

    println!("Tls connected. Standby...");

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

        update_data(&mut display, temp, 54, 24);
        update_data(&mut display, hum, 99, 22);
        update_data(&mut display, pres, 148, 23);

        delay.delay_ms(10000u32);
    }
}
