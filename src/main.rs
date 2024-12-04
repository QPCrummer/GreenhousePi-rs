//! Blinks the LED on a Pico board
//!
//! This will blink an LED attached to GP25, which is the pin the Pico uses for the on-board LED.
#![no_std]
#![no_main]

use core::time::Duration;
use bme680::{Bme680, FieldData, FieldDataCondition, I2CAddress, IIRFilterSize, OversamplingSetting, PowerMode, SettingsBuilder};
use bsp::entry;
use defmt_rtt as _;
use embedded_hal::digital::OutputPin;
use embedded_hal::i2c::{I2c, Operation};
use panic_probe as _;

// Provide an alias for our BSP so we can switch targets quickly.
// Uncomment the BSP you included in Cargo.toml, the rest of the code does not need to change.
use rp_pico as bsp;
// use sparkfun_pro_micro_rp2040 as bsp;

use bsp::hal::{
    clocks::{init_clocks_and_plls, Clock},
    pac,
    watchdog::Watchdog,
};
use cortex_m::delay::Delay;
use heapless::String;
use lcd1602_driver::command::{DataWidth, State};
use lcd1602_driver::lcd;
use lcd1602_driver::lcd::{Basic, Ext, Lcd};
use lcd1602_driver::sender::ParallelSender;
use rp_pico::hal;
use rp_pico::hal::fugit::RateExtU32;
use rp_pico::hal::gpio::{Function, Pin};
use rp_pico::hal::pio::PIOExt;
use rp_pico::pac::io_bank0::GPIO;
use ufmt::uwrite;

static mut SENDER: Option<ParallelSender<Pin<Output, Dynamic>, Pin<OpenDrain, Dynamic>, Pin<Output, Dynamic>, 4>> = None;
static mut DELAY: Option<Delay> = None;
const FIRE: &str = "Fire Present";

#[entry]
fn main() -> ! {
    // Grab our singleton objects
    let mut pac = pac::Peripherals::take().unwrap();
    let core = pac::CorePeripherals::take().unwrap();

    // Set up the watchdog driver - needed by the clock setup code
    let mut watchdog = Watchdog::new(pac.WATCHDOG);

    // Configure the clocks
    //
    // The default is to generate a 125 MHz system clock
    let clocks = init_clocks_and_plls(
        rp_pico::XOSC_CRYSTAL_FREQ,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
        .ok()
        .unwrap();

    // The single-cycle I/O block controls our GPIO pins
    let sio = hal::Sio::new(pac.SIO);

    // Set the pins up according to their function on this particular board
    let pins = rp_pico::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    let (mut pio, sm0, _, _, _) = pac.PIO0.split(&mut pac.RESETS);

    let mut delay = Delay::new(core.SYST, clocks.system_clock.freq().to_Hz());

    let mut i2c_pio = i2c_pio::I2C::new(
        &mut pio,
        pins.gpio8,
        pins.gpio9,
        sm0,
        100.kHz(),
        clocks.system_clock.freq(),
    );

    // Set up BME680
    let mut bme = Bme680::init(i2c_pio, &mut delay, I2CAddress::Primary).unwrap();
    let settings = SettingsBuilder::new()
        .with_humidity_oversampling(OversamplingSetting::OS2x)
        .with_pressure_oversampling(OversamplingSetting::OS4x)
        .with_temperature_oversampling(OversamplingSetting::OS8x)
        .with_temperature_filter(IIRFilterSize::Size3)
        .with_gas_measurement(Duration::from_millis(1500), 320, 25)
        .with_run_gas(true)
        .build();

    bme.set_sensor_settings(&mut delay, settings).unwrap();

    bme.set_sensor_mode(&mut delay, PowerMode::ForcedMode).unwrap();

    // Set up LCD1602
    unsafe {
        // TODO Fix this
        SENDER = Some(ParallelSender::<Pin<Gpio0, Function<Output>>, Pin<Gpio21, Function<OpenDrain>>, Pin<Gpio1, Function<Output>>, 4>::new_4pin(
            pins.gpio0.into_function(),
            pins.gpio21.into_function(),
            pins.gpio1.into_function(),
            pins.gpio2.into_function(),
            pins.gpio3.into_function(),
            pins.gpio4.into_function(),
            pins.gpio5.into_function(),
            None,
        ));

        DELAY = Some(delay);
    }

    let lcd_config = lcd::Config::default().set_data_width(DataWidth::Bit4);
    let mut lcd = Lcd::new(
        unsafe { SENDER.as_mut().unwrap() },
        unsafe { DELAY.as_mut().unwrap() },
        lcd_config,
        10,
    );

    // Set up button up
    let up_button = pins.gpio10.into_pull_up_input();

    // Set up button down
    let down_button = pins.gpio11.into_pull_up_input();

    // Set up button select
    let select_button = pins.gpio12.into_pull_up_input();

    // Set up buzzer
    let mut buzzer = pins.gpio6.into_push_pull_output();

    // Set up smoke detector
    let smoke_detector = pins.gpio7.into_pull_up_input();

    // Set up sprinklers
    let mut sprinklers = pins.gpio13.into_push_pull_output();

    // Set up roof vent
    let mut roof_vent = pins.gpio14.into_push_pull_output();

    let mut current_screen_index: u8 = 0;
    let mut wait_time: u16 = 0;
    let mut data: FieldData = FieldData::default(); // TODO Make sure this is set to a valid value before using it
    let mut preferences: Preferences = Preferences::default();
    // Cooldowns
    let mut button_cooldown: u8 = 50; // 500ms cooldown

    loop {
        delay.delay_ms(10);

        // Tick buttons
        button_cooldown = tick_buttons(button_cooldown);

        let (update_needed, action) = should_update(&up_button, &down_button, &select_button, &mut wait_time, &mut preferences);

        if update_needed {
            match action {
                RefreshAction::UP => {
                    if button_cooldown == 0 {
                        current_screen_index = next_screen(current_screen_index, true);
                        button_cooldown = 50;
                    }
                }
                RefreshAction::DOWN => {
                    if button_cooldown == 0 {
                        current_screen_index = next_screen(current_screen_index, false);
                        button_cooldown = 50;
                    }
                }
                RefreshAction::SELECT => {
                    // Handle SELECT action
                    if button_cooldown == 0 {
                        lcd.clean_display();
                        let mut editing_lower: bool = true;
                        let mut update_date: bool = false;
                        let mut refresh: bool = true;
                        let mut info_str: String<11> = String::new();
                        match current_screen_index {
                            0 => {
                                // Temp
                                for _ in 0..2 {
                                    loop {
                                        if refresh {
                                            uwrite!(&mut info_str, "{} - {}", preferences.temperature.0, preferences.temperature.1).unwrap(); // Max str size 7
                                            render_edit_screen(&info_str, editing_lower, &mut lcd);
                                            refresh = false;
                                        }

                                        delay.delay_ms(500);

                                        if update_date {
                                            preferences.tick_time();
                                        }
                                        update_date = !update_date;

                                        if up_button.is_high() {
                                            if editing_lower {
                                                if preferences.temperature.0 < 1 {
                                                    preferences.temperature.0 += 1;
                                                }
                                            } else {
                                                if preferences.temperature.1 < 1 {
                                                    preferences.temperature.1 += 1;
                                                }
                                            }
                                            refresh = true;
                                        } else if down_button.is_high() {
                                            if editing_lower {
                                                if preferences.temperature.0 > 0 {
                                                    preferences.temperature.0 -= 1;
                                                }
                                            } else {
                                                if preferences.temperature.1 > 0 {
                                                    preferences.temperature.1 -= 1;
                                                }
                                            }
                                            refresh = true;
                                        } else if select_button.is_high() {
                                            editing_lower = false;
                                            lcd.set_cursor_blink_state(State::Off);
                                            refresh = true;
                                            break;
                                        }
                                    }
                                }
                                // Check legality
                                if preferences.temperature.0 > preferences.temperature.1 {
                                    let temp = preferences.temperature.0;
                                    preferences.temperature.0 = preferences.temperature.1;
                                    preferences.temperature.1 = temp;
                                }
                            }
                            1 => {
                                // Humidity
                                for _ in 0..2 {
                                    loop {
                                        if refresh {
                                            uwrite!(&mut info_str, "{}% - {}%", preferences.humidity.0, preferences.humidity.1).unwrap(); // Max str size 11
                                            render_edit_screen(&info_str, editing_lower, &mut lcd);
                                            refresh = false;
                                        }

                                        delay.delay_ms(500);

                                        if update_date {
                                            preferences.tick_time();
                                        }
                                        update_date = !update_date;

                                        if up_button.is_high() {
                                            if editing_lower {
                                                if preferences.humidity.0 < 100 {
                                                    preferences.humidity.0 += 1;
                                                }
                                            } else {
                                                if preferences.humidity.1 < 100 {
                                                    preferences.humidity.1 += 1;
                                                }
                                            }
                                            refresh = true;
                                        } else if down_button.is_high() {
                                            if editing_lower {
                                                if preferences.humidity.0 > 0 {
                                                    preferences.humidity.0 -= 1;
                                                }
                                            } else {
                                                if preferences.humidity.1 > 0 {
                                                    preferences.humidity.1 -= 1;
                                                }
                                            }
                                            refresh = true;
                                        } else if select_button.is_high() {
                                            editing_lower = false;
                                            lcd.set_cursor_blink_state(State::Off);
                                            refresh = true;
                                            break;
                                        }
                                    }
                                }
                                // Check legality
                                if preferences.humidity.0 > preferences.humidity.1 {
                                    let temp = preferences.humidity.0;
                                    preferences.humidity.0 = preferences.humidity.1;
                                    preferences.humidity.1 = temp;
                                }
                            },
                            3 => {
                                // Date

                                // Minute
                                loop {
                                    if refresh {
                                        uwrite!(&mut info_str, "Minute: {}", preferences.date.1).unwrap(); // Max str size 10
                                        render_date_edit_screen(&info_str, &mut lcd);
                                        refresh = false;
                                    }

                                    delay.delay_ms(500);

                                    if update_date {
                                        preferences.tick_time();
                                    }
                                    update_date = !update_date;

                                    if up_button.is_high() {
                                        preferences.date.1 = (preferences.date.1 + 1) % 60;
                                        refresh = true;
                                    } else if down_button.is_high() {
                                        preferences.date.1 = (preferences.date.1 + 59) % 60;
                                        refresh = true;
                                    } else if select_button.is_high() {
                                        refresh = true;
                                        break;
                                    }
                                }

                                // Hour
                                loop {
                                    if refresh {
                                        uwrite!(&mut info_str, "Hour: {}", preferences.date.2).unwrap(); // Max str size 8
                                        render_date_edit_screen(&info_str, &mut lcd);
                                        refresh = false;
                                    }
                                    delay.delay_ms(500);

                                    if update_date {
                                        preferences.tick_time();
                                    }
                                    update_date = !update_date;

                                    if up_button.is_high() {
                                        preferences.date.2 = (preferences.date.2 + 1) % 24;
                                        refresh = true;
                                    } else if down_button.is_high() {
                                        preferences.date.2 = (preferences.date.2 + 23) % 24;
                                        refresh = true;
                                    } else if select_button.is_high() {
                                        refresh = true;
                                        break;
                                    }
                                }

                                // Day
                                loop {
                                    if refresh {
                                        uwrite!(&mut info_str, "Day: {}", preferences.date.3).unwrap(); // Max str size 7
                                        render_date_edit_screen(&info_str, &mut lcd);
                                        refresh = false;
                                    }
                                    delay.delay_ms(500);

                                    if update_date {
                                        preferences.tick_time();
                                    }
                                    update_date = !update_date;

                                    if up_button.is_high() {
                                        preferences.date.3 = preferences.change_days(true);
                                        refresh = true;
                                    } else if down_button.is_high() {
                                        preferences.date.3 = preferences.change_days(false);
                                        refresh = true;
                                    } else if select_button.is_high() {
                                        refresh = true;
                                        break;
                                    }
                                }

                                // Month
                                // TODO Changing this will for sure break the day counter...
                                // TODO But I couldn't care less :)
                                loop {
                                    if refresh {
                                        uwrite!(&mut info_str, "Month: {}", preferences.date.4).unwrap(); // Max str size 9
                                        render_date_edit_screen(&info_str, &mut lcd);
                                        refresh = false;
                                    }
                                    delay.delay_ms(500);

                                    if update_date {
                                        preferences.tick_time();
                                    }
                                    update_date = !update_date;

                                    if up_button.is_high() {
                                        preferences.date.4 = (preferences.date.4 + 1) % 12;
                                        refresh = true;
                                    } else if down_button.is_high() {
                                        preferences.date.4 = (preferences.date.4 + 11) % 12;
                                        refresh = true;
                                    } else if select_button.is_high() {
                                        refresh = true;
                                        break;
                                    }
                                }

                                // Year
                                loop {
                                    if refresh {
                                        uwrite!(&mut info_str, "Year: {}", preferences.date.5).unwrap(); // Max str size 10
                                        render_date_edit_screen(&info_str, &mut lcd);
                                        refresh = false;
                                    }
                                    delay.delay_ms(500);

                                    if update_date {
                                        preferences.tick_time();
                                    }
                                    update_date = !update_date;

                                    if up_button.is_high() {
                                        // I'm going to assume that no one is stupid enough
                                        // to actually hit the u16 integer limit
                                        preferences.date.5 += 1;
                                        refresh = true;
                                    } else if down_button.is_high() {
                                        if preferences.date.5 != 0 {
                                            preferences.date.5 -= 1;
                                        }
                                        refresh = true;
                                    } else if select_button.is_high() {
                                        refresh = true;
                                        break;
                                    }
                                }

                                lcd.set_cursor_blink_state(State::Off);
                            }
                            4 => {
                                let mut remove: bool = false;
                                for index in 0..4 {
                                    loop {
                                        if refresh {
                                            render_edit_screen(&preferences.format_watering_time(), index < 2, &mut lcd);
                                            refresh = false;
                                        }

                                        delay.delay_ms(500);

                                        if update_date {
                                            preferences.tick_time();
                                        }
                                        update_date = !update_date;

                                        if up_button.is_high().unwrap() && down_button.is_high().unwrap() {
                                            remove = true;
                                            break;
                                        }

                                        if up_button.is_high() {
                                            if preferences.watering.is_none() {
                                                preferences.set_default_watering_time();
                                            } else {
                                                match index {
                                                    0 => {
                                                        preferences.watering.unwrap().1 = (preferences.watering.unwrap().1 + 1) % 24;
                                                    }
                                                    1 => {
                                                        preferences.watering.unwrap().0 = (preferences.watering.unwrap().0 + 1) % 60;
                                                    }
                                                    2 => {
                                                        preferences.watering.unwrap().3 = (preferences.watering.unwrap().3 + 1) % 24;
                                                    }
                                                    3 => {
                                                        preferences.watering.unwrap().2 = (preferences.watering.unwrap().2 + 1) % 60;
                                                    }
                                                    _ => {}
                                                }
                                            }
                                            refresh = true;
                                        } else if down_button.is_high() {
                                            if preferences.watering.is_none() {
                                                preferences.set_default_watering_time();
                                            } else {
                                                match index {
                                                    0 => {
                                                        preferences.watering.unwrap().1 = (preferences.watering.unwrap().1 + 23) % 24;
                                                    }
                                                    1 => {
                                                        preferences.watering.unwrap().0 = (preferences.watering.unwrap().0 + 59) % 60;
                                                    }
                                                    2 => {
                                                        preferences.watering.unwrap().3 = (preferences.watering.unwrap().3 + 23) % 24;
                                                    }
                                                    3 => {
                                                        preferences.watering.unwrap().2 = (preferences.watering.unwrap().2 + 59) % 60;
                                                    }
                                                    _ => {}
                                                }
                                            }
                                            refresh = true;
                                        } else if select_button.is_high() {
                                            refresh = true;
                                            break;
                                        }
                                    }
                                    if remove {
                                        break;
                                    }
                                }
                                // Check legality
                                if !remove {
                                    if (preferences.watering.unwrap().1 > preferences.watering.unwrap().3) || // Hours are incorrect
                                        (preferences.watering.unwrap().1 == preferences.watering.unwrap().3 && // Minutes are incorrect assuming hours are equal
                                            preferences.watering.unwrap().0 > preferences.watering.unwrap().2) {
                                        preferences.watering = Some((preferences.watering.unwrap().2, preferences.watering.unwrap().3, preferences.watering.unwrap().0, preferences.watering.unwrap().1));
                                    }
                                }
                            }
                            _ => {
                                // Pressure has no configuration
                            }
                        }
                    }
                }
                _ => {
                    if smoke_detector.is_high() {
                        // Panic!!!
                        let roof_open = &roof_vent.is_set_high();
                        render_screen(FIRE, true, &mut lcd);
                        while smoke_detector.is_high() {
                            // Enable sprinklers
                            sprinklers.set_high();
                            // Ensure windows are closed
                            roof_vent.set_low();
                            // Sound alarm
                            buzzer.set_high();
                            delay.delay_ms(1000);
                            // Still keep track of time though
                            preferences.tick_time();
                        }
                        // Safe; Disable sprinklers and open vent if it was open before
                        buzzer.set_low();
                        sprinklers.set_low();
                        if *roof_open {
                            roof_vent.set_high();
                        }
                    }

                    data = get_bme_data(&mut bme, &mut delay, &mut buzzer);

                    // Check if temperature is valid
                    let temp = get_temperature(&data);
                    if temp < preferences.temperature.0 || temp > preferences.temperature.1 {
                        // open vent
                        roof_vent.set_high();
                    } else {
                        roof_vent.set_low();
                    }

                    // Check if humidity is valid
                    let humidity = get_humidity(&data);
                    if humidity < preferences.humidity.0 || humidity > preferences.humidity.1 {
                        // enable sprinklers
                        sprinklers.set_high();
                    } else {
                        sprinklers.set_low();
                    }

                    // Check if it is watering time
                    if preferences.is_watering_time() {
                        sprinklers.set_high();
                    } else {
                        sprinklers.set_low();
                    }
                }
            }
        } else {
            continue;
        }

        let mut data_str: String<12> = String::new();
        match current_screen_index {
            0 => { // Temp
                // TODO Something shady is happening with this value
                uwrite!(&mut data_str, "Temp: {}F", get_temperature(&data)).unwrap(); // Str size 9
                render_screen(&data_str, true, &mut lcd);
                uwrite!(&mut data_str, "({}, {})", preferences.temperature.0, preferences.temperature.1).unwrap(); // Str size 8
                render_screen(&data_str, false, &mut lcd);
            }
            1 => { // Humidity
                uwrite!(&mut data_str, "RH: {}%", get_humidity(&data)).unwrap(); // Str size 8
                render_screen(&data_str, true, &mut lcd);
                uwrite!(&mut data_str, "({}%, {}%)", preferences.humidity.0, preferences.humidity.1).unwrap(); // Str size 12
                render_screen(&data_str, false, &mut lcd);
            }
            2 => { // Pressure
                uwrite!(&mut data_str, "PRS: {} mb", get_pressure(&data)).unwrap(); // Str size 12
                render_screen(&data_str, true, &mut lcd);
            }
            3 => { // Date
                let (time, date) = preferences.get_date_formatted();
                render_screen(&time, true, &mut lcd);
                render_screen(&date, false, &mut lcd);
            }
            _ => { // Water Schedule
                render_screen(&preferences.format_watering_time(), true, &mut lcd);
            }
        }
    }
}

/// Gets data from the BME sensor
/// param bme: BME sensor instance
/// param delayer: BME sensor delay
/// param alarm: Buzzer Pin
/// returns FieldData
fn get_bme_data(bme: &mut Bme680<i2c_pio, Delay>, delayer: &mut Delay, alarm: &mut Pin<Output, PB1>) -> FieldData {
    prep_bme(bme, delayer, alarm);
    bme.get_sensor_data(delayer).unwrap_or((FieldData::default(), FieldDataCondition::Unchanged)).0
}

/// Gets temperature in Fahrenheit
/// param data: FieldData from get_bme_data()
fn get_temperature(data: &FieldData) -> u8 {
    (data.temperature_celsius() * (9. / 5.) + 32.) as u8
}

/// Gets percent humidity (whole number)
/// param data: FieldData from get_bme_data()
fn get_humidity(data: &FieldData) -> u8 {
    data.humidity_percent() as u8
}

/// Gets atmospheric pressure in millibars
/// param data: FieldData from get_bme_data()
fn get_pressure(data: &FieldData) -> u16 {
    data.pressure_hpa() as u16
}

/// Sets the sensor's mode to Forced
/// This should be called before getting data
/// If there is an error setting up, an alarm is sounded
/// param bme: BME sensor reference
/// param delayer: BME delay
/// param alarm: Buzzer Pin
fn prep_bme(bme: &mut Bme680<I2c, Delay>, delayer: &mut Delay, alarm: &mut Pin<Output, PB1>) {
    if bme.set_sensor_mode(delayer, PowerMode::ForcedMode).is_err() {
        loop {
            alarm.set_high();
            delayer.delay_ms(500);
            alarm.set_low();
            delayer.delay_ms(1000);
        }
    }
}

/// Basic function for rendering text onto the LCD
/// It only clears the screen when the top line is written to
/// param line: text to render
/// param top_line: if the top line is to be written to
/// param lcd: LCD instance
fn render_screen(line: &str, top_line: bool, lcd: &mut Lcd<'static, 'static, ParallelSender<Pin<Output>, Pin<OpenDrain>, Pin<Output>, 4>, Delay<>>) {
    // Set cursor to the correct line
    if top_line {
        // Reset screen
        lcd.clean_display();
        lcd.set_cursor_pos((0, 0));
    } else {
        lcd.set_cursor_pos((0, 1));
    }
    lcd.write_str_to_cur(line);
}

/// Renders the Preferences on screen with a blinking indicator cursor
/// param line: The preferences line
/// param left_cursor: If the lower bound is selected
/// param lcd: LCD instance
fn render_edit_screen<const N: usize>(line: &String<N>, left_cursor: bool, lcd: &mut Lcd<'static, 'static, ParallelSender<Pin<Output>, Pin<OpenDrain>, Pin<Output>, 4>, Delay<>>) {
    // Clear
    lcd.clean_display();

    // Write top info
    lcd.set_cursor_pos((0, 0));
    lcd.write_str_to_cur(line);

    // Create bottom blinking cursor
    if left_cursor {
        lcd.set_cursor_pos((0, 1));
    } else {
        lcd.set_cursor_pos((15, 1));
    }
    lcd.set_cursor_blink_state(State::On);
}

/// Renders the current date unit (min, hr, day, etc.) on the first line with a central blinking cursor on the second line
/// param line: The date line
/// param lcd: LCD instance
fn render_date_edit_screen<const N: usize>(line: &String<N>, lcd: &mut Lcd<'static, 'static, ParallelSender<Pin<Output>, Pin<OpenDrain>, Pin<Output>, 4>, Delay<>>) {
    // Clear
    lcd.clean_display();

    // Write date segment
    lcd.set_cursor_pos((0, 0));
    lcd.write_str_to_cur(line);

    // Create blinking cursor
    lcd.set_cursor_pos((7, 1));
    lcd.set_cursor_blink_state(State::On);
}

enum RefreshAction {
    UP,
    DOWN,
    SELECT,
    SENSOR,
}

/// Whether to update the LCD
/// param up: Up Button
/// param down: Down Button
/// param select: Selection Button
/// param wait_time: The amount of time between sensor polling
/// param preferences: Client Preferences
/// returns: if the LCD needs an update
fn should_update(up: &Pin<Input<PullUp>, GPIO>, down: &Pin<Input<PullUp>, GPIO1>, select: &Pin<Input<PullUp>, GPIO2>, wait_time: &mut u16, preferences: &mut Preferences) -> (bool, RefreshAction) {
    *wait_time += 1;
    // Make sure time is kept track of
    if *wait_time % 100 == 0 {
        preferences.tick_time();
    }

    // Prioritize button pressing
    if up.is_high() {
        return (true, RefreshAction::UP);
    } else if down.is_high() {
        return (true, RefreshAction::DOWN);
    } else if select.is_high() {
        return (true, RefreshAction::SELECT);
    }

    // Check if sensors need updated
    if *wait_time >= 100 {
        *wait_time = 0; // TODO See if this actually works
        return (true, RefreshAction::SENSOR);
    }
    (false, RefreshAction::SENSOR) // It's ok to return SENSOR since it gets ignored
}

/// Ticks the cooldown for buttons
/// param cooldown: The amount of cooldown left
/// returns the new value for cooldown
fn tick_buttons(mut cooldown: u8) -> u8 {
    if cooldown > 0 {
        cooldown -= 1;
    }
    cooldown
}

/// Iterates forwards or backwards through Screens
/// param current_screen: The current screen being displayed
/// param next: Whether to iterate forward; If false, iterate backwards
/// returns: The next Screen
fn next_screen(mut current_screen_index: u8, next: bool) -> u8 {
    if next {
        current_screen_index = (current_screen_index + 1) % 5;
    } else {
        current_screen_index = (current_screen_index + 5 - 1) % 5;
    }
    current_screen_index
}

pub struct Preferences {
    pub temperature: (u8, u8),
    pub humidity: (u8, u8),
    pub date: (u8, u8, u8, u8, u8, u16), // Sec, Min, Hour, Day, Month, Year
    pub watering: Option<(u8, u8, u8, u8)>, // Start (Min, Hour), End (Min, Hour)
}

impl Default for Preferences {
    fn default() -> Self {
        Preferences {
            temperature: (60, 80), // Ideal range is 60F - 80F
            humidity: (60, 70), // Ideal range is 60% - 70%
            date: (0, 0, 0, 1, 1, 2000), // Date: 00:00:00 Jan 1 2000
            watering: None, // No default watering times set
        }
    }
}

impl Preferences {
    /// Increments by 1 second
    fn tick_time(&mut self) {
        self.date.0 += 1;

        // Check for rollovers
        if self.date.0 >= 60 {
            self.date.1 += self.date.0 / 60;
            self.date.0 = self.date.0 % 60;
        } else {
            return;
        }

        if self.date.1 >= 60 {
            self.date.2 += self.date.1 / 60;
            self.date.1 = self.date.1 % 60;
        } else {
            return;
        }

        if self.date.2 >= 24 {
            self.date.3 += self.date.2 / 24;
            self.date.2 = self.date.2 % 24;
        } else {
            return;
        }

        // Handle month and day rollovers
        loop {
            let days_in_month = self.get_days_in_month();

            if self.date.3 > days_in_month {
                self.date.3 -= days_in_month;
                self.date.4 += 1;
            } else {
                break;
            }

            if self.date.4 > 12 {
                self.date.4 = 1;
                self.date.5 += 1;
            }
        }

        // Update the date tuple
        self.date = (self.date.0, self.date.1, self.date.2, self.date.3, self.date.4, self.date.5);
    }

    /// Gets the date in the HH:MM:SS DD/MM/YYYY format
    /// Since the indexes start at 0 and months and days start at 1,
    /// the function ensures that 1 is added
    /// returns: (HH:MM:SS, DD/MM/YYYY)
    fn get_date_formatted(&mut self) -> (String<8>, String<10>) {
        // Format the date as a string
        let mut val1: String<8> = String::new();
        let mut val2: String<10> = String::new();
        // TODO Find a way to pad numbers <10 with a "0"
        uwrite!(&mut val1, "{}:{}:{}", self.date.2, self.date.1, self.date.0).unwrap();
        uwrite!(&mut val2, "{}/{}/{}", self.date.3 + 1, self.date.4 + 1, self.date.5).unwrap();
        (val1, val2)
    }

    /// Calculates if it is leap year
    /// param year: The current year
    fn is_leap_year(year: u16) -> bool {
        year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
    }

    /// Gets the next index for the current day depending on the month and leap year
    /// param increment: If the values are incrementing (not decrementing)
    /// returns the next day's index
    fn change_days(&self, increment: bool) -> u8 {
        let days_in_month: u8 = self.get_days_in_month();

        if increment {
            (self.date.3 + 1) % days_in_month
        } else {
            (self.date.3 + (days_in_month - 1)) % days_in_month
        }
    }

    /// Gets the amount of days in the current month
    /// returns the amount of days in the month
    fn get_days_in_month(&self) -> u8 {
        match self.date.4 {
            2 => if Self::is_leap_year(self.date.5) { 29 } else { 28 },
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        }
    }

    /// Checks if it is time to enable the sprinklers
    /// returns if the current time is within the watering time
    /// returns false if there is no watering time set
    fn is_watering_time(&self) -> bool {
        if let Some(watering_time) = self.watering {
            self.date.1 >= watering_time.0 && // Minutes are not too small
                self.date.1 <= watering_time.2 && // Minutes are not too large
                self.date.2 >= watering_time.1 && // Hours are not too small
                self.date.2 <= watering_time.3 // Hours are not too large
        } else {
            false
        }
    }

    /// Formats the watering time: HH:MM - HH:MM
    /// Returns a String of length 16 containing the formatted times
    fn format_watering_time(&self) -> String<16> {
        let mut str: String<16> = String::new();
        if let Some(watering_time) = self.watering {
            // TODO Find a way to pad numbers <10 with a "0"
            uwrite!(str, "{}:{} - {}:{}", watering_time.1, watering_time.0, watering_time.3, watering_time.2).unwrap();
        } else {
            uwrite!(str, "None").unwrap();
        }
        str
    }

    /// Sets the watering time from 00:00 to 01:00
    fn set_default_watering_time(&mut self) {
        self.watering = Some((0, 0, 0, 1));
    }
}

// End of file
