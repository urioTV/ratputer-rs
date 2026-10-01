use rand_core::{CryptoRng, UnwrapErr};
#[allow(unused_imports)]
use {
    crate::error::{Error, Result, TrapBug},
    log::{debug, error, info, log, trace, warn},
};

pub(crate) fn rng() -> impl CryptoRng {
    UnwrapErr(getrandom::SysRng)
}

pub fn fill_random(buf: &mut [u8]) -> Result<(), Error> {
    getrandom::fill(buf).map_err(|_| Error::msg("RNG failed"))
}
