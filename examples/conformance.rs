use std::env;
use std::sync::Arc;

use libneo::glass::{GlassEffectContent, GlassEffectGroup, GlassEffectStyle, glass_effect};
use libneo::install;
use libneo::layers::{
    AnchorCorner, Edges, LayerFitting, LayerPositionMode, OVERLAY_PRIORITY, Overlay, overlay, point,
};
use libneo::menu::{About, Settings, Zoom};
use libneo::table::{FontWeight, NativeTextTableRow, native_text_table};
use libneo::theme::{Theme, ThemeAppearance, ThemeMode, ThemeTokens};
use libneo::toolbar::{Toolbar, ToolbarItem, ToolbarSystemItem};
use libneo::window::{
    Context, DefaultColors, IntoElement, ParentElement, Render, Rgba, Styled, VisualEffectMaterial,
    Window, WindowBackground, WindowBackgroundAppearance, WindowBuilder, WindowChrome, div, px,
    rgba, run,
};

const WINDOW_SIZE: (f32, f32) = (1500.0, 800.0);
const MINIMUM_SIZE: (f32, f32) = (900.0, 600.0);
const WINDOW_CONTROLS_POSITION: (f32, f32) = (14.0, 14.0);
const SCROLL_OFFSET: f32 = 4480.0;
const ROW_HEIGHT: f32 = 56.0;
const ROW_TEXT_SIZE: f32 = 22.0;
const ROW_COUNT: usize = 200;
const ROW_HUE_COUNT: usize = 12;
const GLASS_SIZE: (f32, f32) = (180.0, 52.0);
const GLASS_CORNER_RADIUS: f32 = 18.0;
const GLASS_GROUP_SPACING: f32 = 20.0;
const WINDOW_MARGIN: f32 = 24.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Page {
    Table,
    Glass,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Surface {
    Standard,
    UnderWindowBackground,
    HudWindow,
    Sidebar,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolbarMode {
    Declared,
    Empty,
}

impl Surface {
    fn background(self) -> WindowBackground {
        match self {
            Self::Standard => WindowBackground::Standard,
            Self::UnderWindowBackground => {
                WindowBackground::VisualEffect(VisualEffectMaterial::UnderWindowBackground)
            }
            Self::HudWindow => WindowBackground::VisualEffect(VisualEffectMaterial::HudWindow),
            Self::Sidebar => WindowBackground::VisualEffect(VisualEffectMaterial::Sidebar),
        }
    }

    fn is_visual_effect(self) -> bool {
        !matches!(self, Self::Standard)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct HarnessConfiguration {
    page: Page,
    theme: ThemeMode,
    surface: Surface,
    chrome: WindowChrome,
    toolbar: ToolbarMode,
    background_appearance: WindowBackgroundAppearance,
}

impl Default for HarnessConfiguration {
    fn default() -> Self {
        Self {
            page: Page::Table,
            theme: ThemeMode::Light,
            surface: Surface::Standard,
            chrome: WindowChrome::Toolbar,
            toolbar: ToolbarMode::Declared,
            background_appearance: WindowBackgroundAppearance::Opaque,
        }
    }
}

impl HarnessConfiguration {
    fn parse(arguments: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let mut configuration = Self::default();
        for argument in arguments {
            let argument = argument.as_ref();
            let (option, value) = argument
                .split_once('=')
                .unwrap_or_else(|| panic!("expected --option=value, received {argument}"));
            match option {
                "--page" => {
                    configuration.page = match value {
                        "table" => Page::Table,
                        "glass" => Page::Glass,
                        _ => panic!("unknown page {value}"),
                    };
                }
                "--theme" => {
                    configuration.theme = match value {
                        "system" => ThemeMode::FollowSystem,
                        "light" => ThemeMode::Light,
                        "dark" => ThemeMode::Dark,
                        _ => panic!("unknown theme {value}"),
                    };
                }
                "--surface" => {
                    configuration.surface = match value {
                        "standard" => Surface::Standard,
                        "under-window" => Surface::UnderWindowBackground,
                        "hud" => Surface::HudWindow,
                        "sidebar" => Surface::Sidebar,
                        _ => panic!("unknown surface {value}"),
                    };
                }
                "--chrome" => {
                    configuration.chrome = match value {
                        "transparent" => WindowChrome::TransparentTitleBar,
                        "toolbar" => WindowChrome::Toolbar,
                        _ => panic!("unknown chrome {value}"),
                    };
                }
                "--background" => {
                    configuration.background_appearance = match value {
                        "opaque" => WindowBackgroundAppearance::Opaque,
                        "transparent" => WindowBackgroundAppearance::Transparent,
                        "blurred" => WindowBackgroundAppearance::Blurred,
                        _ => panic!("unknown background appearance {value}"),
                    };
                }
                "--toolbar" => {
                    configuration.toolbar = match value {
                        "declared" => ToolbarMode::Declared,
                        "empty" => ToolbarMode::Empty,
                        _ => panic!("unknown toolbar mode {value}"),
                    };
                }
                _ => panic!("unknown option {option}"),
            }
        }
        configuration
    }
}

struct ConformanceHarness {
    configuration: HarnessConfiguration,
    rows: Arc<[NativeTextTableRow]>,
}

impl Render for ConformanceHarness {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        let mode = theme_mode_name(theme.mode());
        let appearance = appearance_name(theme.appearance());
        let tokens = theme.tokens();
        let default_background = cx.default_colors().background;
        let light = ThemeTokens::light();
        let dark = ThemeTokens::dark();
        let group = GlassEffectGroup::new("anchor-probes").spacing(px(GLASS_GROUP_SPACING));
        let margin = Edges::all(px(WINDOW_MARGIN));
        let mut background = tokens.background;
        if self.configuration.surface.is_visual_effect() {
            background.a = 0.18;
        }

        let root = div().relative().size_full().bg(background);
        let root = match self.configuration.page {
            Page::Table => root.child(
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
            ),
            Page::Glass => root,
        };

        root.child(anchor_probe(
            "top-left",
            "Top left",
            AnchorCorner::TopLeft,
            (24.0, 80.0),
            LayerPositionMode::Window,
            LayerFitting::SwitchAnchor,
            GlassEffectStyle::Regular,
            group.clone(),
            tokens.glass_tint,
        ))
        .child(anchor_probe(
            "top-center",
            "Top center",
            AnchorCorner::TopCenter,
            (WINDOW_SIZE.0 / 2.0, 80.0),
            LayerPositionMode::Local,
            LayerFitting::SnapToWindow,
            GlassEffectStyle::Clear,
            group.clone(),
            light.glass_tint,
        ))
        .child(anchor_probe(
            "top-right",
            "Top right",
            AnchorCorner::TopRight,
            (WINDOW_SIZE.0 - 24.0, 80.0),
            LayerPositionMode::Window,
            LayerFitting::SnapToWindowWithMargin(margin),
            GlassEffectStyle::Regular,
            group.clone(),
            dark.glass_tint,
        ))
        .child(anchor_probe(
            "left-center",
            "Left center",
            AnchorCorner::LeftCenter,
            (24.0, WINDOW_SIZE.1 / 2.0),
            LayerPositionMode::Local,
            LayerFitting::SnapToWindow,
            GlassEffectStyle::Clear,
            group.clone(),
            tokens.accent,
        ))
        .child(anchor_probe(
            "right-center",
            "Right center",
            AnchorCorner::RightCenter,
            (WINDOW_SIZE.0 - 24.0, WINDOW_SIZE.1 / 2.0),
            LayerPositionMode::Window,
            LayerFitting::SnapToWindowWithMargin(margin),
            GlassEffectStyle::Regular,
            group.clone(),
            tokens.grouped_background,
        ))
        .child(anchor_probe(
            "bottom-left",
            "Bottom left",
            AnchorCorner::BottomLeft,
            (24.0, WINDOW_SIZE.1 - 24.0),
            LayerPositionMode::Local,
            LayerFitting::SwitchAnchor,
            GlassEffectStyle::Clear,
            group.clone(),
            tokens.text,
        ))
        .child(anchor_probe(
            "bottom-center",
            "Bottom center",
            AnchorCorner::BottomCenter,
            (WINDOW_SIZE.0 / 2.0, WINDOW_SIZE.1 - 24.0),
            LayerPositionMode::Window,
            LayerFitting::SnapToWindow,
            GlassEffectStyle::Regular,
            group.clone(),
            tokens.muted_text,
        ))
        .child(
            anchor_probe(
                "bottom-right",
                &format!("{mode} / {appearance}"),
                AnchorCorner::BottomRight,
                (WINDOW_SIZE.0 - 24.0, WINDOW_SIZE.1 - 24.0),
                LayerPositionMode::Local,
                LayerFitting::SnapToWindowWithMargin(margin),
                GlassEffectStyle::Clear,
                group,
                default_background,
            )
            .offset(point(px(-8.0), px(-8.0)))
            .priority(OVERLAY_PRIORITY + 2),
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn anchor_probe(
    id: &str,
    label: &str,
    anchor: AnchorCorner,
    position: (f32, f32),
    position_mode: LayerPositionMode,
    fitting: LayerFitting,
    style: GlassEffectStyle,
    group: GlassEffectGroup,
    tint: Rgba,
) -> Overlay {
    overlay(
        glass_effect(id)
            .effect_style(style)
            .corner_radius(px(GLASS_CORNER_RADIUS))
            .tint(tint)
            .group(group)
            .content(GlassEffectContent::Label(label.to_owned()))
            .w(px(GLASS_SIZE.0))
            .h(px(GLASS_SIZE.1)),
    )
    .anchor(anchor)
    .position(point(px(position.0), px(position.1)))
    .position_mode(position_mode)
    .fitting(fitting)
    .priority(OVERLAY_PRIORITY + 1)
}

fn theme_mode_name(mode: ThemeMode) -> &'static str {
    match mode {
        ThemeMode::FollowSystem => "system",
        ThemeMode::Light => "light",
        ThemeMode::Dark => "dark",
    }
}

fn appearance_name(appearance: ThemeAppearance) -> &'static str {
    match appearance {
        ThemeAppearance::Light => "light appearance",
        ThemeAppearance::Dark => "dark appearance",
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

fn harness_toolbar(mode: ToolbarMode) -> Toolbar {
    let toolbar = Toolbar::new(match mode {
        ToolbarMode::Declared => "conformance.declared-toolbar",
        ToolbarMode::Empty => "conformance.empty-toolbar",
    });
    match mode {
        ToolbarMode::Declared => toolbar.items([
            ToolbarItem::action("conformance.about", "About libneo", About).symbol("info.circle"),
            ToolbarItem::action("conformance.zoom", "Zoom", Zoom)
                .symbol("arrow.up.left.and.arrow.down.right"),
            ToolbarItem::system(ToolbarSystemItem::FlexibleSpace),
            ToolbarItem::action("conformance.disabled", "Disabled", Settings)
                .symbol("nosign")
                .enabled(false),
        ]),
        ToolbarMode::Empty => toolbar,
    }
}

fn main() {
    let configuration = HarnessConfiguration::parse(env::args().skip(1));
    let window = WindowBuilder::new()
        .title("libneo conformance")
        .size(WINDOW_SIZE.0, WINDOW_SIZE.1)
        .minimum_size(MINIMUM_SIZE.0, MINIMUM_SIZE.1)
        .window_controls_position(WINDOW_CONTROLS_POSITION.0, WINDOW_CONTROLS_POSITION.1)
        .background_appearance(configuration.background_appearance)
        .background(configuration.surface.background())
        .chrome(configuration.chrome);
    let window = if configuration.chrome == WindowChrome::Toolbar {
        window.toolbar(harness_toolbar(configuration.toolbar))
    } else {
        window
    };

    run(window, move |cx| {
        install(cx);
        Theme::set_mode(configuration.theme, cx);
        ConformanceHarness {
            configuration,
            rows: demo_rows(),
        }
    });
}

#[cfg(test)]
mod tests {
    use libneo::glass::GlassEffectStyle;
    use libneo::layers::{LayerFitting, LayerPositionMode};
    use libneo::theme::ThemeMode;
    use libneo::window::{
        VisualEffectMaterial, WindowBackground, WindowBackgroundAppearance, WindowChrome, rgba,
    };

    use super::{HarnessConfiguration, Page, ROW_COUNT, Surface, ToolbarMode, demo_rows};

    #[test]
    fn builds_the_complete_demo_row_set() {
        let rows = demo_rows();

        assert_eq!(rows.len(), ROW_COUNT);
        assert_eq!(rows[0].text(), "  Item 1");
        assert_eq!(rows[ROW_COUNT - 1].text(), "  Item 200");
        assert_eq!(rows[0].background_color(), rows[12].background_color());
        assert_eq!(rows[0].foreground_color(), rgba(0xffffffff));
    }

    #[test]
    fn parses_every_launch_variant() {
        let system = HarnessConfiguration::parse([
            "--page=glass",
            "--theme=system",
            "--surface=under-window",
            "--chrome=transparent",
            "--background=transparent",
            "--toolbar=empty",
        ]);
        let dark = HarnessConfiguration::parse([
            "--theme=dark",
            "--surface=hud",
            "--chrome=toolbar",
            "--background=blurred",
        ]);
        let sidebar = HarnessConfiguration::parse([
            "--theme=light",
            "--surface=sidebar",
            "--background=opaque",
        ]);

        assert_eq!(system.page, Page::Glass);
        assert_eq!(system.theme, ThemeMode::FollowSystem);
        assert_eq!(system.surface, Surface::UnderWindowBackground);
        assert_eq!(system.chrome, WindowChrome::TransparentTitleBar);
        assert_eq!(system.toolbar, ToolbarMode::Empty);
        assert_eq!(
            system.background_appearance,
            WindowBackgroundAppearance::Transparent
        );
        assert_eq!(dark.theme, ThemeMode::Dark);
        assert_eq!(dark.surface, Surface::HudWindow);
        assert_eq!(
            dark.background_appearance,
            WindowBackgroundAppearance::Blurred
        );
        assert_eq!(sidebar.surface, Surface::Sidebar);
    }

    #[test]
    fn maps_every_native_surface_material() {
        assert_eq!(Surface::Standard.background(), WindowBackground::Standard);
        assert!(!Surface::Standard.is_visual_effect());
        assert_eq!(
            Surface::UnderWindowBackground.background(),
            WindowBackground::VisualEffect(VisualEffectMaterial::UnderWindowBackground)
        );
        assert_eq!(
            Surface::HudWindow.background(),
            WindowBackground::VisualEffect(VisualEffectMaterial::HudWindow)
        );
        assert_eq!(
            Surface::Sidebar.background(),
            WindowBackground::VisualEffect(VisualEffectMaterial::Sidebar)
        );
        assert!(Surface::UnderWindowBackground.is_visual_effect());
        assert!(Surface::HudWindow.is_visual_effect());
        assert!(Surface::Sidebar.is_visual_effect());
    }

    #[test]
    fn exercises_public_defaults() {
        assert_eq!(GlassEffectStyle::default(), GlassEffectStyle::Regular);
        assert_eq!(LayerPositionMode::default(), LayerPositionMode::Window);
        assert_eq!(LayerFitting::default(), LayerFitting::SwitchAnchor);
        assert_eq!(ThemeMode::default(), ThemeMode::FollowSystem);
        assert_eq!(WindowChrome::default(), WindowChrome::TransparentTitleBar);
        assert_eq!(
            WindowBackgroundAppearance::default(),
            WindowBackgroundAppearance::Opaque
        );
    }
}
