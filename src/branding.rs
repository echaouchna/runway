//! The runway monogram and wordmark, sampled from the shared SVG artwork
//! (`design/render-branding.py --terminal`). `branding.txt` is a pixel grid:
//! `.` transparent, `b` blue mark, `w` wordmark. Each terminal cell shows 2x2
//! pixels with a quadrant block character.

const CELLS: &str = include_str!("branding.txt");

/// Quadrant characters by filled pixels: top-left 1, top-right 2,
/// bottom-left 4, bottom-right 8.
const QUADRANTS: [char; 16] = [
    ' ', '▘', '▝', '▀', '▖', '▌', '▞', '▛', '▗', '▚', '▐', '▜', '▄', '▙', '▟', '█',
];

/// ASCII fallback (no color, redirected output): a half cell counts as filled
/// when both of its pixels are, which keeps the gaps between letters.
fn ascii(mask: u8) -> char {
    match (mask & 3 == 3, mask & 12 == 12) {
        (true, true) => '#',
        (true, false) => '\'',
        (false, true) => '_',
        (false, false) => ' ',
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Paint {
    None,
    /// A full cell: a background-colored space (no seams between glyphs).
    Background(u8),
    /// A partial cell: a quadrant character in the foreground color.
    Foreground(u8),
}

pub fn logo(color: bool) -> String {
    let rows: Vec<&[u8]> = CELLS.lines().map(str::as_bytes).collect();
    rows.chunks(2)
        .map(|pair| {
            let pixel = |row: usize, x: usize| {
                pair.get(row)
                    .and_then(|r| r.get(x))
                    .copied()
                    .unwrap_or(b'.')
            };
            let width = pair
                .iter()
                .map(|r| r.iter().rposition(|&p| p != b'.').map_or(0, |i| i + 1))
                .max()
                .unwrap_or(0);
            let mut line = String::new();
            let mut paint = Paint::None;
            for cell in 0..width.div_ceil(2) {
                let x = cell * 2;
                let quad = [pixel(0, x), pixel(0, x + 1), pixel(1, x), pixel(1, x + 1)];
                let mask = quad
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| **p != b'.')
                    .fold(0u8, |m, (i, _)| m | (1 << i));
                if !color {
                    line.push(ascii(mask));
                    continue;
                }
                let ink = quad.iter().copied().find(|p| *p != b'.').unwrap_or(b'.');
                let next = match mask {
                    0 => Paint::None,
                    15 => Paint::Background(ink),
                    _ => Paint::Foreground(ink),
                };
                if next != paint {
                    line.push_str("\x1b[0m");
                    match next {
                        Paint::None => {}
                        Paint::Background(ink) => set_color(&mut line, ink, true),
                        Paint::Foreground(ink) => set_color(&mut line, ink, false),
                    }
                    paint = next;
                }
                line.push(if mask == 15 {
                    ' '
                } else {
                    QUADRANTS[mask as usize]
                });
            }
            if color {
                line.push_str("\x1b[0m");
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn set_color(line: &mut String, cell: u8, background: bool) {
    let rgb = match cell {
        b'b' => "48;48;239",
        b'w' => "231;236;255",
        _ => return,
    };
    let plane = if background { 48 } else { 38 };
    line.push_str(&format!("\x1b[{plane};2;{rgb}m"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colored_logo_resets_the_background_before_help_text() {
        let logo = logo(true);
        assert!(logo.contains("\x1b[48;2;48;48;239m"));
        assert!(logo.contains("\x1b[48;2;231;236;255m"));
        assert!(logo.lines().all(|line| line.ends_with("\x1b[0m")));
        assert!(logo.contains('▀'));
        assert!(logo.contains('▄'));
    }

    #[test]
    fn monogram_retains_three_runway_dashes_without_color() {
        let logo = logo(false);
        // The dashes are holes in the monogram's stem (pixel column 6).
        let gaps: Vec<_> = CELLS.lines().map(|row| row.as_bytes()[6] == b'.').collect();
        assert_eq!(
            gaps.windows(2).filter(|pair| !pair[0] && pair[1]).count(),
            3
        );
        // Like the road in the artwork, the dashes have equal lengths.
        let mut lengths = Vec::new();
        for gap in &gaps {
            match (gap, lengths.last_mut()) {
                (true, Some((len, true))) => *len += 1,
                (true, _) => lengths.push((1, true)),
                (false, Some((_, open))) => *open = false,
                (false, None) => {}
            }
        }
        let lengths: Vec<usize> = lengths.iter().map(|(len, _)| *len).collect();
        assert_eq!(lengths, [3, 3, 3]);
        assert!(logo.contains("### ###"));
        assert!(logo.is_ascii());
        assert!(!logo.contains('\x1b'));
    }

    #[test]
    fn colored_and_plain_logos_have_the_same_terminal_dimensions() {
        let plain = logo(false);
        let colored = logo(true);
        for (plain, colored) in plain.lines().zip(colored.lines()) {
            let visible_width: usize = colored
                .split('\x1b')
                .map(|part| {
                    part.split_once('m')
                        .map_or(part, |(_, text)| text)
                        .chars()
                        .count()
                })
                .sum();
            assert_eq!(plain.len(), visible_width);
            assert!(plain.len() <= 76);
        }
        assert_eq!(plain.lines().count(), 9);
        assert_eq!(colored.lines().count(), 9);
        assert!(CELLS.lines().all(|row| row.len() == 150));
    }
}
