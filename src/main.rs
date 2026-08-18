//! Shows the libneo APIs in a macOS app window.

use std::sync::Arc;

use libneo::layers::{AnchorCorner, LayerFitting, overlay, point};
use libneo::table::{FontWeight, NativeTextTableRow, native_text_table};
use libneo::theme::{Theme, ThemeMode};
use libneo::window::{
    Context, IntoElement, ParentElement, Render, Rgba, Styled, Window, WindowBuilder, WindowChrome,
    div, px, rgba, run,
};

const WINDOW_SIZE: (f32, f32) = (1500.0, 800.0);
const MINIMUM_SIZE: (f32, f32) = (900.0, 600.0);
const SCROLL_OFFSET: f32 = 4480.0;
const ROW_HEIGHT: f32 = 56.0;
const ROW_TEXT_SIZE: f32 = 22.0;
const ROW_COUNT: usize = 200;
const ROW_HUE_COUNT: usize = 12;

struct NeoWindow {
    rows: Arc<[NativeTextTableRow]>,
}

impl Render for NeoWindow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().relative().size_full().child(
            overlay(
                native_text_table("content", self.rows.clone())
                    .row_height(px(ROW_HEIGHT))
                    .font_size(px(ROW_TEXT_SIZE))
                    .font_weight(FontWeight::SEMIBOLD)
                    .initial_scroll_offset(px(SCROLL_OFFSET))
                    .w(px(WINDOW_SIZE.0))
                    .h(px(WINDOW_SIZE.1)),
            )
            .anchor(AnchorCorner::TopLeft)
            .position(point(px(0.0), px(0.0)))
            .fitting(LayerFitting::SwitchAnchor),
        )
    }
}

fn demo_rows() -> Arc<[NativeTextTableRow]> {
    (0..ROW_COUNT)
        .map(|index| {
            let hue = (index % ROW_HUE_COUNT) as f32 / ROW_HUE_COUNT as f32;
            NativeTextTableRow::new(
                format!("  Item {}", index + 1),
                hsv_color(hue, 0.85, 0.95),
                rgba(0xffffffff),
            )
        })
        .collect::<Vec<_>>()
        .into()
}

fn hsv_color(hue: f32, saturation: f32, value: f32) -> Rgba {
    let chroma = value * saturation;
    let section = hue * 6.0;
    let secondary = chroma * (1.0 - (section % 2.0 - 1.0).abs());
    let offset = value - chroma;
    let (red, green, blue) = match section.floor() as u8 {
        0 | 6 => (chroma, secondary, 0.0),
        1 => (secondary, chroma, 0.0),
        2 => (0.0, chroma, secondary),
        3 => (0.0, secondary, chroma),
        4 => (secondary, 0.0, chroma),
        _ => (chroma, 0.0, secondary),
    };
    Rgba {
        r: red + offset,
        g: green + offset,
        b: blue + offset,
        a: 1.0,
    }
}

fn main() {
    run(
        WindowBuilder::new()
            .title("NEO")
            .size(WINDOW_SIZE.0, WINDOW_SIZE.1)
            .minimum_size(MINIMUM_SIZE.0, MINIMUM_SIZE.1)
            .chrome(WindowChrome::Toolbar),
        |cx| {
            Theme::set_mode(ThemeMode::Light, cx);
            NeoWindow { rows: demo_rows() }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::{ROW_COUNT, demo_rows};

    #[test]
    fn builds_the_complete_demo_row_set() {
        let rows = demo_rows();

        assert_eq!(rows.len(), ROW_COUNT);
        assert_eq!(rows[0].text(), "  Item 1");
        assert_eq!(rows[ROW_COUNT - 1].text(), "  Item 200");
        assert_eq!(rows[0].background_color(), rows[12].background_color());
    }
}
