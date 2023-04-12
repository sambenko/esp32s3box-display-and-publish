#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;
use embedded_graphics::{
    mono_font::MonoTextStyle, pixelcolor::Rgb565, prelude::*, text::Text, Drawable,
    primitives::RoundedRectangle,
};

use embedded_graphics_framebuf::FrameBuf;

use esp32s3_hal::{
    clock::{ClockControl, CpuClock}, 
    peripherals::Peripherals, prelude::*, timer::TimerGroup, spi, Rtc, Rng, IO, Delay,
};

use esp_wifi::esp_now::{PeerInfo, BROADCAST_ADDRESS};
use esp_wifi::{current_millis, initialize};

use esp_backtrace as _;
use esp_println::println;

use display_interface_spi::SPIInterfaceNoCS;
use mipidsi::{ColorOrder, Orientation};
use profont::PROFONT_24_POINT;


fn make_bits(bytes :&[u8]) -> u32 {
    ((bytes[0] as u32) << 24)
        | ((bytes[1] as u32) << 16)
        | ((bytes[2] as u32) << 8)
        | 0
}


#[entry]
fn main() -> ! {

    let peripherals = Peripherals::take();
    let mut system = peripherals.SYSTEM.split();

    let clocks = ClockControl::configure(system.clock_control, CpuClock::Clock240MHz).freeze();

    // Disable the RTC and TIMG watchdog timers
    let mut rtc = Rtc::new(peripherals.RTC_CNTL);

    rtc.swd.disable();
    rtc.rwdt.disable();

    let timg1 = TimerGroup::new(peripherals.TIMG1, &clocks);
    initialize(timg1.timer0, Rng::new(peripherals.RNG), system.radio_clock_control, &clocks).unwrap();
    
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

    let mut data = [Rgb565::WHITE; 320 * 240];
    let mut fbuf = FrameBuf::new(&mut data, 320, 240);
    display.clear(Rgb565::WHITE).unwrap();
    let text_style = MonoTextStyle::new(&PROFONT_24_POINT, RgbColor::BLACK);

    Text::new("Temperature: ", Point::new(10, 25), text_style)
        .draw(&mut display)
        .unwrap();

    let (wifi, _) = peripherals.RADIO.split();
    let mut esp_now = esp_wifi::esp_now::EspNow::new(wifi).unwrap();
    println!("esp-now version {}", esp_now.get_version().unwrap());

    let mut temperature: heapless::String<16> = heapless::String::new();

    let mut next_send_time = current_millis() + 5 * 1000;
    
    loop {
        let r = esp_now.receive();
        if let Some(r) = r {
            fbuf.clear(Rgb565::WHITE).unwrap();
            let bits: u32 = make_bits(r.get_data());
            println!("Received {:.1}°C ", f32::from_bits(bits));
            write!(temperature,"{:.1}°C", f32::from_bits(bits)).unwrap();
            Text::new("Temperature: ", Point::new(10, 25), text_style)
                .draw(&mut fbuf)
                .unwrap();
            Text::new(&temperature, Point::new(210, 28), text_style)
                .draw(&mut fbuf)
                .unwrap();
            temperature.clear();
            display.draw_iter(fbuf.into_iter()).unwrap();

            if r.info.dst_address == BROADCAST_ADDRESS {
                if !esp_now.peer_exists(&r.info.src_address).unwrap() {
                    esp_now
                        .add_peer(PeerInfo {
                            peer_address: r.info.src_address,
                            lmk: None,
                            channel: None,
                            encrypt: false,
                        })
                        .unwrap();
                }
                esp_now.send(&r.info.src_address, b"Received, Thanks!").unwrap();
            }
        }
        if current_millis() >= next_send_time {
            next_send_time = current_millis() + 5 * 5000;
            println!("Send");
            esp_now.send(&BROADCAST_ADDRESS, b"0123456789").unwrap();
        }
    }
}
