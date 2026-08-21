mod menu;

use libneo::theme::{Theme, ThemeMode};
use libneo::window::{
    Context, IntoElement, Render, Styled, VisualEffectMaterial, Window, WindowBackground,
    WindowBackgroundAppearance, WindowBuilder, WindowChrome, div, run,
};

const WINDOW_SIZE: (f32, f32) = (1500.0, 800.0);
const MINIMUM_SIZE: (f32, f32) = (900.0, 600.0);

struct AppRoot;

impl Render for AppRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut background = Theme::global(cx).tokens().background;
        background.a = 0.18;

        div().size_full().bg(background)
    }
}

fn run_with<V>(
    window: WindowBuilder,
    configure_app: impl FnOnce(&mut Context<V>) + 'static,
    build_root: impl FnOnce(&mut Context<V>) -> V + 'static,
) where
    V: Render + 'static,
{
    run(window, move |cx| {
        configure_app(cx);
        build_root(cx)
    });
}

fn main() {
    run_with(
        WindowBuilder::new()
            .title("NEO")
            .size(WINDOW_SIZE.0, WINDOW_SIZE.1)
            .minimum_size(MINIMUM_SIZE.0, MINIMUM_SIZE.1)
            .background_appearance(WindowBackgroundAppearance::Transparent)
            .background(WindowBackground::VisualEffect(
                VisualEffectMaterial::UnderWindowBackground,
            ))
            .chrome(WindowChrome::Toolbar),
        |cx| {
            menu::install(cx);
            Theme::set_mode(ThemeMode::FollowSystem, cx);
        },
        |_| AppRoot,
    );
}
