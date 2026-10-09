//! The pixel cats: sprites stamped on a 22x16 canvas of palette keys, drawn two pixels to a
//! terminal cell with half blocks in 24-bit colour.

use std::ffi::OsString;
use std::ops::Range;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Canvas width in pixels, one per column.
pub const WIDTH: usize = 22;

/// Canvas height in pixels, two per row.
pub const HEIGHT: usize = 16;

/// A canvas of palette keys, `b'.'` where there is no pixel.
pub type Grid = [[u8; WIDTH]; HEIGHT];

/// No pixel.
const EMPTY: u8 = b'.';

/// Each key's colour: outline, orange fur, light fur, pink, eye, eye shine, star, z, heart.
const PALETTE: [(u8, Color); 9] = [
    (b'k', Color::Rgb(59, 42, 32)),
    (b'o', Color::Rgb(244, 162, 89)),
    (b'l', Color::Rgb(251, 211, 141)),
    (b'p', Color::Rgb(242, 139, 155)),
    (b'e', Color::Rgb(34, 34, 34)),
    (b'w', Color::Rgb(255, 255, 255)),
    (b'*', Color::Rgb(255, 214, 64)),
    (b'z', Color::Rgb(150, 170, 220)),
    (b'h', Color::Rgb(255, 105, 135)),
];

/// The cat's ears and brow, above its eyes.
const HEAD: [&str; 5] = [
    "..k..........k..",
    ".kpk........kpk.",
    ".kppk......kppk.",
    ".kooooooooooook.",
    "kooooooooooooook",
];

/// The cat's cheeks, nose and chin, below its eyes.
const FACE_END: [&str; 5] = [
    "kooooooppooooook",
    "kolloooooooollok",
    ".kooooolloooook.",
    "..kkooooooookk..",
    "....kkkkkkkk....",
];

const EYES_OPEN: [&str; 2] = ["kooeeooooooeeook", "kooewooooooewook"];
const EYES_CLOSED: [&str; 2] = ["kooooooooooooook", "kookkoooooookkok"];
const EYES_HALF: [&str; 2] = ["kooooooooooooook", "kooeeooooooeeook"];
const EYES_HAPPY: [&str; 2] = ["kookkoooooookkok", "kokooooooooookok"];
const EYES_WIDE: [&str; 2] = ["koeeeooooooeeeok", "koewwooooooewwok"];

const Z: [&str; 4] = ["zzzz", "..z.", ".z..", "zzzz"];
const STAR: [&str; 3] = [".*.", "***", ".*."];
const HEART: [&str; 3] = ["h.h", "hhh", ".h."];

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
    let mut grid = [[EMPTY; WIDTH]; HEIGHT];
    match pose {
        Pose::Dozing => {
            cat(&mut grid, EYES_CLOSED, 1, nth([3, 3, 4, 4], n));
            let (x, y) = nth([(17, 6), (17, 4), (18, 2), (18, 0)], n);
            stamp(&mut grid, &Z, x, y);
        }
        Pose::Excited => {
            cat(&mut grid, EYES_WIDE, 3, nth([3, 2, 1, 2], n));
            let [(x1, y1), (x2, y2)] = nth([[(0, 0), (19, 9)], [(19, 0), (0, 9)]], n);
            stamp(&mut grid, &STAR, x1, y1);
            stamp(&mut grid, &STAR, x2, y2);
        }
        Pose::Idle => {
            let eyes = if n % 12 == 11 { EYES_CLOSED } else { EYES_OPEN };
            cat(&mut grid, eyes, 3, 3);
        }
        Pose::Drowsy => {
            let (eyes, y) = nth([(EYES_CLOSED, 3), (EYES_HALF, 4)], n / 3);
            cat(&mut grid, eyes, 3, y);
        }
        Pose::Happy => {
            cat(&mut grid, EYES_HAPPY, 3, nth([3, 1, 3, 1, 3, 3], n));
            stamp(&mut grid, &HEART, 19, nth([4, 2, 0, 4, 2, 0], n));
        }
    }
    grid
}

/// The value for frame `n` of a cycle.
fn nth<T: Copy, const N: usize>(cycle: [T; N], n: u32) -> T {
    cycle[n as usize % N]
}

/// The cat with `eyes`, its top left corner at (`x`, `y`).
fn cat(grid: &mut Grid, eyes: [&str; 2], x: usize, y: usize) {
    stamp(grid, &HEAD, x, y);
    stamp(grid, &eyes, x, y + HEAD.len());
    stamp(grid, &FACE_END, x, y + HEAD.len() + eyes.len());
}

/// Copies `sprite`'s pixels onto `grid` with its top left corner at (`x`, `y`), clipped to
/// the canvas.
fn stamp(grid: &mut Grid, sprite: &[&str], x: usize, y: usize) {
    for (row, line) in grid.iter_mut().skip(y).zip(sprite) {
        for (pixel, &key) in row.iter_mut().skip(x).zip(line.as_bytes()) {
            if key != EMPTY {
                *pixel = key;
            }
        }
    }
}

/// The canvas as half-block rows, each pixel drawn `scale` x `scale`: each cell's top pixel
/// in the foreground of `▀`, its bottom one in the background, or `▄` when only the bottom
/// pixel is set.
pub fn lines(grid: &Grid, scale: usize) -> Vec<Line<'static>> {
    half_blocks(grid, scale, 0..WIDTH * scale, 0..HEIGHT * scale / 2)
}

/// [`lines`] without the blank rows and columns around the pixels set.
pub fn cropped(grid: &Grid, scale: usize) -> Vec<Line<'static>> {
    let mut columns = (0..WIDTH).filter(|&x| grid.iter().any(|row| row[x] != EMPTY));
    let mut rows = (0..HEIGHT).filter(|&y| grid[y].iter().any(|&key| key != EMPTY));
    let (Some(left), Some(top)) = (columns.next(), rows.next()) else {
        return Vec::new();
    };
    let right = columns.next_back().unwrap_or(left);
    let bottom = rows.next_back().unwrap_or(top);
    half_blocks(
        grid,
        scale,
        left * scale..(right + 1) * scale,
        top * scale / 2..((bottom + 1) * scale).div_ceil(2),
    )
}

/// The half-block cells of scaled pixel `columns` on cell `rows`, each holding scaled pixel
/// rows `2 * row` and `2 * row + 1`.
fn half_blocks(
    grid: &Grid,
    scale: usize,
    columns: Range<usize>,
    rows: Range<usize>,
) -> Vec<Line<'static>> {
    rows.map(|row| {
        let (top, bottom) = (&grid[2 * row / scale], &grid[(2 * row + 1) / scale]);
        Line::from(
            columns
                .clone()
                .map(|x| cell(top[x / scale], bottom[x / scale]))
                .collect::<Vec<_>>(),
        )
    })
    .collect()
}

fn cell(top: u8, bottom: u8) -> Span<'static> {
    match (colour(top), colour(bottom)) {
        (None, None) => Span::raw(" "),
        (Some(top), None) => Span::styled("▀", Style::new().fg(top)),
        (None, Some(bottom)) => Span::styled("▄", Style::new().fg(bottom)),
        (Some(top), Some(bottom)) => Span::styled("▀", Style::new().fg(top).bg(bottom)),
    }
}

/// `key`'s colour, `None` for no pixel.
fn colour(key: u8) -> Option<Color> {
    PALETTE
        .iter()
        .find(|&&(palette_key, _)| palette_key == key)
        .map(|&(_, colour)| colour)
}

/// Whether the terminal draws 24-bit colour and colour is wanted: `COLORTERM` is
/// `truecolor` or `24bit` and `NO_COLOR` is unset.
pub fn supported(env: impl Fn(&str) -> Option<OsString>) -> bool {
    env("COLORTERM").is_some_and(|colorterm| colorterm == "truecolor" || colorterm == "24bit")
        && env("NO_COLOR").is_none()
}

#[cfg(test)]
mod tests {
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
                Pose::Dozing => 10,
                Pose::Excited => 2 * 5,
                Pose::Idle | Pose::Drowsy => 0,
                Pose::Happy => 6,
            };
            for n in 0..16 {
                let grid = frame(pose, n);
                assert_eq!(pixels(&grid), cat + overlays, "{pose:?} frame {n}");
                for scale in 1..=3 {
                    let lines = lines(&grid, scale);
                    assert_eq!(lines.len(), HEIGHT / 2 * scale, "{pose:?} frame {n}");
                    assert!(lines.iter().all(|line| line.width() == WIDTH * scale));
                }
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
        for (n, (x, y)) in [(17, 6), (17, 4), (18, 2), (18, 0), (17, 6)]
            .into_iter()
            .enumerate()
        {
            let grid = frame(Pose::Dozing, u32::try_from(n).unwrap());
            assert_eq!(row(&grid, y, x, 4), "zzzz", "frame {n}");
            assert_eq!(row(&grid, y + 1, x, 4), "..z.", "frame {n}");
            assert_eq!(row(&grid, y + 2, x, 4), ".z..", "frame {n}");
            assert_eq!(row(&grid, y + 3, x, 4), "zzzz", "frame {n}");
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
        let lines = lines(&grid, 1);
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
    fn scaling_turns_each_pixel_into_a_square_of_pixels() {
        let mut grid = [[b'.'; WIDTH]; HEIGHT];
        grid[0][0] = b'k';
        grid[1][1] = b'k';
        let lines = lines(&grid, 2);
        let cells = |row: usize| -> Vec<(&str, Style)> {
            lines[row].spans[..4]
                .iter()
                .map(|span| (span.content.as_ref(), span.style))
                .collect()
        };
        let full = ("▀", Style::new().fg(OUTLINE).bg(OUTLINE));
        let blank = (" ", Style::new());
        assert_eq!(cells(0), [full, full, blank, blank]);
        assert_eq!(cells(1), [blank, blank, full, full]);
        assert_eq!(lines[2].to_string(), " ".repeat(2 * WIDTH));
        let bounds = Bounds::of(&grid).unwrap();
        assert_eq!(
            bounds,
            Bounds {
                columns: 0..2,
                rows: 0..2
            }
        );
        assert_eq!(bounds.cells(2), (0..4, 0..2));
        assert_eq!(
            cropped(&grid, &bounds, 2)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["▀▀  ", "  ▀▀"]
        );
        assert_eq!(Bounds::of(&[[b'.'; WIDTH]; HEIGHT]), None);
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
