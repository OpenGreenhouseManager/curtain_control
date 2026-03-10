
pub(crate) type Result<T> = core::result::Result<T, Error>;

#[derive(Debug)]
pub(crate) enum Error {
    NotCalibrated,
    CalibrationFailed,
}

// region:    --- Error Boilerplate
impl core::fmt::Display for Error {
    fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::result::Result<(), core::fmt::Error> {
        write!(fmt, "{self:?}")
    }
}



