use gpui::{App, Global, Rgba, Window, WindowAppearance, rgba};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThemeMode {
    FollowSystem,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThemeAppearance {
    Light,
    Dark,
}

impl From<WindowAppearance> for ThemeAppearance {
    fn from(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::Light,
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Dark,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThemeTokens {
    pub background: Rgba,
    pub grouped_background: Rgba,
    pub text: Rgba,
    pub muted_text: Rgba,
    pub accent: Rgba,
    pub glass_tint: Rgba,
}

impl ThemeTokens {
    pub fn light() -> Self {
        Self {
            background: rgba(0xf7faf9ff),
            grouped_background: rgba(0xe9f1efff),
            text: rgba(0x16201eff),
            muted_text: rgba(0x60716dff),
            accent: rgba(0x007f73ff),
            glass_tint: rgba(0x48c9b044),
        }
    }

    pub fn dark() -> Self {
        Self {
            background: rgba(0x0e1716ff),
            grouped_background: rgba(0x182422ff),
            text: rgba(0xeaf5f2ff),
            muted_text: rgba(0x94aaa5ff),
            accent: rgba(0x56d5c2ff),
            glass_tint: rgba(0x34bfae55),
        }
    }
}

pub struct Theme {
    mode: ThemeMode,
    light: ThemeTokens,
    dark: ThemeTokens,
}

impl Global for Theme {}

impl Theme {
    pub fn install(mode: ThemeMode, cx: &mut App) {
        cx.set_window_appearance(window_appearance_override(mode));
        cx.set_global(Self {
            mode,
            light: ThemeTokens::light(),
            dark: ThemeTokens::dark(),
        });
        cx.refresh_windows();
    }

    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    pub fn set_mode(mode: ThemeMode, cx: &mut App) {
        cx.set_window_appearance(window_appearance_override(mode));
        cx.global_mut::<Self>().mode = mode;
        cx.refresh_windows();
    }

    pub fn mode(&self) -> ThemeMode {
        self.mode
    }

    pub fn appearance(&self, window: &Window) -> ThemeAppearance {
        match self.mode {
            ThemeMode::FollowSystem => window.appearance().into(),
            ThemeMode::Light => ThemeAppearance::Light,
            ThemeMode::Dark => ThemeAppearance::Dark,
        }
    }

    pub fn tokens(&self, window: &Window) -> &ThemeTokens {
        match self.appearance(window) {
            ThemeAppearance::Light => &self.light,
            ThemeAppearance::Dark => &self.dark,
        }
    }
}

fn window_appearance_override(mode: ThemeMode) -> Option<WindowAppearance> {
    match mode {
        ThemeMode::FollowSystem => None,
        ThemeMode::Light => Some(WindowAppearance::Light),
        ThemeMode::Dark => Some(WindowAppearance::Dark),
    }
}

#[cfg(test)]
mod tests {
    use gpui::{WindowAppearance, rgba};

    use super::{ThemeAppearance, ThemeMode, ThemeTokens, window_appearance_override};

    #[test]
    fn owns_distinct_sea_glass_palettes() {
        let light = ThemeTokens::light();
        let dark = ThemeTokens::dark();

        assert_eq!(light.background, rgba(0xf7faf9ff));
        assert_eq!(light.accent, rgba(0x007f73ff));
        assert_eq!(light.glass_tint, rgba(0x48c9b044));
        assert_eq!(dark.background, rgba(0x0e1716ff));
        assert_eq!(dark.accent, rgba(0x56d5c2ff));
        assert_eq!(dark.glass_tint, rgba(0x34bfae55));
        assert_ne!(light, dark);
    }

    #[test]
    fn maps_modes_and_vibrant_appearances_explicitly() {
        assert_eq!(window_appearance_override(ThemeMode::FollowSystem), None);
        assert_eq!(
            window_appearance_override(ThemeMode::Light),
            Some(WindowAppearance::Light)
        );
        assert_eq!(
            window_appearance_override(ThemeMode::Dark),
            Some(WindowAppearance::Dark)
        );
        assert_eq!(
            ThemeAppearance::from(WindowAppearance::VibrantLight),
            ThemeAppearance::Light
        );
        assert_eq!(
            ThemeAppearance::from(WindowAppearance::VibrantDark),
            ThemeAppearance::Dark
        );
    }
}
