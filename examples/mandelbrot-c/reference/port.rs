//! Reference port of mandelbrot: 4-wide ILP (bit-identical IEEE-754 order, zero FMA)
//! + dynamic row work-stealing across CPU threads.

use std::sync::atomic::{AtomicI32, Ordering};

#[inline(always)]
fn mandelbrot_scalar(x0: f64, y0: f64, max_iter: i32) -> i32 {
    let (mut x, mut y, mut i) = (0.0f64, 0.0f64, 0);
    while x * x + y * y <= 4.0 && i < max_iter {
        let t = x * x - y * y + x0;
        y = 2.0 * x * y + y0;
        x = t;
        i += 1;
    }
    i32::from(i == max_iter)
}

pub fn mandelbrot(width: i32, height: i32, max_iter: i32) -> Result<i64, String> {
    if width <= 0 || height <= 0 || max_iter <= 0 {
        return Ok(if max_iter == 0 && width > 0 && height > 0 {
            i64::from(width) * i64::from(height)
        } else {
            0
        });
    }
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(height as usize);
    let next_row = AtomicI32::new(0);
    let w_f = width as f64;
    let h_f = height as f64;
    let total = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut inside = 0i64;
                    loop {
                        let start_y = next_row.fetch_add(4, Ordering::Relaxed);
                        if start_y >= height {
                            break;
                        }
                        let end_y = (start_y + 4).min(height);
                        for py in start_y..end_y {
                            let y0 = -1.5 + 3.0 * py as f64 / h_f;
                            let mut px = 0;
                            while px + 4 <= width {
                                let x0 = [
                                    -2.0 + 3.0 * px as f64 / w_f,
                                    -2.0 + 3.0 * (px + 1) as f64 / w_f,
                                    -2.0 + 3.0 * (px + 2) as f64 / w_f,
                                    -2.0 + 3.0 * (px + 3) as f64 / w_f,
                                ];
                                let mut x = [0.0f64; 4];
                                let mut y = [0.0f64; 4];
                                let mut it = [0i32; 4];
                                for _ in 0..max_iter {
                                    let mut any = false;
                                    for k in 0..4 {
                                        let xx = x[k] * x[k];
                                        let yy = y[k] * y[k];
                                        if xx + yy <= 4.0 {
                                            y[k] = 2.0 * x[k] * y[k] + y0;
                                            x[k] = xx - yy + x0[k];
                                            it[k] += 1;
                                            any = true;
                                        }
                                    }
                                    if !any {
                                        break;
                                    }
                                }
                                for count in it {
                                    inside += i64::from(count == max_iter);
                                }
                                px += 4;
                            }
                            while px < width {
                                let x0 = -2.0 + 3.0 * px as f64 / w_f;
                                inside += i64::from(mandelbrot_scalar(x0, y0, max_iter));
                                px += 1;
                            }
                        }
                    }
                    inside
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("worker panicked"))
            .sum::<i64>()
    });
    Ok(total)
}
