//! Process-wide Clock / Entropy install points for Phase 1a threading.
//!
//! `Recorder::launch` installs the config-supplied backends so `util::epoch_ms`
//! / `random_hex` / `random_unit_f64` honor host overrides without threading an
//! `Arc` through every call site. Defaults are [`SystemClock`] / [`SystemEntropy`].

use std::sync::{Arc, RwLock};

use bugsee_platform::{Clock, Entropy, SystemClock, SystemEntropy};

struct Services {
    clock: Arc<dyn Clock>,
    entropy: Arc<dyn Entropy>,
}

fn services() -> &'static RwLock<Services> {
    use std::sync::OnceLock;
    static SERVICES: OnceLock<RwLock<Services>> = OnceLock::new();
    SERVICES.get_or_init(|| {
        RwLock::new(Services {
            clock: Arc::new(SystemClock),
            entropy: Arc::new(SystemEntropy),
        })
    })
}

/// Replace the process-wide clock/entropy used by [`crate::util`].
pub fn install(clock: Arc<dyn Clock>, entropy: Arc<dyn Entropy>) {
    let mut g = services().write().expect("platform services lock");
    g.clock = clock;
    g.entropy = entropy;
}

pub(crate) fn unix_time_ms() -> i64 {
    services()
        .read()
        .expect("platform services lock")
        .clock
        .unix_time_ms()
}

pub(crate) fn fill_entropy(buf: &mut [u8]) -> Result<(), bugsee_platform::EntropyError> {
    services()
        .read()
        .expect("platform services lock")
        .entropy
        .fill(buf)
}
