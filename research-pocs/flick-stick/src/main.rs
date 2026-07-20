//! Flick Stick proof-of-concept (Jibb Smart algorithm).

#[derive(Debug, Clone)]
struct FlickStickConfig {
    threshold: f32,
    flick_time: f32,
    sensitivity: f32,
}

impl Default for FlickStickConfig {
    fn default() -> Self {
        Self {
            threshold: 0.90,
            flick_time: 0.10,
            sensitivity: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
struct FlickStick {
    flick_progress: f32,
    flick_target_yaw: f32,
    last_stick: (f32, f32),
    is_flicking: bool,
}

impl FlickStick {
    fn new() -> Self {
        Self {
            flick_progress: 0.0,
            flick_target_yaw: 0.0,
            last_stick: (0.0, 0.0),
            is_flicking: false,
        }
    }

    fn update(&mut self, stick: (f32, f32), gyro_yaw: f32, dt: f32, config: &FlickStickConfig) -> f32 {
        let (x, y) = stick;
        let (lx, ly) = self.last_stick;
        let mag = (x * x + y * y).sqrt();
        let last_mag = (lx * lx + ly * ly).sqrt();
        let mut yaw = 0.0f32;

        if mag >= config.threshold && last_mag < config.threshold {
            self.is_flicking = true;
            self.flick_progress = 0.0;
            self.flick_target_yaw = (-x).atan2(y) * config.sensitivity;
            println!("  FLICK start angle={:.1}°", self.flick_target_yaw.to_degrees());
        }

        if self.is_flicking {
            let last_p = self.flick_progress;
            self.flick_progress = (self.flick_progress + dt).min(config.flick_time);
            let last_t = last_p / config.flick_time;
            let this_t = self.flick_progress / config.flick_time;
            let warped_last = warp_ease_out(last_t);
            let warped_this = warp_ease_out(this_t);
            yaw += (warped_this - warped_last) * self.flick_target_yaw;
            if self.flick_progress >= config.flick_time {
                self.is_flicking = false;
            }
        }

        yaw += gyro_yaw;
        self.last_stick = stick;
        yaw
    }
}

fn warp_ease_out(t: f32) -> f32 {
    let flipped = 1.0 - t.clamp(0.0, 1.0);
    1.0 - flipped * flipped
}

trait Degrees {
    fn to_degrees(self) -> f32;
}
impl Degrees for f32 {
    fn to_degrees(self) -> f32 {
        self * 180.0 / std::f32::consts::PI
    }
}

fn main() {
    println!("=== Flick Stick PoC @ 120 Hz ===");
    let config = FlickStickConfig::default();
    let mut fs = FlickStick::new();
    let dt = 1.0 / 120.0;
    let mut total = 0.0f32;

    // Sequence: center, flick right, return, flick up
    for frame in 0..240 {
        let t = frame as f32;
        let (stick, gyro) = match frame {
            0..=30 => ((0.0, 0.0), 0.0f32),
            31..=40 => (((t - 30.0) / 10.0 * 0.9, 0.0), 0.0f32),
            41..=100 => ((0.9, 0.0), 0.0f32),
            101..=130 => ((0.9 * (1.0 - (t - 100.0) / 30.0), 0.0), 0.0f32),
            131..=160 => ((0.0, 0.0), 0.0f32),
            161..=170 => ((0.0, (t - 160.0) / 10.0 * 0.9), 0.0f32),
            171..=230 => ((0.0, 0.9), 0.0f32),
            _ => ((0.0, 0.0), 0.0f32),
        };

        let delta = fs.update(stick, gyro, dt, &config);
        total += delta;

        if frame % 20 == 0 {
            println!("frame {:3}: stick=({:+.2}, {:+.2}) delta={:+.2}° total={:+.2}°",
                     frame, stick.0, stick.1, delta.to_degrees(), total.to_degrees());
        }
    }
}
