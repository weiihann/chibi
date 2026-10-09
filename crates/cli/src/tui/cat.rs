//! The pixel cats: sprites stamped on a 22x16 canvas of palette keys, drawn two pixels to a
//! terminal cell with half blocks in 24-bit colour.

use std::ffi::OsString;

use ratatui::text::Line;

/// Canvas width in pixels, one per column.
pub const WIDTH: usize = 22;

/// Canvas height in pixels, two per row.
pub const HEIGHT: usize = 16;

/// A canvas of palette keys, `b'.'` where there is no pixel.
pub type Grid = [[u8; WIDTH]; HEIGHT];

/// What the cat is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pose {
    /// Asleep and breathing while waiting for a state peer, a z drifting up.
    Dozing,
    /// Hopping, wide-eyed, at the first state peer, stars twinkling.
    Excited,
    /// Sitting beside the form, blinking now and then.
    Idle,
    /// Nodding off while no state peer is connected.
    Drowsy,
    /// Bouncing at a verified successful receipt, a heart rising.
    Happy,
}

/// Frame `n` of `pose`, one frame per 250 ms.
pub fn frame(pose: Pose, n: u32) -> Grid {
    let _ = (pose, n);
    todo!()
}

/// The canvas as half-block rows: each cell's top pixel in the foreground of `▀`, its
/// bottom one in the background, or `▄` when only the bottom pixel is set.
pub fn lines(grid: &Grid) -> Vec<Line<'static>> {
    let _ = grid;
    todo!()
}

/// Whether the terminal draws 24-bit colour and colour is wanted: `COLORTERM` is
/// `truecolor` or `24bit` and `NO_COLOR` is unset.
pub fn supported(env: impl Fn(&str) -> Option<OsString>) -> bool {
    let _ = env;
    todo!()
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};

    use super::*;

    const POSES: [Pose; 5] = [
        Pose::Dozing,
        Pose::Excited,
        Pose::Idle,
        Pose::Drowsy,
        Pose::Happy,
    ];

    const OUTLINE: Color = Color::Rgb(59, 42, 32);

    fn row(grid: &Grid, y: usize, x: usize, len: usize) -> &str {
        std::str::from_utf8(&grid[y][x..x + len]).unwrap()
    }

    fn pixels(grid: &Grid) -> usize {
        grid.iter().flatten().filter(|&&key| key != b'.').count()
    }

    #[test]
    fn every_frame_holds_the_whole_cat_and_its_overlays_on_the_canvas() {
        let cat = 112 + 32;
        for pose in POSES {
            let overlays = match pose {
                Pose::Dozing => 7,
                Pose::Excited => 2 * 5,
                Pose::Idle | Pose::Drowsy => 0,
                Pose::Happy => 6,
            };
            for n in 0..16 {
                let grid = frame(pose, n);
                assert_eq!(pixels(&grid), cat + overlays, "{pose:?} frame {n}");
                let lines = lines(&grid);
                assert_eq!(lines.len(), HEIGHT / 2, "{pose:?} frame {n}");
                assert!(lines.iter().all(|line| line.width() == WIDTH));
            }
        }
    }

    #[test]
    fn the_excited_cat_hops_between_alternating_stars() {
        let first = frame(Pose::Excited, 0);
        let second = frame(Pose::Excited, 1);
        assert_eq!((first[1][1], first[1][20]), (b'*', b'.'));
        assert_eq!((second[1][1], second[1][20]), (b'.', b'*'));
        assert_eq!((first[10][1], first[10][20]), (b'.', b'*'));
        assert_eq!((second[10][1], second[10][20]), (b'*', b'.'));
        let tops: Vec<usize> = (0..4)
            .map(|n| {
                (0..HEIGHT)
                    .find(|&y| frame(Pose::Excited, n)[y][5] == b'k')
                    .unwrap()
            })
            .collect();
        assert_eq!(tops, [3, 2, 1, 2]);
        assert_eq!(row(&first, 3 + 6, 3, 16), "koewwooooooewwok");
    }

    #[test]
    fn the_idle_cat_blinks_on_every_twelfth_frame() {
        for n in [0, 10, 12] {
            let grid = frame(Pose::Idle, n);
            assert_eq!(row(&grid, 8, 3, 16), "kooeeooooooeeook", "frame {n}");
            assert_eq!(row(&grid, 9, 3, 16), "kooewooooooewook", "frame {n}");
        }
        let blink = frame(Pose::Idle, 11);
        assert_eq!(row(&blink, 8, 3, 16), "kooooooooooooook");
        assert_eq!(row(&blink, 9, 3, 16), "kookkoooooookkok");
    }

    #[test]
    fn the_dozing_cat_breathes_and_its_z_drifts_up() {
        for (n, (x, y)) in [(17, 6), (18, 4), (18, 2), (19, 0), (17, 6)]
            .into_iter()
            .enumerate()
        {
            let grid = frame(Pose::Dozing, u32::try_from(n).unwrap());
            assert_eq!(row(&grid, y, x, 3), "zzz", "frame {n}");
            assert_eq!(row(&grid, y + 1, x, 3), ".z.", "frame {n}");
            assert_eq!(row(&grid, y + 2, x, 3), "zzz", "frame {n}");
            let top = if n % 4 < 2 { 3 } else { 4 };
            assert_eq!(grid[top][3], b'k', "frame {n}");
            assert_eq!(grid[top - 1][3], b'.', "frame {n}");
            assert_eq!(row(&grid, top + 6, 1, 16), "kookkoooooookkok");
        }
    }

    #[test]
    fn the_drowsy_cat_nods_every_three_frames_and_the_happy_one_bounces_under_a_heart() {
        let drowsy: Vec<(usize, &str)> = [0, 2, 3, 5, 6]
            .into_iter()
            .map(|n| {
                let grid = frame(Pose::Drowsy, n);
                let top = (0..HEIGHT).find(|&y| grid[y][5] == b'k').unwrap();
                let eyes = if grid[top + 6][6] == b'e' {
                    "half"
                } else {
                    "closed"
                };
                (top, eyes)
            })
            .collect();
        assert_eq!(
            drowsy,
            [
                (3, "closed"),
                (3, "closed"),
                (4, "half"),
                (4, "half"),
                (3, "closed")
            ]
        );
        let happy: Vec<(usize, usize)> = (0..6)
            .map(|n| {
                let grid = frame(Pose::Happy, n);
                let top = (0..HEIGHT).find(|&y| grid[y][5] == b'k').unwrap();
                let heart = (0..HEIGHT).find(|&y| grid[y][19] == b'h').unwrap();
                (top, heart)
            })
            .collect();
        assert_eq!(happy, [(3, 4), (1, 2), (3, 0), (1, 4), (3, 2), (3, 0)]);
        assert_eq!(row(&frame(Pose::Happy, 0), 8, 3, 16), "kookkoooooookkok");
    }

    #[test]
    fn a_cell_draws_its_two_pixels_with_half_blocks() {
        let mut grid = [[b'.'; WIDTH]; HEIGHT];
        grid[0][0] = b'k';
        grid[1][2] = b'e';
        grid[0][3] = b'o';
        grid[1][3] = b'l';
        let lines = lines(&grid);
        let cells: Vec<(&str, Style)> = lines[0].spans[..4]
            .iter()
            .map(|span| (span.content.as_ref(), span.style))
            .collect();
        assert_eq!(
            cells,
            [
                ("▀", Style::new().fg(OUTLINE)),
                (" ", Style::new()),
                ("▄", Style::new().fg(Color::Rgb(34, 34, 34))),
                (
                    "▀",
                    Style::new()
                        .fg(Color::Rgb(244, 162, 89))
                        .bg(Color::Rgb(251, 211, 141))
                ),
            ]
        );
        assert_eq!(lines[1].to_string(), " ".repeat(WIDTH));
    }

    #[test]
    fn pixels_need_24_bit_colour_and_no_no_color() {
        let env = |colorterm: Option<&str>, no_color: Option<&str>| {
            supported(|key| match key {
                "COLORTERM" => colorterm.map(OsString::from),
                "NO_COLOR" => no_color.map(OsString::from),
                _ => None,
            })
        };
        assert!(env(Some("truecolor"), None));
        assert!(env(Some("24bit"), None));
        assert!(!env(Some("truecolor"), Some("1")));
        assert!(!env(Some("24bit"), Some("")));
        assert!(!env(Some("256"), None));
        assert!(!env(None, None));
    }
}
