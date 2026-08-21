use libneo::menu::{App, Menu, MenuBar};

const APPLICATION_NAME: &str = "NEO";

pub fn install(cx: &mut App) {
    MenuBar::new()
        .menus([
            Menu::application(APPLICATION_NAME),
            Menu::window(),
            Menu::help(),
        ])
        .install(cx);
}
