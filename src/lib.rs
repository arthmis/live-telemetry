pub mod mapped_view;

/// `WaitForSingleObject` timeout value meaning "wait forever".
pub const INFINITE: u32 = 0xFFFF_FFFF;

/// Timer period: ~3 ms ≈ 333 Hz.
pub const PERIOD_MS: u64 = 16;

/// Same period in 100-ns intervals; negative value means relative to now.
pub const PERIOD_100NS: i64 = -30_000;

/// Mirrors the first three fields of `SPageFilePhysics`.
///
/// The struct uses `#pragma pack(4)` and every field is 4 bytes wide, so the
/// layout is a flat byte blob with no padding:
///
/// | offset | type | field      |
/// |--------|------|------------|
/// |  0     | i32  | packet_id  |
/// |  4     | f32  | throttle   |
/// |  8     | f32  | brake      |
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PhysicsPage {
    packet_id: i32,
    /// Throttle input in the range `0.0–1.0`.
    throttle: f32,
    /// Brake input in the range `0.0–1.0`.
    brake: f32,
}

impl PhysicsPage {
    pub fn new(packet_id: i32, throttle: f32, brake: f32) -> Self {
        Self {
            packet_id,
            throttle,
            brake,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Inputs {
    throttle: f64,
    brake: f64,
}

impl Inputs {
    pub fn new(throttle: f64, brake: f64) -> Self {
        Self { throttle, brake }
    }

    #[inline(always)]
    pub fn throttle(&self) -> f64 {
        self.throttle
    }

    #[inline(always)]
    pub fn brake(&self) -> f64 {
        self.brake
    }
}

impl From<PhysicsPage> for Inputs {
    fn from(page: PhysicsPage) -> Self {
        Self {
            throttle: (page.throttle * 100.) as f64,
            brake: (page.brake * 100.) as f64,
        }
    }
}
