#![no_std]
#![allow(warnings)]

use embedded_graphics::{
    mono_font::MonoTextStyle, pixelcolor::Rgb565, text::Text, Drawable,
    primitives::{ RoundedRectangle, PrimitiveStyleBuilder, Rectangle, Primitive },
    image::Image, 
    prelude::{DrawTarget, Dimensions, Point, RgbColor, WebColors, Size},
};

use core::fmt::Write as FmtWrite;
use profont::PROFONT_18_POINT;
use tinybmp::Bmp;

const POS_X: i32 = 205;
const POS_Y: i32 = 70;

pub fn pressure_icon<D>(display: &mut D)
where 
    D:DrawTarget<Color = Rgb565>+Dimensions {

    let icon_data = include_bytes!("../icons/pressure.bmp");
    let pressure = Bmp::from_slice(icon_data).unwrap();
    Image::new(&pressure, Point::new(POS_X + 7, POS_Y)).draw(display);
    
}

pub fn pressure_field<D>(display: &mut D)
where 
    D:DrawTarget<Color = Rgb565>+Dimensions {

        let style = PrimitiveStyleBuilder::new()
            .stroke_width(5)
            .stroke_color(Rgb565::BLACK)
            .fill_color(Rgb565::CSS_ALICE_BLUE)
            .build();

        RoundedRectangle::with_equal_corners(
            Rectangle::new(Point::new(POS_X, POS_Y + 70), Size::new(80, 35)),
            Size::new(10, 10),
        )
        .into_styled(style)
        .draw(display);
}

pub fn update_pressure<D>(display: &mut D, pres_data: f32)
where 
    D:DrawTarget<Color = Rgb565>+Dimensions {

        let text_style = MonoTextStyle::new(&PROFONT_18_POINT, RgbColor::BLACK);

        let text_position: Point = Point::new(POS_X + 10, POS_Y + 93);
        let mut data_string: heapless::String<16> = heapless::String::new();
        write!(data_string,"{:.1}", pres_data).unwrap();
        
        // By redrawing the field, we clear the data
        pressure_field(display);

        // Draw the new data
        Text::new(
            &data_string,
            text_position, 
            text_style
        )
        .draw(display);
}