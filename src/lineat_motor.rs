use esp_hal::gpio::{Input, Output};

use crate::error::{Error, Result};

pub struct LinearMotorController<'a> {
    linear_motor: Motor<'a>,
    state: Option<u8>,
}

impl<'a> LinearMotorController<'a> {
    pub fn new(end_point: Input<'a>, a: Output<'a>, b: Output<'a>) -> Self {
        Self {
            linear_motor: Motor::new(end_point, a, b),
            state: None,
        }
    }

    pub fn set_state(&mut self, new_state: u8) -> Result<()> {
        if self.state.is_none() {
            return Err(Error::NotCalibrated);
        }
        self.state = None;
        self.move_to(new_state);
        self.state = Some(new_state);
        Ok(())
    }

    pub fn get_state(&self) -> Option<u8> {
        self.state
    }

    pub fn calibrate(&mut self) -> Result<()> {
        self.state = None;
        self.move_to(u8::MAX);
        self.move_to(0);
        if self.linear_motor.end_point.is_high() {
            self.state = Some(0);
            return Ok(());
        }
        Err(Error::CalibrationFailed)
        //self.stepper_motor.calibrate();
    }

    pub fn move_to(&self, position: u8) {}
}

struct Motor<'a> {
    end_point: Input<'a>,
    a: Output<'a>,
    b: Output<'a>,
}

impl<'a> Motor<'a> {
    pub fn new(end_point: Input<'a>, a: Output<'a>, b: Output<'a>) -> Self {
        Self { end_point, a, b }
    }
}
