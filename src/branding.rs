//! Terminal cells sampled from the logo, with the SVG's light wordmark.

const CELLS: &str = include_str!("branding.txt");

pub fn logo(color: bool) -> String {
    CELLS
        .lines()
        .map(|row| {
            let mut line = String::new();
            let mut previous = '.';
            for cell in row.trim_end_matches('.').chars() {
                if color && cell != previous {
                    line.push_str(match cell {
                        'c' => "\x1b[48;2;34;211;238m",
                        't' => "\x1b[48;2;0;168;223m",
                        'b' => "\x1b[48;2;0;143;212m",
                        'v' => "\x1b[48;2;167;139;250m",
                        'p' => "\x1b[48;2;128;112;237m",
                        'w' => "\x1b[48;2;231;236;255m",
                        _ => "\x1b[0m",
                    });
                }
                line.push(if color || cell == '.' { ' ' } else { '#' });
                previous = cell;
            }
            if color {
                line.push_str("\x1b[0m");
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colored_logo_resets_the_background_before_help_text() {
        let logo = logo(true);
        assert!(logo.contains("\x1b[48;2;34;211;238m"));
        assert!(logo.contains("\x1b[48;2;167;139;250m"));
        assert!(logo.lines().all(|line| line.ends_with("\x1b[0m")));
        assert!(logo.is_ascii());
    }
}
