//! Status LED on the onboard WS2812 (Waveshare ESP32-S3-ETH, GPIO 21).
//!
//! One embassy task owns the RMT channel and the GPIO. Other code asks
//! for pattern changes via [`set`] which forwards to a latest-value
//! [`Signal`]. The animation loop never blocks the kubelet.
//!
//! WS2812 framing is built straight on top of `esp_hal::rmt::PulseCode`;
//! we don't depend on `esp-hal-smartled` because its 0.17 release pins
//! `esp-hal ~1.0` and we're on 1.1. Driving the wire ourselves is ~30
//! lines and removes the version-coupling.

use embassy_executor::task;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};

use esp_hal::Async;
use esp_hal::gpio::Level;
use esp_hal::peripherals::{GPIO21, RMT};
use esp_hal::rmt::{Channel, PulseCode, Rmt, Tx, TxChannelConfig, TxChannelCreator};
use esp_hal::time::Rate;

/// Brightness ceiling applied to every channel before it hits the wire.
/// 38/255 ≈ 15%. The onboard WS2812 is genuinely painful at full power.
pub const LED_BRIGHTNESS_CAP: u8 = 38;

/// 30 Hz animation rate. Plenty for ~1 Hz breathing.
const FRAME_PERIOD_MS: u64 = 33;

/// Activity flash duration. Heartbeat blip, not a strobe.
const ACTIVITY_FLASH_MS: u64 = 100;

/// Visual states. `Activity` is a transient overlay; everything else is a
/// steady state. `Warning` / `Disconnected` / `Panic` are reserved for
/// later phases when error handling exists to drive them.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub enum LedPattern {
    SelfTest,
    Booting,
    Connecting,
    Healthy,
    Activity,
    Warning,
    Disconnected,
    Panic,
}

pub static LED_SIGNAL: Signal<CriticalSectionRawMutex, LedPattern> = Signal::new();

/// Fire-and-forget: never blocks the caller. Latest signal wins.
pub fn set(pattern: LedPattern) {
    LED_SIGNAL.signal(pattern);
}

#[derive(Clone, Copy)]
struct Color {
    r: u8,
    g: u8,
    b: u8,
}

impl Color {
    const OFF: Self = Color { r: 0, g: 0, b: 0 };
    const RED: Self = Color {
        r: LED_BRIGHTNESS_CAP,
        g: 0,
        b: 0,
    };
    const GREEN: Self = Color {
        r: 0,
        g: LED_BRIGHTNESS_CAP,
        b: 0,
    };
    const BLUE: Self = Color {
        r: 0,
        g: 0,
        b: LED_BRIGHTNESS_CAP,
    };
    const WHITE: Self = Color {
        r: LED_BRIGHTNESS_CAP,
        g: LED_BRIGHTNESS_CAP,
        b: LED_BRIGHTNESS_CAP,
    };
    const YELLOW: Self = Color {
        r: LED_BRIGHTNESS_CAP,
        g: LED_BRIGHTNESS_CAP,
        b: 0,
    };
}

#[task]
pub async fn led_task(rmt: RMT<'static>, pin: GPIO21<'static>) {
    let rmt = match Rmt::new(rmt, Rate::from_mhz(80)) {
        Ok(r) => r.into_async(),
        Err(_) => return,
    };
    let cfg = TxChannelConfig::default()
        .with_clk_divider(1)
        .with_idle_output_level(Level::Low)
        .with_idle_output(true);
    let mut tx = match rmt.channel0.configure_tx(&cfg) {
        Ok(c) => c.with_pin(pin),
        Err(_) => return,
    };

    // Self-test runs unconditionally before we start listening for signals.
    self_test(&mut tx).await;

    let mut steady = LedPattern::Booting;
    let mut steady_started = Instant::now();
    let _ = write(&mut tx, render_steady(steady, 0)).await;

    loop {
        let frame = Timer::after(Duration::from_millis(FRAME_PERIOD_MS));
        match select(frame, LED_SIGNAL.wait()).await {
            Either::First(_) => {
                let elapsed = steady_started.elapsed().as_millis();
                let _ = write(&mut tx, render_steady(steady, elapsed)).await;
            }
            Either::Second(LedPattern::Activity) => {
                let _ = write(&mut tx, Color::YELLOW).await;
                Timer::after(Duration::from_millis(ACTIVITY_FLASH_MS)).await;
                let elapsed = steady_started.elapsed().as_millis();
                let _ = write(&mut tx, render_steady(steady, elapsed)).await;
            }
            Either::Second(LedPattern::SelfTest) => {
                self_test(&mut tx).await;
                steady = LedPattern::Booting;
                steady_started = Instant::now();
            }
            Either::Second(other) => {
                steady = other;
                steady_started = Instant::now();
            }
        }
    }
}

async fn self_test(tx: &mut Channel<'static, Async, Tx>) {
    for color in [Color::RED, Color::GREEN, Color::BLUE, Color::OFF] {
        let _ = write(tx, color).await;
        Timer::after(Duration::from_millis(200)).await;
    }
}

fn render_steady(pattern: LedPattern, elapsed_ms: u64) -> Color {
    match pattern {
        LedPattern::Booting => Color::WHITE,
        // ~1.5 Hz blue: still working on it.
        LedPattern::Connecting => breathe(elapsed_ms, 667, 0, 0, LED_BRIGHTNESS_CAP),
        // ~1 Hz green: sleeping peacefully.
        LedPattern::Healthy => breathe(elapsed_ms, 1000, 0, LED_BRIGHTNESS_CAP, 0),
        // States we don't drive yet — leave the LED dark so it's obvious
        // when the kubelet does eventually surface one of these.
        _ => Color::OFF,
    }
}

/// Eased pulse between 5% and 25% of each channel cap. Smoothstep keeps
/// the corners gentle; a triangle wave reads as frantic.
fn breathe(elapsed_ms: u64, period_ms: u64, r_cap: u8, g_cap: u8, b_cap: u8) -> Color {
    let pct = breathe_pct(elapsed_ms, period_ms);
    let scale = |c: u8| ((c as u32 * pct as u32) / 100) as u8;
    Color {
        r: scale(r_cap),
        g: scale(g_cap),
        b: scale(b_cap),
    }
}

fn breathe_pct(elapsed_ms: u64, period_ms: u64) -> u8 {
    let t = (elapsed_ms % period_ms) as u32;
    let half = (period_ms / 2) as u32;
    let tri = if t < half {
        t
    } else {
        (period_ms as u32).saturating_sub(t)
    };
    let n = (tri.saturating_mul(255) / half).min(255);
    // smoothstep: 3n² − 2n³ in u32 with /255 scale.
    let n_sq = n * n / 255;
    let n_cu = n_sq * n / 255;
    let eased = (3 * n_sq).saturating_sub(2 * n_cu).min(255);
    // Map 0..=255 onto 5..=25 (% of channel cap).
    (5 + eased * 20 / 255) as u8
}

// ---- WS2812 framing ------------------------------------------------------
//
// At 80 MHz RMT clock with clk_divider=1, each tick = 12.5 ns.
// Datasheet timings (±150 ns tolerance):
//   bit 0: 0.40 µs HIGH, 0.85 µs LOW
//   bit 1: 0.80 µs HIGH, 0.45 µs LOW
// One PulseCode encodes both halves of a single bit.
const T0H: u16 = 32; // 400 ns
const T0L: u16 = 68; // 850 ns
const T1H: u16 = 64; // 800 ns
const T1L: u16 = 36; // 450 ns

async fn write(tx: &mut Channel<'static, Async, Tx>, c: Color) -> Result<(), esp_hal::rmt::Error> {
    // WS2812 expects GRB, MSB first.
    let grb: u32 = ((c.g as u32) << 16) | ((c.r as u32) << 8) | (c.b as u32);
    let mut buf = [PulseCode::end_marker(); 25];
    for (i, slot) in buf.iter_mut().take(24).enumerate() {
        let bit = (grb >> (23 - i)) & 1;
        *slot = if bit == 1 {
            PulseCode::new(Level::High, T1H, Level::Low, T1L)
        } else {
            PulseCode::new(Level::High, T0H, Level::Low, T0L)
        };
    }
    tx.transmit(&buf).await
}
