use esp_hal::{gpio::{Output, Pin}, peripherals::Peripherals};

pub struct StepperController<'a> {
    stepper_motor: StepperMotor<'a>,
}

impl<'a> StepperController<'a> {
    pub fn new(step_pin: Output<'a>, direction_pin: Output<'a>, enable_pin: Output<'a>) -> Self {
        Self {
            stepper_motor: StepperMotor::new(step_pin, direction_pin, enable_pin),
        }
    }

    pub fn calibrate(&mut self) {
        //self.stepper_motor.calibrate();
    }
}


struct StepperMotor<'a> {
    step_pin: Output<'a>,
    direction_pin: Output<'a>,
    enable_pin: Output<'a>,
}

impl<'a> StepperMotor<'a> {
    pub fn new(step_pin: Output<'a>, direction_pin: Output<'a>, enable_pin: Output<'a>) -> Self {
        Self {
            step_pin: step_pin,
            direction_pin: direction_pin,
            enable_pin: enable_pin,
        }
    }
}