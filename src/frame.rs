use serde::{Deserialize, Serialize};
use vt100::{Color as TerminalColor, Screen};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

pub const DEFAULT_FOREGROUND: Color = Color {
    r: 201,
    g: 209,
    b: 217,
};
pub const DEFAULT_BACKGROUND: Color = Color {
    r: 13,
    g: 17,
    b: 23,
};

impl Color {
    pub fn css(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Attributes {
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: Option<Underline>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Underline {
    Single,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Cell {
    pub x: u16,
    pub y: u16,
    pub text: String,
    pub width: u16,
    pub foreground: Color,
    pub background: Color,
    pub attributes: Attributes,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub color: Color,
    pub blinking: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Frame {
    pub version: u8,
    pub cols: u16,
    pub rows: u16,
    pub foreground: Color,
    pub background: Color,
    pub cursor: Option<Cursor>,
    pub cells: Vec<Cell>,
}

impl Frame {
    pub fn has_visible_content(&self) -> bool {
        self.cells
            .iter()
            .any(|cell| !cell.text.trim().is_empty() || cell.background != self.background)
    }

    pub fn text(&self) -> String {
        let mut rows =
            vec![vec![String::from(" "); usize::from(self.cols)]; usize::from(self.rows)];
        for cell in &self.cells {
            if cell.text.is_empty() {
                continue;
            }
            rows[usize::from(cell.y)][usize::from(cell.x)] = cell.text.clone();
            if cell.width == 2 && cell.x + 1 < self.cols {
                rows[usize::from(cell.y)][usize::from(cell.x + 1)].clear();
            }
        }
        rows.into_iter()
            .map(|line| line.join("").trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_owned()
    }
}

pub fn from_screen(screen: &Screen) -> Frame {
    let (rows, cols) = screen.size();
    let foreground = DEFAULT_FOREGROUND;
    let background = DEFAULT_BACKGROUND;
    let mut cells = Vec::new();
    for y in 0..rows {
        for x in 0..cols {
            let Some(cell) = screen.cell(y, x) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut cell_foreground = resolve_color(cell.fgcolor(), foreground);
            let mut cell_background = resolve_color(cell.bgcolor(), background);
            if cell.inverse() {
                std::mem::swap(&mut cell_foreground, &mut cell_background);
            }
            let attributes = Attributes {
                bold: cell.bold(),
                italic: cell.italic(),
                faint: cell.dim(),
                invisible: false,
                strikethrough: false,
                overline: false,
                underline: cell.underline().then_some(Underline::Single),
            };
            let text = cell.contents().to_owned();
            if !text.is_empty() || cell_background != background || has_attributes(&attributes) {
                cells.push(Cell {
                    x,
                    y,
                    text,
                    width: if cell.is_wide() { 2 } else { 1 },
                    foreground: cell_foreground,
                    background: cell_background,
                    attributes,
                });
            }
        }
    }
    let (cursor_y, cursor_x) = screen.cursor_position();
    Frame {
        version: 1,
        cols,
        rows,
        foreground,
        background,
        cursor: (!screen.hide_cursor()).then_some(Cursor {
            x: cursor_x,
            y: cursor_y,
            color: foreground,
            blinking: false,
        }),
        cells,
    }
}

fn has_attributes(attributes: &Attributes) -> bool {
    attributes.bold || attributes.italic || attributes.faint || attributes.underline.is_some()
}

fn resolve_color(color: TerminalColor, default: Color) -> Color {
    match color {
        TerminalColor::Default => default,
        TerminalColor::Rgb(r, g, b) => Color { r, g, b },
        TerminalColor::Idx(index) => indexed_color(index),
    }
}

fn indexed_color(index: u8) -> Color {
    const ANSI: [Color; 16] = [
        Color { r: 0, g: 0, b: 0 },
        Color {
            r: 205,
            g: 49,
            b: 49,
        },
        Color {
            r: 13,
            g: 188,
            b: 121,
        },
        Color {
            r: 229,
            g: 229,
            b: 16,
        },
        Color {
            r: 36,
            g: 114,
            b: 200,
        },
        Color {
            r: 188,
            g: 63,
            b: 188,
        },
        Color {
            r: 17,
            g: 168,
            b: 205,
        },
        Color {
            r: 229,
            g: 229,
            b: 229,
        },
        Color {
            r: 102,
            g: 102,
            b: 102,
        },
        Color {
            r: 241,
            g: 76,
            b: 76,
        },
        Color {
            r: 35,
            g: 209,
            b: 139,
        },
        Color {
            r: 245,
            g: 245,
            b: 67,
        },
        Color {
            r: 59,
            g: 142,
            b: 234,
        },
        Color {
            r: 214,
            g: 112,
            b: 214,
        },
        Color {
            r: 41,
            g: 184,
            b: 219,
        },
        Color {
            r: 255,
            g: 255,
            b: 255,
        },
    ];
    if index < 16 {
        return ANSI[usize::from(index)];
    }
    if index >= 232 {
        let value = 8 + (index - 232) * 10;
        return Color {
            r: value,
            g: value,
            b: value,
        };
    }
    let value = index - 16;
    let channel = |component: u8| {
        if component == 0 {
            0
        } else {
            55 + component * 40
        }
    };
    Color {
        r: channel(value / 36),
        g: channel((value % 36) / 6),
        b: channel(value % 6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_truecolor_backgrounds_and_text() {
        let mut parser = vt100::Parser::new(3, 20, 0);
        parser.process(b"\x1b[48;2;30;34;42m\x1b[38;2;196;215;240m Hi \x1b[0m");

        let frame = from_screen(parser.screen());

        assert_eq!(frame.text(), " Hi");
        assert_eq!(
            frame.cells[0].background,
            Color {
                r: 30,
                g: 34,
                b: 42
            }
        );
        assert_eq!(
            frame.cells[0].foreground,
            Color {
                r: 196,
                g: 215,
                b: 240
            }
        );
    }

    #[test]
    fn maps_xterm_color_cube_values() {
        assert_eq!(
            indexed_color(1),
            Color {
                r: 205,
                g: 49,
                b: 49
            }
        );
        assert_eq!(
            indexed_color(214),
            Color {
                r: 255,
                g: 175,
                b: 0
            }
        );
        assert_eq!(
            indexed_color(244),
            Color {
                r: 128,
                g: 128,
                b: 128
            }
        );
    }
}
