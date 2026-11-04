//! Cursor and smooth-scroll animations. The grid from nvim is already final,
//! these only offset where it gets drawn until they settle.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::cells::RowInstances;

/// Cubic ease-out.
fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

fn progress(start: Instant, duration: Duration, now: Instant) -> f32 {
    if duration.is_zero() {
        return 1.0;
    }
    now.saturating_duration_since(start).as_secs_f32() / duration.as_secs_f32()
}

/// One cursor corner. Critically damped spring, so it never overshoots.
#[derive(Debug, Clone, Copy, Default)]
struct Corner {
    pos: [f32; 2],
    vel: [f32; 2],
    target: [f32; 2],
    /// roughly the settle time, in seconds
    length: f32,
}

impl Corner {
    fn step(&mut self, dt: f32) {
        if self.length <= 0.0 {
            self.pos = self.target;
            self.vel = [0.0; 2];
            return;
        }
        let omega = 4.0 / self.length;
        let decay = (-omega * dt).exp();
        for i in 0..2 {
            let x = self.pos[i] - self.target[i];
            let v = self.vel[i];
            let k = v + omega * x;
            self.pos[i] = self.target[i] + (x + k * dt) * decay;
            self.vel[i] = (v - omega * k * dt) * decay;
        }
    }

    fn settled(&self) -> bool {
        let close =
            |i: usize| (self.pos[i] - self.target[i]).abs() < 0.5 && self.vel[i].abs() < 5.0;
        close(0) && close(1)
    }
}

// TL, TR, BR, BL
const CORNERS: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];

/// Neovide-style cursor: every corner has its own spring. The ones facing the
/// direction of travel are faster, so the cursor smears out and then the rear
/// catches up.
#[derive(Debug, Clone, Copy)]
pub struct CursorAnimation {
    corners: [Corner; 4],
    rect: [f32; 4],
    last: Instant,
    initialized: bool,
}

impl CursorAnimation {
    pub fn new() -> Self {
        Self {
            corners: [Corner::default(); 4],
            rect: [0.0; 4],
            last: Instant::now(),
            initialized: false,
        }
    }

    /// `trail` (0-1) is how far the leading corners get ahead of the rear ones.
    /// Short moves within a row (i.e. typing) take half of `length`.
    pub fn retarget(
        &mut self,
        rect: [f32; 4],
        cell: [f32; 2],
        length: Duration,
        trail: f32,
        now: Instant,
    ) {
        if self.initialized && rect == self.rect {
            return;
        }
        let [x, y, w, h] = rect;
        let targets = CORNERS.map(|[cx, cy]| [x + cx * w, y + cy * h]);
        if !self.initialized || length.is_zero() {
            for (corner, target) in self.corners.iter_mut().zip(targets) {
                *corner = Corner {
                    pos: target,
                    vel: [0.0; 2],
                    target,
                    length: 0.0,
                };
            }
            *self = Self {
                rect,
                last: now,
                initialized: true,
                ..*self
            };
            return;
        }
        if self.settled() {
            // coming from rest, don't count the idle time
            self.last = now;
        }

        let old = self.rect;
        let travel = [
            x + w / 2.0 - (old[0] + old[2] / 2.0),
            y + h / 2.0 - (old[1] + old[3] / 2.0),
        ];
        let short = travel[1].abs() < 0.5 && travel[0].abs() <= 2.0 * cell[0];
        let base = length.as_secs_f32() * if short { 0.5 } else { 1.0 };
        let norm = (travel[0] * travel[0] + travel[1] * travel[1])
            .sqrt()
            .max(1e-3);
        let dir = [travel[0] / norm, travel[1] / norm];
        // the two corners most aligned with the travel direction lead
        let mut ranked: Vec<(usize, f32)> = CORNERS
            .iter()
            .enumerate()
            .map(|(i, [cx, cy])| (i, (cx - 0.5) * dir[0] + (cy - 0.5) * dir[1]))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (rank, (i, _)) in ranked.into_iter().enumerate() {
            let leading = rank < 2;
            self.corners[i].target = targets[i];
            self.corners[i].length = if leading && !short {
                base * (1.0 - trail.clamp(0.0, 1.0))
            } else {
                base
            };
        }
        self.rect = rect;
    }

    /// Corners TL, TR, BR, BL, and whether it settled.
    pub fn update(&mut self, now: Instant) -> ([[f32; 2]; 4], bool) {
        // cap dt, otherwise after a long idle gap the spring jumps
        let mut remaining = now
            .saturating_duration_since(self.last)
            .as_secs_f32()
            .min(0.1);
        self.last = now;
        while remaining > 0.0 {
            let dt = remaining.min(1.0 / 240.0);
            self.corners.iter_mut().for_each(|c| c.step(dt));
            remaining -= dt;
        }
        let settled = self.settled();
        if settled {
            self.corners.iter_mut().for_each(|c| {
                c.pos = c.target;
                c.vel = [0.0; 2];
            });
        }
        // subpixel while moving (edges are AA'd), snapped to the cell once settled
        (self.corners.map(|c| c.pos), settled)
    }

    fn settled(&self) -> bool {
        self.corners.iter().all(Corner::settled)
    }
}

/// In cells: rows [top, bot), cols [left, right).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub top: usize,
    pub bot: usize,
    pub left: usize,
    pub right: usize,
}

pub struct ScrollAnimation {
    pub region: Region,
    /// rows; positive = content starts below its final spot (scrolling down)
    from: f32,
    start: Instant,
    duration: Duration,
    /// Rows scrolled off the top; the last one sits right above the region.
    pub above: Vec<Arc<RowInstances>>,
    /// Rows scrolled off the bottom; the first one sits right below the region.
    pub below: Vec<Arc<RowInstances>>,
}

impl ScrollAnimation {
    pub fn offset(&self, now: Instant) -> f32 {
        self.from * (1.0 - ease_out(progress(self.start, self.duration, now)))
    }

    pub fn done(&self, now: Instant) -> bool {
        progress(self.start, self.duration, now) >= 1.0
    }
}

/// Running scrolls, plus last frame's rows so we know what went off screen.
#[derive(Default)]
pub struct Scrolling {
    pub animations: Vec<ScrollAnimation>,
    snapshot: Vec<Option<Arc<RowInstances>>>,
}

impl Scrolling {
    pub fn set_snapshot(&mut self, rows: impl Iterator<Item = Arc<RowInstances>>) {
        self.snapshot.clear();
        self.snapshot.extend(rows.map(Some));
    }

    /// `rows` > 0 means the content moves up.
    pub fn scroll(&mut self, region: Region, rows: i64, duration: Duration, now: Instant) {
        let bot = region.bot.min(self.snapshot.len());
        if region.top >= bot {
            return;
        }
        let height = bot - region.top;
        let n = rows.unsigned_abs() as usize;
        // big jumps (gg, G) don't animate, same as Neovide
        if n == 0 || n >= height || duration.is_zero() {
            self.animations.retain(|a| a.region != region);
            self.rotate(region.top, bot, rows);
            return;
        }

        let out: Vec<Arc<RowInstances>> = if rows > 0 {
            self.snapshot[region.top..region.top + n]
                .iter()
                .flatten()
                .cloned()
                .collect()
        } else {
            self.snapshot[bot - n..bot]
                .iter()
                .flatten()
                .cloned()
                .collect()
        };
        let index = match self.animations.iter().position(|a| a.region == region) {
            Some(i) => i,
            None => {
                self.animations.push(ScrollAnimation {
                    region,
                    from: 0.0,
                    start: now,
                    duration,
                    above: Vec::new(),
                    below: Vec::new(),
                });
                self.animations.len() - 1
            }
        };
        let anim = &mut self.animations[index];
        let current = anim.offset(now);
        if rows > 0 {
            anim.above.extend(out);
        } else {
            anim.below.splice(0..0, out);
        }
        let excess = anim.above.len().saturating_sub(height);
        anim.above.drain(..excess);
        anim.below.truncate(height);
        let limit_up = anim.above.len() as f32;
        let limit_down = anim.below.len() as f32;
        anim.from = (current + rows as f32).clamp(-limit_down, limit_up);
        anim.start = now;
        anim.duration = duration;
        self.rotate(region.top, bot, rows);
    }

    fn rotate(&mut self, top: usize, bot: usize, rows: i64) {
        let n = (rows.unsigned_abs() as usize).min(bot - top);
        let slice = &mut self.snapshot[top..bot];
        if rows > 0 {
            slice.rotate_left(n);
            let len = slice.len();
            slice[len - n..].fill(None);
        } else {
            slice.rotate_right(n);
            slice[..n].fill(None);
        }
    }

    pub fn prune(&mut self, now: Instant) -> bool {
        self.animations.retain(|a| !a.done(now));
        !self.animations.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize) -> Vec<Arc<RowInstances>> {
        (0..n).map(|_| Arc::new(RowInstances::default())).collect()
    }

    const REGION: Region = Region {
        top: 0,
        bot: 10,
        left: 0,
        right: 80,
    };
    const D: Duration = Duration::from_millis(100);

    #[test]
    fn cursor_corners_trail_and_settle() {
        let now = Instant::now();
        let cell = [10.0, 20.0];
        let mut c = CursorAnimation::new();
        c.retarget([0.0, 0.0, 10.0, 20.0], cell, D, 0.4, now);
        assert!(c.update(now).1);
        // long move right: the right (leading) corners get ahead, so it stretches
        c.retarget([200.0, 0.0, 10.0, 20.0], cell, D, 0.4, now);
        let (corners, settled) = c.update(now + D / 4);
        assert!(!settled);
        let left_progress = corners[0][0];
        let right_progress = corners[1][0] - 10.0;
        assert!(right_progress > left_progress, "{corners:?}");
        let mut t = now + D / 4;
        while t < now + D * 3 {
            t += Duration::from_millis(13);
            c.update(t);
        }
        let (corners, settled) = c.update(t);
        assert!(settled);
        assert_eq!(
            corners,
            [[200.0, 0.0], [210.0, 0.0], [210.0, 20.0], [200.0, 20.0]]
        );
    }

    #[test]
    fn scroll_keeps_rows_that_left_the_screen() {
        let now = Instant::now();
        let snap = rows(10);
        let mut s = Scrolling::default();
        s.set_snapshot(snap.iter().cloned());
        s.scroll(REGION, 3, D, now);
        let a = &s.animations[0];
        assert_eq!(a.offset(now), 3.0);
        assert_eq!(a.above.len(), 3);
        assert!(Arc::ptr_eq(&a.above[2], &snap[2]));
        // scrolling again mid-animation stacks on the current offset
        s.scroll(REGION, 3, D, now);
        let a = &s.animations[0];
        assert_eq!(a.offset(now), 6.0);
        assert!(Arc::ptr_eq(&a.above[5], &snap[5]));
        assert!(s.prune(now));
        assert!(!s.prune(now + D));
    }

    #[test]
    fn scroll_up_fills_from_below_and_big_jumps_do_not_animate() {
        let now = Instant::now();
        let snap = rows(10);
        let mut s = Scrolling::default();
        s.set_snapshot(snap.iter().cloned());
        s.scroll(REGION, -2, D, now);
        let a = &s.animations[0];
        assert_eq!(a.offset(now), -2.0);
        assert!(Arc::ptr_eq(&a.below[0], &snap[8]));
        assert!(Arc::ptr_eq(&a.below[1], &snap[9]));
        s.scroll(REGION, 10, D, now);
        assert!(s.animations.is_empty());
    }
}
