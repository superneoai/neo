use std::env;
use std::sync::Arc;

use libneo::glass::{
    GlassEffectConfiguration, GlassEffectContent, GlassEffectGroup, GlassEffectStyle, glass_effect,
};
use libneo::install;
use libneo::layers::{
    AnchorCorner, Edges, LayerFitting, LayerPositionMode, Overlay, OverlayConfiguration, overlay,
    point,
};
use libneo::menu::{About, Settings, Zoom};
use libneo::table::{FontWeight, NativeTextTableRow, TextTableConfiguration, native_text_table};
use libneo::toolbar::{
    Toolbar, ToolbarConfiguration, ToolbarDisplayMode, ToolbarItem, ToolbarStyle, ToolbarSystemItem,
};
use libneo::window::{
    Context, IntoElement, ParentElement, Render, Rgba, Styled, VisualEffectMaterial, Window,
    WindowBackground, WindowBackgroundAppearance, WindowBuilder, WindowChrome, WindowCornerRadius,
    div, px, rgba, run,
};
use neo::theme::{Theme, ThemeAppearance, ThemeMode, ThemeTokens};

const WINDOW_SIZE: (f32, f32) = (1500.0, 800.0);
const MINIMUM_SIZE: (f32, f32) = (900.0, 600.0);
const WINDOW_CONTROLS_POSITION: (f32, f32) = (14.0, 14.0);
const CONTENT_BACKGROUND_ALPHA: f32 = 0.18;
const SCROLL_OFFSET: f32 = 4480.0;
const ROW_HEIGHT: f32 = 56.0;
const ROW_TEXT_SIZE: f32 = 22.0;
const ROW_COUNT: usize = 200;
const ROW_HUE_COUNT: usize = 12;
const GLASS_SIZE: (f32, f32) = (180.0, 52.0);
const GLASS_CORNER_RADIUS: f32 = 18.0;
const GLASS_GROUP_SPACING: f32 = 20.0;
const WINDOW_MARGIN: f32 = 24.0;
const CONTENT_OVERLAY_PRIORITY: usize = 1;
const GLASS_OVERLAY_PRIORITY: usize = 2;
const FOREGROUND_OVERLAY_PRIORITY: usize = 3;

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
enum ChromeMode {
    TransparentTitleBar,
    Toolbar,
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

impl ChromeMode {
    fn chrome(self, toolbar: ToolbarMode) -> WindowChrome {
        match self {
            Self::TransparentTitleBar => WindowChrome::TransparentTitleBar,
            Self::Toolbar => WindowChrome::Toolbar(harness_toolbar(toolbar)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct HarnessConfiguration {
    page: Page,
    theme: ThemeMode,
    surface: Surface,
    chrome: ChromeMode,
    toolbar: ToolbarMode,
    background_appearance: WindowBackgroundAppearance,
}

impl HarnessConfiguration {
    fn standard() -> Self {
        Self {
            page: Page::Table,
            theme: ThemeMode::Light,
            surface: Surface::Standard,
            chrome: ChromeMode::Toolbar,
            toolbar: ToolbarMode::Declared,
            background_appearance: WindowBackgroundAppearance::Opaque,
        }
    }

    fn parse(arguments: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let mut configuration = Self::standard();
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
                        "transparent" => ChromeMode::TransparentTitleBar,
                        "toolbar" => ChromeMode::Toolbar,
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        let mode = theme_mode_name(theme.mode());
        let appearance = appearance_name(theme.appearance(window));
        let tokens = *theme.tokens(window);
        let light = ThemeTokens::light();
        let dark = ThemeTokens::dark();
        let group = GlassEffectGroup::new("anchor-probes", px(GLASS_GROUP_SPACING));
        let margin = Edges::all(px(WINDOW_MARGIN));
        let mut background = tokens.background;
        if self.configuration.surface.is_visual_effect() {
            background.a = CONTENT_BACKGROUND_ALPHA;
        }

        let root = div().relative().size_full().bg(background);
        let root = match self.configuration.page {
            Page::Table => root.child(overlay(
                native_text_table("content", text_table_configuration(self.rows.clone()))
                    .w(px(WINDOW_SIZE.0))
                    .h(px(WINDOW_SIZE.1)),
                OverlayConfiguration {
                    anchor: AnchorCorner::TopLeft,
                    position: Some(point(px(0.0), px(0.0))),
                    offset: None,
                    position_mode: LayerPositionMode::Window,
                    fitting: LayerFitting::SwitchAnchor,
                    priority: CONTENT_OVERLAY_PRIORITY,
                },
            )),
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
                dark.background,
            )
            .offset(point(px(-8.0), px(-8.0)))
            .priority(FOREGROUND_OVERLAY_PRIORITY),
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
        glass_effect(id, glass_effect_configuration(label, style, group, tint))
            .w(px(GLASS_SIZE.0))
            .h(px(GLASS_SIZE.1)),
        OverlayConfiguration {
            anchor,
            position: Some(point(px(position.0), px(position.1))),
            offset: None,
            position_mode,
            fitting,
            priority: GLASS_OVERLAY_PRIORITY,
        },
    )
}

fn glass_effect_configuration(
    label: &str,
    style: GlassEffectStyle,
    group: GlassEffectGroup,
    tint: Rgba,
) -> GlassEffectConfiguration {
    GlassEffectConfiguration {
        style,
        corner_radius: px(GLASS_CORNER_RADIUS),
        tint: Some(tint),
        group: Some(group),
        content: Some(GlassEffectContent::Label(label.to_owned())),
    }
}

fn text_table_configuration(rows: Arc<[NativeTextTableRow]>) -> TextTableConfiguration {
    TextTableConfiguration {
        rows,
        row_height: px(ROW_HEIGHT),
        font_size: px(ROW_TEXT_SIZE),
        font_weight: FontWeight::SEMIBOLD,
        initial_scroll_offset: px(SCROLL_OFFSET),
    }
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

fn toolbar_configuration() -> ToolbarConfiguration {
    ToolbarConfiguration {
        display_mode: ToolbarDisplayMode::IconAndLabel,
        style: ToolbarStyle::Unified,
        autosaves_configuration: false,
        allows_user_customization: false,
    }
}

fn harness_toolbar(mode: ToolbarMode) -> Toolbar {
    let toolbar = Toolbar::new(
        match mode {
            ToolbarMode::Declared => "conformance.declared-toolbar",
            ToolbarMode::Empty => "conformance.empty-toolbar",
        },
        toolbar_configuration(),
    );
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
    let window = WindowBuilder::new(
        "libneo conformance",
        WINDOW_SIZE,
        MINIMUM_SIZE,
        WINDOW_CONTROLS_POSITION,
        WindowCornerRadius::System,
        configuration.chrome.chrome(configuration.toolbar),
        configuration.surface.background(),
        configuration.background_appearance,
    );

    run(window, move |cx| {
        install(cx);
        Theme::install(configuration.theme, cx);
        ConformanceHarness {
            configuration,
            rows: demo_rows(),
        }
    });
}

#[cfg(test)]
mod tests {
    use libneo::glass::{GlassEffectContent, GlassEffectGroup, GlassEffectStyle};
    use libneo::layers::{AnchorCorner, LayerFitting, LayerPositionMode, point};
    use libneo::table::FontWeight;
    use libneo::toolbar::{ToolbarDisplayMode, ToolbarStyle};
    use libneo::window::{
        VisualEffectMaterial, WindowBackground, WindowBackgroundAppearance, WindowChrome, px, rgba,
    };
    use neo::theme::ThemeMode;

    use super::{
        CONTENT_OVERLAY_PRIORITY, ChromeMode, GLASS_CORNER_RADIUS, GLASS_GROUP_SPACING,
        HarnessConfiguration, Page, ROW_COUNT, ROW_HEIGHT, ROW_TEXT_SIZE, SCROLL_OFFSET, Surface,
        ToolbarMode, demo_rows, glass_effect_configuration, text_table_configuration,
        toolbar_configuration,
    };

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
        assert_eq!(system.chrome, ChromeMode::TransparentTitleBar);
        assert_eq!(system.toolbar, ToolbarMode::Empty);
        assert_eq!(
            system.background_appearance,
            WindowBackgroundAppearance::Transparent
        );
        assert_eq!(dark.theme, ThemeMode::Dark);
        assert_eq!(dark.surface, Surface::HudWindow);
        assert_eq!(dark.chrome, ChromeMode::Toolbar);
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
    fn uses_neo_owned_explicit_configurations() {
        let harness = HarnessConfiguration::standard();
        assert_eq!(harness.page, Page::Table);
        assert_eq!(harness.theme, ThemeMode::Light);
        assert_eq!(harness.surface, Surface::Standard);
        assert_eq!(harness.chrome, ChromeMode::Toolbar);
        assert_eq!(harness.toolbar, ToolbarMode::Declared);
        assert_eq!(
            harness.background_appearance,
            WindowBackgroundAppearance::Opaque
        );

        let rows = demo_rows();
        let table = text_table_configuration(rows.clone());
        assert_eq!(table.rows, rows);
        assert_eq!(table.row_height, px(ROW_HEIGHT));
        assert_eq!(table.font_size, px(ROW_TEXT_SIZE));
        assert_eq!(table.font_weight, FontWeight::SEMIBOLD);
        assert_eq!(table.initial_scroll_offset, px(SCROLL_OFFSET));

        let group = GlassEffectGroup::new("test-group", px(GLASS_GROUP_SPACING));
        let glass = glass_effect_configuration(
            "Explicit",
            GlassEffectStyle::Regular,
            group.clone(),
            rgba(0x48c9b044),
        );
        assert_eq!(glass.style, GlassEffectStyle::Regular);
        assert_eq!(glass.corner_radius, px(GLASS_CORNER_RADIUS));
        assert_eq!(glass.tint, Some(rgba(0x48c9b044)));
        assert_eq!(glass.group, Some(group));
        assert_eq!(
            glass.content,
            Some(GlassEffectContent::Label("Explicit".to_owned()))
        );

        let overlay = libneo::layers::OverlayConfiguration {
            anchor: AnchorCorner::TopLeft,
            position: Some(point(px(0.0), px(0.0))),
            offset: None,
            position_mode: LayerPositionMode::Window,
            fitting: LayerFitting::SwitchAnchor,
            priority: CONTENT_OVERLAY_PRIORITY,
        };
        assert_eq!(overlay.anchor, AnchorCorner::TopLeft);
        assert_eq!(overlay.position, Some(point(px(0.0), px(0.0))));
        assert_eq!(overlay.offset, None);
        assert_eq!(overlay.position_mode, LayerPositionMode::Window);
        assert_eq!(overlay.fitting, LayerFitting::SwitchAnchor);
        assert_eq!(overlay.priority, CONTENT_OVERLAY_PRIORITY);

        let toolbar = toolbar_configuration();
        assert_eq!(toolbar.display_mode, ToolbarDisplayMode::IconAndLabel);
        assert_eq!(toolbar.style, ToolbarStyle::Unified);
        assert!(!toolbar.autosaves_configuration);
        assert!(!toolbar.allows_user_customization);

        assert!(matches!(
            ChromeMode::TransparentTitleBar.chrome(ToolbarMode::Empty),
            WindowChrome::TransparentTitleBar
        ));
        assert!(matches!(
            ChromeMode::Toolbar.chrome(ToolbarMode::Declared),
            WindowChrome::Toolbar(_)
        ));
    }
}
