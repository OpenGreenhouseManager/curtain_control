#![no_std]

pub mod tcp_client;
pub mod stepper_controll;
pub const RECONNECT_DELAY_MS: u64 = 2_000;
const CLIENT_UUID: &str = "8a3a3b0e-10b0-4f5e-bb14-7eac9ced0001";
