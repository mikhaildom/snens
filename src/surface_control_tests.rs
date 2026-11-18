// Тесты снятия контроля хода: чужой ход не должен снимать контроль живого, а
// закрытый канал отмены не должен читаться как отмена.
//
// Предмет — гонка двух `launch` на одну сессию: первый ход заканчивается уже
// после того, как `launch` второго занял место. Раньше снятие было безусловным,
// и вместе с `watch::Sender` закрывался канал: новый ход видел `Err` в
// `changed()`, писал себе `cancelled` и умирал, а `message` перестал находить
// очередь входа.
use super::test_support::runtime;
use crate::surface_runtime::Control;

/// Чужой ход не снимает контроль: `run_id` не совпал — запись остаётся.
#[tokio::test]
async fn foreign_run_does_not_release_control() {
    let (s, _dir) = runtime().await;
    let (cancel, _rx) = tokio::sync::watch::channel(false);
    let (queued, _qrx) = tokio::sync::mpsc::unbounded_channel();
    s.controls.lock().await.insert(
        "a".into(),
        Control {
            run: "run-2".into(),
            cancel,
            queued,
        },
    );

    s.release_control("a", "run-1").await;
    assert!(
        s.controls.lock().await.contains_key("a"),
        "контроль живого хода снят чужим завершением"
    );

    s.release_control("a", "run-2").await;
    assert!(
        !s.controls.lock().await.contains_key("a"),
        "свой контроль не снят"
    );
}

/// Канал отмены остаётся открытым, пока контроль в карте: иначе живой ход
/// получил бы `Err` из `changed()` и считал себя отменённым.
#[tokio::test]
async fn control_keeps_cancel_channel_open_for_the_live_run() {
    let (s, _dir) = runtime().await;
    let (cancel, mut rx) = tokio::sync::watch::channel(false);
    let (queued, _qrx) = tokio::sync::mpsc::unbounded_channel();
    s.controls.lock().await.insert(
        "a".into(),
        Control {
            run: "run-1".into(),
            cancel,
            queued,
        },
    );
    s.release_control("a", "run-2").await;
    // Канал жив и не отменён: `has_changed` без значения не паникует, а
    // закрытие канала видно по `changed()`.
    assert!(!*rx.borrow_and_update(), "пришла ложная отмена");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), rx.changed())
            .await
            .is_err(),
        "канал закрылся, хотя ход жив"
    );
}
