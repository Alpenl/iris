use std::time::Instant;
/// Interpolate the host playback clock between native position notifications.
pub struct OverlayClock {
    position: f64,
    anchor: Instant,
    speed: f64,
    paused: bool,
    duration: Option<f64>,
}
impl Default for OverlayClock {
    fn default() -> Self {
        Self {
            position: 0.,
            anchor: Instant::now(),
            speed: 1.,
            paused: true,
            duration: None,
        }
    }
}
impl OverlayClock {
    pub fn update(
        &mut self,
        position: f64,
        speed: f64,
        paused: bool,
        buffering: bool,
        duration: f64,
        now: Instant,
    ) {
        self.position = if position.is_finite() {
            position.max(0.)
        } else {
            0.
        };
        self.speed = if speed.is_finite() {
            speed.clamp(0.05, 16.)
        } else {
            1.
        };
        self.paused = paused || buffering;
        self.anchor = now;
        self.duration = (duration.is_finite() && duration > 0.).then_some(duration);
    }
    pub fn position_ms(&self, now: Instant) -> u64 {
        let elapsed = if self.paused {
            0.
        } else {
            now.saturating_duration_since(self.anchor).as_secs_f64() * self.speed
        };
        let position = self.position + elapsed;
        (self.duration.map_or(position, |end| position.min(end)) * 1000.) as u64
    }
    pub fn paused(&self) -> bool {
        self.paused
    }
    pub fn duration_ms(&self) -> Option<u64> {
        self.duration.map(|d| (d * 1000.) as u64)
    }
    pub fn effectively_paused(&self, now: Instant) -> bool {
        self.paused
            || self
                .duration_ms()
                .is_some_and(|end| self.position_ms(now) >= end)
    }
    pub fn speed(&self) -> f64 {
        self.speed
    }
}
