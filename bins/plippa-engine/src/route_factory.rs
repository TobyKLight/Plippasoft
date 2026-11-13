//! Sink target shared between the route manager and the audio thread (AUDIO_03).
//!
//! The control plane writes [`RouteTarget`] under a short-lived mutex; the audio
//! thread reads one snapshot at each stream open. The Pulse-backed factory itself
//! is only compiled with the `pulse` feature, but the target types stay available so
//! the mock engine and its tests use the same seam.

use plippa_audio::PulseConfig;
use std::sync::{Arc, Mutex};

/// What the factory should open next. Written by the route manager, read by the audio
/// thread through the factory closure.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteTarget {
    /// Sink name; `None` is the server default.
    pub dev: Option<String>,
    /// Latency profile for this target.
    pub cfg: PulseConfig,
    /// Route revision that produced this target (only changed targets re-open).
    pub generation: u64,
}

impl RouteTarget {
    /// A cell holding the server-default target (before routing has resolved).
    pub fn default_cell() -> RouteTargetCell {
        Arc::new(Mutex::new(Self {
            dev: None,
            cfg: PulseConfig::low_latency(),
            generation: 0,
        }))
    }
}

pub type RouteTargetCell = Arc<Mutex<RouteTarget>>;

/// `plippa.multi` is the only multi-output playback target.
pub const MULTI_TARGET: &str = plippa_audio::route::MULTI_SINK_NAME;

/// Snapshot the cell without holding the guard across the (blocking) stream open.
#[cfg(feature = "pulse")]
fn snapshot(cell: &RouteTargetCell) -> RouteTarget {
    cell.lock()
        .map(|g| g.clone())
        .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
}

/// Open `dev`, retrying once on the server default when a named sink fails.
///
/// A `pulse` build must keep playing when the route names a sink the server cannot
/// open (discovery race, unplugged device, module not yet created) — AUDIO_03 §5.3.
/// `None` is already the server default, so it has no further fallback. The fallback
/// is deliberate and logged; only a server-default open failure stops the stream.
#[cfg(any(feature = "pulse", test))]
fn open_with_fallback<T>(
    dev: Option<&str>,
    mut open: impl FnMut(Option<&str>) -> Result<T, plippa_audio::AudioError>,
) -> Result<T, plippa_audio::AudioError> {
    match open(dev) {
        Ok(value) => Ok(value),
        Err(first) => match dev {
            Some(requested) => {
                eprintln!(
                    "plippa-engine: audio target {requested} failed ({first}); falling back to server-default"
                );
                open(None).map_err(|second| {
                    plippa_audio::AudioError::Sink(format!(
                        "audio target {requested} failed ({first}); server default failed ({second})"
                    ))
                })
            }
            None => Err(first),
        },
    }
}

/// Sink factory that opens the target currently in `cell`.
#[cfg(feature = "pulse")]
pub struct RoutedSinkFactory {
    cell: RouteTargetCell,
}

#[cfg(feature = "pulse")]
impl RoutedSinkFactory {
    pub fn new(cell: RouteTargetCell) -> Self {
        Self { cell }
    }
}

#[cfg(feature = "pulse")]
impl plippa_audio::AudioSinkFactory for RoutedSinkFactory {
    fn open(
        &self,
        channels: u16,
        sample_rate_hz: u32,
    ) -> Result<Box<dyn plippa_audio::AudioSink>, plippa_audio::AudioError> {
        let target = snapshot(&self.cell);
        let dev = target.dev.as_deref();
        let profile = if target.cfg == PulseConfig::low_latency() {
            "low-latency"
        } else {
            "multi"
        };
        eprintln!(
            "plippa-engine: audio target {} (profile {profile}, generation {})",
            dev.unwrap_or("server-default"),
            target.generation
        );
        let sink = open_with_fallback(dev, |candidate| {
            plippa_audio::PulseSink::new_at(
                channels,
                sample_rate_hz,
                target.cfg.clone(),
                candidate,
            )
        })?;
        Ok(Box::new(sink) as Box<dyn plippa_audio::AudioSink>)
    }
}

#[cfg(test)]
mod tests {
    use super::open_with_fallback;
    use plippa_audio::AudioError;

    fn fail(message: &str) -> AudioError {
        AudioError::Sink(message.into())
    }

    #[test]
    fn a_named_target_is_used_when_it_opens() {
        let mut attempts = Vec::new();
        let value = open_with_fallback(Some("usb"), |dev| {
            attempts.push(dev.map(str::to_string));
            Ok::<_, AudioError>("opened")
        })
        .unwrap();
        assert_eq!(value, "opened");
        assert_eq!(attempts, vec![Some("usb".to_string())]);
    }

    #[test]
    fn a_failed_named_target_retries_on_the_server_default() {
        let mut attempts = Vec::new();
        let value = open_with_fallback(Some("usb"), |dev| {
            attempts.push(dev.map(str::to_string));
            match dev {
                Some(_) => Err(fail("no such sink")),
                None => Ok("server-default"),
            }
        })
        .unwrap();
        assert_eq!(value, "server-default");
        assert_eq!(attempts, vec![Some("usb".to_string()), None]);
    }

    #[test]
    fn a_failed_server_default_is_not_retried() {
        let mut attempts = 0;
        let result = open_with_fallback(None, |dev| {
            attempts += 1;
            assert_eq!(dev, None);
            Err::<&str, _>(fail("server down"))
        });
        assert!(result.is_err());
        assert_eq!(attempts, 1, "the default is already the fallback");
    }

    #[test]
    fn both_failures_are_reported_when_the_fallback_also_fails() {
        let mut attempts = Vec::new();
        let error = open_with_fallback(Some("usb"), |dev| {
            attempts.push(dev.map(str::to_string));
            Err::<&str, _>(fail(if dev.is_some() {
                "no such sink"
            } else {
                "server down"
            }))
        })
        .unwrap_err();
        assert_eq!(attempts, vec![Some("usb".to_string()), None]);
        let message = error.to_string();
        assert!(
            message.contains("usb") && message.contains("server down"),
            "{message}"
        );
    }
}
