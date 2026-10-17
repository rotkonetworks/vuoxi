use crate::GateFuture;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::task::{Context, Poll};
use tower_layer::Layer;
use tower_service::Service;
use vuoxi_core::{Error, Request, Response};

/// CPU temperature in degrees Celsius; `None` if it cannot be read.
pub trait TempSource {
    fn celsius(&self) -> Option<f32>;
}

/// Reads `temp1_input` of the `k10temp` / `coretemp` chips under a hwmon
/// directory; the hottest wins. Nothing else is touched.
#[derive(Clone, Debug)]
pub struct Hwmon {
    root: PathBuf,
}

impl Hwmon {
    pub fn system() -> Self {
        Self::at("/sys/class/hwmon")
    }

    /// A test tree shaped like `/sys/class/hwmon`.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl TempSource for Hwmon {
    fn celsius(&self) -> Option<f32> {
        let read = |p: &Path, f: &str| fs::read_to_string(p.join(f)).ok();
        fs::read_dir(&self.root)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| read(p, "name").is_some_and(|n| matches!(n.trim(), "k10temp" | "coretemp")))
            .filter_map(|p| read(&p, "temp1_input")?.trim().parse::<i32>().ok())
            .max()
            .map(|milli| milli as f32 / 1000.0)
    }
}

/// A temperature set by hand; clones share it.
#[derive(Clone, Debug)]
pub struct ManualTemp(Arc<AtomicI32>);

impl ManualTemp {
    pub fn new(celsius: f32) -> Self {
        Self(Arc::new(AtomicI32::new((celsius * 1000.0) as i32)))
    }

    pub fn set(&self, celsius: f32) {
        self.0.store((celsius * 1000.0) as i32, Ordering::Relaxed);
    }
}

impl TempSource for ManualTemp {
    fn celsius(&self) -> Option<f32> {
        Some(self.0.load(Ordering::Relaxed) as f32 / 1000.0)
    }
}

pub const DEFAULT_LIMIT_C: f32 = 90.0;

/// Defers the hashing- and IO-heavy requests (`Changed`, `Fetch`) while the
/// CPU is hotter than the limit. `Offer` and `Reconcile` always pass. An
/// unreadable sensor never blocks work.
#[derive(Clone, Debug)]
pub struct ThermalLayer<T> {
    temp: T,
    limit: f32,
}

impl<T> ThermalLayer<T> {
    pub fn new(temp: T) -> Self {
        Self::with_limit(temp, DEFAULT_LIMIT_C)
    }

    pub fn with_limit(temp: T, limit: f32) -> Self {
        Self { temp, limit }
    }
}

impl<S, T: Clone> Layer<S> for ThermalLayer<T> {
    type Service = Thermal<S, T>;

    fn layer(&self, inner: S) -> Self::Service {
        Thermal {
            inner,
            temp: self.temp.clone(),
            limit: self.limit,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Thermal<S, T> {
    inner: S,
    temp: T,
    limit: f32,
}

impl<S, T> Service<Request> for Thermal<S, T>
where
    S: Service<Request, Response = Response, Error = Error>,
    T: TempSource,
{
    type Response = Response;
    type Error = Error;
    type Future = GateFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let heavy = matches!(req, Request::Changed(_) | Request::Fetch(..));
        if heavy && self.temp.celsius().is_some_and(|c| c > self.limit) {
            GateFuture::Deferred
        } else {
            GateFuture::Inner {
                fut: self.inner.call(req),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Counter, fetch, offer, run};
    use vuoxi_core::PathId;

    #[test]
    fn heat_defers_heavy_work_only() {
        let temp = ManualTemp::new(60.0);
        let count = Counter::default();
        let mut svc = ThermalLayer::new(temp.clone()).layer(count.clone());

        assert_eq!(run(&mut svc, Request::Changed(PathId(1))), Response::Done);
        temp.set(90.0);
        assert_eq!(run(&mut svc, fetch()), Response::Done, "limit is exclusive");

        temp.set(91.5);
        assert_eq!(
            run(&mut svc, Request::Changed(PathId(1))),
            Response::Deferred
        );
        assert_eq!(run(&mut svc, fetch()), Response::Deferred);
        assert_eq!(run(&mut svc, offer()), Response::Done);
        assert_eq!(count.calls(), 3);

        temp.set(70.0);
        assert_eq!(run(&mut svc, fetch()), Response::Done);
    }

    #[test]
    fn unreadable_sensor_does_not_block() {
        let mut svc = ThermalLayer::new(Hwmon::at("/nonexistent/hwmon")).layer(Counter::default());
        assert_eq!(run(&mut svc, fetch()), Response::Done);
    }

    #[test]
    fn hwmon_reads_only_cpu_chips() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-hwmon");
        let _ = fs::remove_dir_all(&root);
        let chip = |dir: &str, name: &str, milli: &str| {
            let d = root.join(dir);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("name"), format!("{name}\n")).unwrap();
            fs::write(d.join("temp1_input"), format!("{milli}\n")).unwrap();
        };
        chip("hwmon0", "nvme", "99000");
        chip("hwmon1", "k10temp", "61250");
        chip("hwmon2", "coretemp", "55000");

        assert_eq!(Hwmon::at(&root).celsius(), Some(61.25));
    }
}
