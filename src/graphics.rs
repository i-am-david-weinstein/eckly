//! Inline images drawn with a terminal graphics protocol (Kitty, Sixel,
//! iTerm2) through ratatui-image, behind the `terminal_graphics` option.

use std::fmt;

use image::DynamicImage;
use ratatui::layout::Size;
use ratatui_image::{FontSize, picker::Picker, sliced::SlicedProtocol};

/// An image encoded for the terminal at one reading column width.
pub struct Graphic {
    pub width: usize,
    pub protocol: SlicedProtocol,
}

impl fmt::Debug for Graphic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Graphic")
            .field("width", &self.width)
            .field("size", &self.protocol.size())
            .finish()
    }
}

/// Cells an image covers at its natural size, shrunk to `max_columns` but
/// never scaled up.
pub fn fit(pixels: (u32, u32), font: FontSize, max_columns: usize) -> Size {
    let width = u64::from(pixels.0.max(1));
    let height = u64::from(pixels.1.max(1));
    let cell_width = u64::from(font.width.max(1));
    let cell_height = u64::from(font.height.max(1));
    let max_columns = max_columns.max(1) as u64;

    let natural_columns = width.div_ceil(cell_width);
    let (columns, height) = if natural_columns <= max_columns {
        (natural_columns, height)
    } else {
        (max_columns, height * max_columns * cell_width / width)
    };
    let rows = height.div_ceil(cell_height).max(1);
    Size::new(
        columns.min(u64::from(u16::MAX)) as u16,
        rows.min(u64::from(u16::MAX)) as u16,
    )
}

pub fn encode(
    picker: &Picker,
    image: DynamicImage,
    width: usize,
) -> Result<Graphic, String> {
    let size = fit((image.width(), image.height()), picker.font_size(), width);
    let protocol = SlicedProtocol::new(picker, image, Some(size))
        .map_err(|err| err.to_string())?;
    Ok(Graphic { width, protocol })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FONT: FontSize = FontSize::new(10, 20);

    #[test]
    fn wide_images_shrink_to_the_column() {
        assert_eq!(fit((2000, 1000), FONT, 100), Size::new(100, 25));
    }

    #[test]
    fn small_images_keep_their_size() {
        assert_eq!(fit((50, 45), FONT, 100), Size::new(5, 3));
    }

    #[test]
    fn tiny_images_cover_at_least_one_cell() {
        assert_eq!(fit((1, 1), FONT, 100), Size::new(1, 1));
        assert_eq!(fit((4000, 10), FONT, 40), Size::new(40, 1));
    }

    #[test]
    fn encodes_at_the_fitted_size() {
        let image = DynamicImage::new_rgba8(2000, 1000);
        let graphic = encode(&Picker::halfblocks(), image, 100).unwrap();
        assert_eq!(graphic.width, 100);
        assert_eq!(graphic.protocol.size(), Size::new(100, 25));
    }
}
