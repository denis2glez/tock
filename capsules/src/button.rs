//! Provides userspace control of buttons on a board.
//!
//! This allows for much more cross platform controlling of buttons without
//! having to know which of the GPIO pins exposed across the syscall interface
//! are buttons.
//!
//! Usage
//! -----
//!
//! ```rust
//! # use kernel::static_init;
//!
//! let button_pins = static_init!(
//!     [&'static sam4l::gpio::GPIOPin; 1],
//!     [&sam4l::gpio::PA[16]]);
//! let button = static_init!(
//!     capsules::button::Button<'static>,
//!     capsules::button::Button::new(button_pins, board_kernel.create_grant(&grant_cap)));
//! for btn in button_pins.iter() {
//!     btn.set_client(button);
//! }
//! ```
//!
//! Syscall Interface
//! -----------------
//!
//! - Stability: 2 - Stable
//!
//! ### Command
//!
//! Enable or disable button interrupts and read the current button state.
//!
//! #### `command_num`
//!
//! - `0`: Driver check and get number of buttons on the board.
//! - `1`: Enable interrupts for a given button. This will enable both press
//!   and depress events.
//! - `2`: Disable interrupts for a button. No affect or reliance on
//!   registered callback.
//! - `3`: Read the current state of the button.
//!
//! ### Subscribe
//!
//! Setup a callback for button presses.
//!
//! #### `subscribe_num`
//!
//! - `0`: Set callback for pin interrupts. Note setting this callback has
//!   no reliance on individual pins being configured as interrupts. The
//!   interrupt will be called with two parameters: the index of the button
//!   that triggered the interrupt and the pressed (1) or not pressed (0) state
//!   of the button.

use core::cell::Cell;
use kernel::hil::gpio;
use kernel::hil::gpio::{Configure, Input, InterruptWithValue};
use kernel::{CommandReturn, Driver, ErrorCode, Grant, ProcessId, Upcall};

/// Syscall driver number.
use crate::driver;
pub const DRIVER_NUM: usize = driver::NUM::Button as usize;

/// This capsule keeps track for each app of which buttons it has a registered
/// interrupt for. `SubscribeMap` is a bit array where bits are set to one if
/// that app has an interrupt registered for that button.
pub type SubscribeMap = u32;

/// Manages the list of GPIO pins that are connected to buttons and which apps
/// are listening for interrupts from which buttons.
pub struct Button<'a, P: gpio::InterruptPin<'a>> {
    pins: &'a [(
        &'a gpio::InterruptValueWrapper<'a, P>,
        gpio::ActivationMode,
        gpio::FloatingState,
    )],
    apps: Grant<(Upcall, SubscribeMap)>,
}

impl<'a, P: gpio::InterruptPin<'a>> Button<'a, P> {
    pub fn new(
        pins: &'a [(
            &'a gpio::InterruptValueWrapper<'a, P>,
            gpio::ActivationMode,
            gpio::FloatingState,
        )],
        grant: Grant<(Upcall, SubscribeMap)>,
    ) -> Self {
        for (i, &(pin, _, floating_state)) in pins.iter().enumerate() {
            pin.make_input();
            pin.set_value(i as u32);
            pin.set_floating_state(floating_state);
        }

        Self {
            pins: pins,
            apps: grant,
        }
    }

    fn get_button_state(&self, pin_num: u32) -> gpio::ActivationState {
        let pin = &self.pins[pin_num as usize];
        pin.0.read_activation(pin.1)
    }
}

impl<'a, P: gpio::InterruptPin<'a>> Driver for Button<'a, P> {
    /// Set callbacks.
    ///
    /// ### `subscribe_num`
    ///
    /// - `0`: Set callback for pin interrupts. Note setting this callback has
    ///   no reliance on individual pins being configured as interrupts. The
    ///   interrupt will be called with two parameters: the index of the button
    ///   that triggered the interrupt and the pressed/not pressed state of the
    ///   button.
    fn subscribe(
        &self,
        subscribe_num: usize,
        mut callback: Upcall,
        app_id: ProcessId,
    ) -> Result<Upcall, (Upcall, ErrorCode)> {
        let res = match subscribe_num {
            0 => self
                .apps
                .enter(app_id, |cntr| {
                    core::mem::swap(&mut cntr.0, &mut callback);
                })
                .map_err(|err| err.into()),

            // default
            _ => Err(ErrorCode::NOSUPPORT),
        };

        match res {
            Ok(()) => Ok(callback),
            Err(e) => Err((callback, e)),
        }
    }

    /// Configure interrupts and read state for buttons.
    ///
    /// `data` is the index of the button in the button array as passed to
    /// `Button::new()`.
    ///
    /// All commands greater than zero return `INVAL` if an invalid button
    /// number is passed in.
    ///
    /// ### `command_num`
    ///
    /// - `0`: Driver check and get number of buttons on the board.
    /// - `1`: Enable interrupts for a given button. This will enable both press
    ///   and depress events.
    /// - `2`: Disable interrupts for a button. No affect or reliance on
    ///   registered callback.
    /// - `3`: Read the current state of the button.
    fn command(
        &self,
        command_num: usize,
        data: usize,
        _: usize,
        appid: ProcessId,
    ) -> CommandReturn {
        let pins = self.pins;
        match command_num {
            // return button count
            0 => CommandReturn::success_u32(pins.len() as u32),

            // enable interrupts for a button
            1 => {
                if data < pins.len() {
                    self.apps
                        .enter(appid, |cntr| {
                            cntr.1 |= 1 << data;
                            let _ = pins[data]
                                .0
                                .enable_interrupts(gpio::InterruptEdge::EitherEdge);
                            CommandReturn::success()
                        })
                        .unwrap_or_else(|err| CommandReturn::failure(err.into()))
                } else {
                    CommandReturn::failure(ErrorCode::INVAL) /* impossible button */
                }
            }

            // disable interrupts for a button
            2 => {
                if data >= pins.len() {
                    CommandReturn::failure(ErrorCode::INVAL) /* impossible button */
                } else {
                    let res = self
                        .apps
                        .enter(appid, |cntr| {
                            cntr.1 &= !(1 << data);
                            CommandReturn::success()
                        })
                        .unwrap_or_else(|err| CommandReturn::failure(err.into()));

                    // are any processes waiting for this button?
                    let interrupt_count = Cell::new(0);
                    self.apps.each(|_, cntr| {
                        if cntr.1 & (1 << data) != 0 {
                            interrupt_count.set(interrupt_count.get() + 1);
                        }
                    });

                    // if not, disable the interrupt
                    if interrupt_count.get() == 0 {
                        self.pins[data].0.disable_interrupts();
                    }

                    res
                }
            }

            // read input
            3 => {
                if data >= pins.len() {
                    CommandReturn::failure(ErrorCode::INVAL) /* impossible button */
                } else {
                    let button_state = self.get_button_state(data as u32);
                    CommandReturn::success_u32(button_state as u32)
                }
            }

            // default
            _ => CommandReturn::failure(ErrorCode::NOSUPPORT),
        }
    }
}

impl<'a, P: gpio::InterruptPin<'a>> gpio::ClientWithValue for Button<'a, P> {
    fn fired(&self, pin_num: u32) {
        // Read the value of the pin and get the button state.
        let button_state = self.get_button_state(pin_num);
        let interrupt_count = Cell::new(0);

        // schedule callback with the pin number and value
        self.apps.each(|_, cntr| {
            if cntr.1 & (1 << pin_num) != 0 {
                interrupt_count.set(interrupt_count.get() + 1);
                cntr.0.schedule(pin_num as usize, button_state as usize, 0);
            }
        });

        // It's possible we got an interrupt for a process that has since died
        // (and didn't unregister the interrupt). Lazily disable interrupts for
        // this button if so.
        if interrupt_count.get() == 0 {
            self.pins[pin_num as usize].0.disable_interrupts();
        }
    }
}
