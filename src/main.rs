mod menu;

use libneo::window::{
    Context, IntoElement, Render, Styled, VisualEffectMaterial, Window, WindowBackground,
    WindowBackgroundAppearance, WindowBuilder, WindowChrome, div, run,
};
use neo::theme::{Theme, ThemeMode};

const WINDOW_SIZE: (f32, f32) = (1500.0, 800.0);
const MINIMUM_SIZE: (f32, f32) = (900.0, 600.0);
const WINDOW_CONTROLS_POSITION: (f32, f32) = (14.0, 14.0);
const CONTENT_BACKGROUND_ALPHA: f32 = 0.18;

struct AppRoot;

impl Render for AppRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut background = Theme::global(cx).tokens(window).background;
        background.a = CONTENT_BACKGROUND_ALPHA;

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
        WindowBuilder::new(
            "NEO",
            WINDOW_SIZE,
            MINIMUM_SIZE,
            WINDOW_CONTROLS_POSITION,
            WindowChrome::TransparentTitleBar,
            WindowBackground::VisualEffect(VisualEffectMaterial::UnderWindowBackground),
            WindowBackgroundAppearance::Transparent,
        ),
        |cx| {
            menu::install(cx);
            Theme::install(ThemeMode::FollowSystem, cx);
        },
        |_| AppRoot,
    );
}
