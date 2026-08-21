mod menu;

use libneo::theme::{Theme, ThemeMode};
use libneo::window::{
    Context, IntoElement, Render, Styled, Window, WindowBackground, WindowBackgroundAppearance,
    WindowBuilder, WindowChrome, div, run,
};

struct AppRoot;

impl Render for AppRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(Theme::global(cx).tokens().background)
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
            .size(1500.0, 800.0)
            .minimum_size(900.0, 600.0)
            .background_appearance(WindowBackgroundAppearance::Opaque)
            .background(WindowBackground::Standard)
            .chrome(WindowChrome::Toolbar),
        |cx| {
            menu::install(cx);
            Theme::set_mode(ThemeMode::FollowSystem, cx);
        },
        |_| AppRoot,
    );
}
