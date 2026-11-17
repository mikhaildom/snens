//! Иконка в системном трее и меню.
//!
//! Зачем трей. Ядро живёт столько же, сколько окно, и это правильно: у него
//! долгие ходы, подагенты и инструменты, которым нужен профиль человека. Но
//! тогда закрытое окно = убитая работа, а приложение без окна нечем ни открыть,
//! ни погасить. Трей решает обе стороны: крестик прячет окно, выход — явный
//! пункт меню.

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};

/// Пункты меню. Идентификаторы — строки, а не индексы: меню пересобирается, и
/// привязка к порядку ломалась бы при добавлении пункта.
const OPEN: &str = "open";
const RELOAD: &str = "reload";
const QUIT: &str = "quit";

pub fn install(app: &tauri::AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, OPEN, "Открыть окно", true, None::<&str>)?;
    let reload = MenuItem::with_id(app, RELOAD, "Перезагрузить интерфейс", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, QUIT, "Выход (остановит ядро)", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &reload, &quit])?;

    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("snens")
        .menu(&menu)
        // Левая кнопка показывает окно, меню — по правой: иначе привычный клик
        // «открыть» каждый раз выкатывал бы список.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            OPEN => show(app),
            RELOAD => {
                if let Some(w) = app.get_webview_window("main") {
                    // Интерфейс перезагружается, ядро — нет: адрес ядра не
                    // меняется, и подписки клиента переустанавливаются сами.
                    let _ = w.eval("location.reload()");
                }
            }
            QUIT => {
                // Гасит ядро `RunEvent::Exit` в main.rs — здесь только выход.
                // Свой `stop()` на UI-потоке морозил бы событийный цикл на
                // kill+wait (и был бы вторым вызовом того же).
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show(tray.app_handle());
            }
        });
    // Иконка окна, а не отдельный файл: она уже собрана из `icons/icon.png`, и
    // вторая её копия в бандле расходилась бы с первой при смене логотипа.
    // Если иконки в бандле нет (запуск не из установленного пакета на Linux),
    // берём встроенную: без иконки трей может не создаться вообще, а тогда
    // спрятанное крестиком окно нечем вернуть и нечем выйти.
    let icon = match app.default_window_icon() {
        Some(icon) => icon.clone(),
        // `include_image!` встраивает PNG в бинарь (фича `image-png` не нужна).
        // Без иконки трей на Linux может не создаться вовсе — а тогда
        // спрятанное крестиком окно нечем вернуть и нечем выйти.
        None => tauri::include_image!("icons/icon.png"),
    };
    builder = builder.icon(icon);
    // На macOS иконка в строке меню должна быть template (монохром), иначе
    // цветной логотип выпадает из оформления системы.
    #[cfg(target_os = "macos")]
    {
        builder = builder.icon_as_template(true);
    }
    builder.build(app)?;
    Ok(())
}

fn show(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}
